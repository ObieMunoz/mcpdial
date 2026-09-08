//! What is checked before anything is sent.
//!
//! A bad header, an unreadable timeout or a location that is neither a URL nor a
//! command is exit 2 and no connection at all - the argument was wrong, so there
//! was never a request to make. Reading a JSON argument belongs here for the same
//! reason: it is settled off the wire, and a malformed object costs no dial.

use crate::diagnose::json_kind;
use crate::failure::Failure;
use crate::present::{truncate_at, Presenter};
use mcpdial::{Error, ServerConfig};
use serde_json::{json, Value};
use std::io::{IsTerminal, Read};
use std::time::Duration;

/// A secret from `$VAR`, or from stdin. Never from an argument, where `ps` and the
/// shell history would both keep a copy.
pub(crate) fn read_secret(
    ui: &dyn Presenter,
    env: Option<&str>,
    what: &str,
) -> Result<String, Error> {
    if let Some(var) = env {
        return std::env::var(var)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::usage(format!("${var} is unset or empty")));
    }
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        ui.err(&format!("paste the {what} and press enter: "));
    }
    let mut buf = String::new();
    stdin
        .read_to_string(&mut buf)
        .map_err(|e| Error::usage(e.to_string()))?;
    let secret = buf.trim().to_string();
    if secret.is_empty() {
        return Err(Error::usage(format!("no {what} on stdin")));
    }
    Ok(secret)
}

/// A JSON object given inline, as `@path` to read a file, or `-` to read stdin.
pub(crate) fn read_json_arg(text: &str, what: &str) -> Result<Value, Error> {
    let owned;
    let text = if text == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| Error::usage(format!("reading {what} from stdin: {e}")))?;
        owned = buf;
        &owned
    } else if let Some(path) = text.strip_prefix('@') {
        owned = std::fs::read_to_string(path)
            .map_err(|e| Error::usage(format!("reading {what} from {path}: {e}")))?;
        &owned
    } else {
        text
    };
    parse_object(text, what)
}

/// A JSON object, or a usage error that quotes what arrived instead. Anything
/// unquoted from a shell (a URL, a bare word, a pasted markdown link) lands here.
pub(crate) fn parse_object(text: &str, what: &str) -> Result<Value, Error> {
    match serde_json::from_str::<Value>(text) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(v) => Err(Error::usage(format!(
            "{what} must be a JSON object like {{\"key\": \"value\"}}, not {}",
            json_kind(&v)
        ))),
        Err(e) => Err(Error::usage(match requote_object(text) {
            Some(fixed) => format!(
                "{what} must be a JSON object like {{\"key\": \"value\"}}; {:?} is one with its double quotes missing (did you mean {fixed}?)",
                truncate_at(text, 60)
            ),
            None => format!(
                "{what} must be a JSON object like {{\"key\": \"value\"}}; {:?} is not JSON ({e})",
                truncate_at(text, 60)
            ),
        })),
    }
}

/// `{url:https://x}` back to `{"url": "https://x"}`: the object a shell was most
/// likely handed before it removed the double quotes. Flat objects only, since
/// the quotes were the only thing telling a comma in a value from a separator.
pub(crate) fn requote_object(text: &str) -> Option<String> {
    let inner = text.trim().strip_prefix('{')?.strip_suffix('}')?;
    if inner.contains(['{', '[', '"']) {
        return None;
    }
    let fields = inner
        .split(',')
        .map(|pair| {
            let (key, value) = pair.split_once(':')?;
            let (key, value) = (key.trim(), value.trim());
            if key.is_empty() || value.is_empty() {
                return None;
            }
            let value = serde_json::from_str::<Value>(value)
                .unwrap_or_else(|_| Value::String(value.to_string()));
            Some(format!("{}: {value}", json!(key)))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(format!("{{{}}}", fields.join(", ")))
}

/// Where `add` was told a server lives, checked before anything is written: an
/// http(s) URL with a host, or a command line with at least one word. A mistake
/// here would otherwise surface on the next `ls`, as an unreachable server.
pub(crate) fn validate_location(cfg: &ServerConfig) -> Result<(), Error> {
    // A `${VAR}` is filled in when the server is dialed; what can be checked
    // now is the shape around it.
    let filled = cfg.expanded(|_| Some("var".to_string()))?;
    if let (Some(url), Some(given)) = (&filled.http, &cfg.http) {
        let has_scheme = url.starts_with("http://") || url.starts_with("https://");
        let has_host = url
            .parse::<ureq::http::Uri>()
            .is_ok_and(|u| u.host().is_some_and(|h| !h.is_empty()));
        if !has_scheme || !has_host {
            return Err(Error::usage(format!(
                "--http needs an http:// or https:// URL, got {given:?}"
            )));
        }
    }
    if let Some(cmd) = &filled.stdio {
        mcpdial::transport::stdio::split_command(cmd)?;
    }
    Ok(())
}

/// The `--timeout` that `add` saves: a number of seconds a wait can be bounded
/// by, so the file never holds one that dialing would refuse.
pub(crate) fn validate_timeout(secs: f64) -> Result<f64, Error> {
    if secs.is_finite() && secs >= 0.0 {
        Ok(secs)
    } else {
        Err(Error::usage(format!(
            "--timeout must be a non-negative number of seconds, got {secs}"
        )))
    }
}

/// `--allow` and `--deny` patterns as saved: trimmed, and none of them empty,
/// since an empty pattern matches nothing and would only puzzle a later reader.
pub(crate) fn validate_patterns(patterns: Vec<String>, flag: &str) -> Result<Vec<String>, Error> {
    patterns
        .into_iter()
        .map(|p| match p.trim() {
            "" => Err(Error::usage(format!(
                "{flag} needs a tool name or a glob like 'read_*', got \"\""
            ))),
            p => Ok(p.to_string()),
        })
        .collect()
}

pub(crate) fn parse_headers(items: &[String]) -> Result<Vec<(String, String)>, Error> {
    items
        .iter()
        .map(|item| match item.split_once(':') {
            Some((name, value)) if !name.trim().is_empty() => {
                Ok((name.trim().to_string(), value.trim().to_string()))
            }
            _ => Err(Error::usage(format!(
                "--header must look like 'Name: value', got {item:?}"
            ))),
        })
        .collect()
}

/// `--idle SECS` as a duration; zero or less would be a daemon that quits at once.
pub(crate) fn idle_duration(secs: Option<f64>) -> Result<Option<Duration>, Failure> {
    match secs {
        None => Ok(None),
        Some(s) if s > 0.0 => Ok(Some(Duration::from_secs_f64(s))),
        Some(_) => Err(Error::usage("--idle needs a positive number of seconds").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcpdial::ServerConfig;

    #[test]
    fn a_location_is_checked_before_it_is_saved() {
        assert!(validate_location(&ServerConfig::http("https://mcp.deepwiki.com/mcp")).is_ok());
        assert!(validate_location(&ServerConfig::http("http://127.0.0.1:8080")).is_ok());
        // A placeholder is filled in at dial time; the shape around it is what counts.
        assert!(validate_location(&ServerConfig::http("https://${HOST}/mcp")).is_ok());
        for bad in [
            "notaurl",
            "ftp://x/mcp",
            "http://",
            "https://a b/mcp",
            "mcp.example.com/mcp",
        ] {
            let e = validate_location(&ServerConfig::http(bad)).unwrap_err();
            assert!(matches!(e, Error::Usage(_)), "{bad}: {e}");
            assert!(e.to_string().contains(bad), "{bad}: {e}");
        }
        assert!(validate_location(&ServerConfig::stdio("npx -y thing /tmp")).is_ok());
        assert!(validate_location(&ServerConfig::stdio("   ")).is_err());
        assert!(validate_location(&ServerConfig::stdio("'unterminated")).is_err());
    }

    #[test]
    fn a_timeout_is_checked_before_it_is_saved() {
        assert_eq!(validate_timeout(0.2).unwrap(), 0.2);
        assert_eq!(validate_timeout(0.0).unwrap(), 0.0);
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            let e = validate_timeout(bad).unwrap_err();
            assert!(matches!(e, Error::Usage(_)), "{bad}: {e}");
        }
    }
}
