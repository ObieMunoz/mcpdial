//! What the environment says when a global flag is left off.
//!
//! A rule an agent has to remember on every invocation is one it forgets
//! mid-task, and the first forgotten `--json` is a table it then has to parse.
//! `MCPDIAL_JSON=1` says it once, the way `MCPDIAL_PLAIN` already does for
//! `--plain`. A flag on the command line still wins, and a variable left unset
//! changes nothing at all.

use crate::Cli;
use mcpdial::{Error, Result};

/// Set to anything but `0` for `--json` on every command.
pub const ENV_JSON: &str = "MCPDIAL_JSON";

/// Seconds to wait for a reply, as `--timeout` takes them.
pub const ENV_TIMEOUT: &str = "MCPDIAL_TIMEOUT";

/// A User-Agent, as `--user-agent` takes one.
pub const ENV_USER_AGENT: &str = "MCPDIAL_USER_AGENT";

/// Every global flag the environment may stand in for, filled in from it.
pub fn apply(cli: &mut Cli) -> Result<()> {
    fill(cli, |name| std::env::var(name).ok())
}

/// [`apply`] against any lookup: the real environment in the binary, a table in
/// a test.
fn fill(cli: &mut Cli, lookup: impl Fn(&str) -> Option<String>) -> Result<()> {
    let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
    cli.json = cli.json || set(ENV_JSON).is_some_and(|value| value != "0");
    if cli.user_agent.is_none() {
        cli.user_agent = set(ENV_USER_AGENT);
    }
    // After `json`, so that a malformed timeout is still reported in the shape
    // the environment asked for.
    if cli.timeout.is_none() {
        cli.timeout = set(ENV_TIMEOUT).map(|value| seconds(&value)).transpose()?;
    }
    Ok(())
}

/// The variable's value as `--timeout` would have taken it. A typo is refused
/// rather than ignored: silently falling back to the default is how a harness
/// that meant to allow five minutes waits sixty seconds and never learns why.
fn seconds(raw: &str) -> Result<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .ok_or_else(|| {
            Error::usage(format!(
                "{ENV_TIMEOUT} must be a non-negative number of seconds, got {raw:?}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn resolved(args: &[&str], env: &[(&str, &str)]) -> Result<Cli> {
        let mut cli = Cli::parse_from(std::iter::once("mcpdial").chain(args.iter().copied()));
        let env = env.to_vec();
        fill(&mut cli, |name| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })?;
        Ok(cli)
    }

    fn cli(args: &[&str], env: &[(&str, &str)]) -> Cli {
        resolved(args, env).unwrap()
    }

    #[test]
    fn a_flag_beats_the_variable_which_beats_the_built_in_default() {
        assert!(!cli(&["ls"], &[]).json);
        assert!(cli(&["ls"], &[(ENV_JSON, "1")]).json);
        assert!(cli(&["--json", "ls"], &[]).json);
        assert!(cli(&["--json", "ls"], &[(ENV_JSON, "0")]).json);

        assert_eq!(cli(&["ls"], &[]).timeout, None);
        assert_eq!(cli(&["ls"], &[(ENV_TIMEOUT, "0.5")]).timeout, Some(0.5));
        assert_eq!(
            cli(&["--timeout", "9", "ls"], &[(ENV_TIMEOUT, "0.5")]).timeout,
            Some(9.0)
        );

        assert_eq!(cli(&["ls"], &[]).user_agent, None);
        assert_eq!(
            cli(&["ls"], &[(ENV_USER_AGENT, "probe/1")])
                .user_agent
                .as_deref(),
            Some("probe/1")
        );
        assert_eq!(
            cli(
                &["--user-agent", "flag/1", "ls"],
                &[(ENV_USER_AGENT, "env/1")]
            )
            .user_agent
            .as_deref(),
            Some("flag/1")
        );
    }

    #[test]
    fn a_variable_that_says_nothing_leaves_every_default_alone() {
        let untouched = cli(
            &["ls"],
            &[(ENV_JSON, ""), (ENV_TIMEOUT, ""), (ENV_USER_AGENT, "")],
        );
        assert!(!untouched.json);
        assert_eq!(untouched.timeout, None);
        assert_eq!(untouched.user_agent, None);
        // `0` is the off switch MCPDIAL_PLAIN already answers to.
        assert!(!cli(&["ls"], &[(ENV_JSON, "0")]).json);
    }

    #[test]
    fn a_timeout_that_is_not_seconds_is_refused_by_name() {
        for bad in ["soon", "30s", "-1", "nan", "inf", ""] {
            let Err(e) = seconds(bad) else {
                panic!("{bad:?} was accepted as a timeout");
            };
            assert!(matches!(e, Error::Usage(_)), "{bad:?}: {e}");
            assert!(e.to_string().starts_with(ENV_TIMEOUT), "{bad:?}: {e}");
        }
        assert_eq!(seconds("0").unwrap(), 0.0);
        assert_eq!(seconds("2.5").unwrap(), 2.5);
    }

    #[test]
    fn a_malformed_timeout_still_arrives_after_the_json_it_will_be_printed_as() {
        let mut cli = Cli::parse_from(["mcpdial", "ls"]);
        let e = fill(&mut cli, |name| {
            (name == ENV_JSON)
                .then(|| "1".to_string())
                .or_else(|| (name == ENV_TIMEOUT).then(|| "soon".to_string()))
        })
        .unwrap_err();
        assert!(matches!(e, Error::Usage(_)), "{e}");
        assert!(
            cli.json,
            "the error is reported as JSON, as MCPDIAL_JSON asked"
        );
    }
}
