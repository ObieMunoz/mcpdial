//! A command that did not work, and the exit code it leaves behind.
//!
//! Three codes and no more: the server said no, the arguments were wrong, or a
//! snapshot no longer matches. A [`Failure`] carries the error with the one hint
//! worth printing under it, so that where a command gives up is also where it
//! says what to try instead.

use crate::present::Presenter;
use mcpdial::{Error, ServerConfig};
use serde_json::{json, Value};

pub(crate) const EXIT_ERROR: u8 = 1; // the server said no: JSON-RPC error, HTTP error, or tool isError
pub(crate) const EXIT_USAGE: u8 = 2; // bad arguments or config; nothing was sent
pub(crate) const EXIT_DRIFT: u8 = 3; // --check: the server no longer matches the snapshot

/// A command that failed, plus an optional hint that spells out what was
/// expected instead. The hint is a second block of prose for a human and an
/// `error.hint` string under `--json`, so neither has to guess a tool's shape.
pub(crate) struct Failure {
    pub(crate) error: Error,
    pub(crate) hint: Option<String>,
    /// The tool the error is about, as `error.tool` under `--json`.
    pub(crate) tool: Option<String>,
}

impl Failure {
    pub(crate) fn hinted(error: Error, hint: impl Into<String>) -> Self {
        Self {
            error,
            hint: Some(hint.into()),
            tool: None,
        }
    }

    /// What the process exits with: a request that was never going to work is
    /// told apart from one that failed on its way out.
    pub(crate) fn exit_code(&self) -> u8 {
        match self.error {
            Error::Usage(_) | Error::Config(_) => EXIT_USAGE,
            _ => EXIT_ERROR,
        }
    }

    pub(crate) fn report(&self, ui: &dyn Presenter) {
        ui.error(&self.error.to_string(), self.hint.as_deref());
    }

    pub(crate) fn to_json(&self) -> Value {
        let mut v = error_json(&self.error);
        if let Some(hint) = &self.hint {
            v["error"]["hint"] = json!(hint);
        }
        if let Some(tool) = &self.tool {
            v["error"]["tool"] = json!(tool);
        }
        v
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self {
            error,
            hint: None,
            tool: None,
        }
    }
}

/// The refusal a saved server's allow and deny lists make before anything is
/// sent for `tool`; `Ok` when they permit it.
pub(crate) fn refuse_denied(cfg: &ServerConfig, name: &str, tool: &str) -> Result<(), Failure> {
    cfg.refuse_denied(name, tool).map_err(|error| Failure {
        error,
        hint: None,
        tool: Some(tool.to_string()),
    })
}

/// One JSON object per error, so a program can branch on `kind` without parsing prose.
pub(crate) fn error_json(e: &Error) -> Value {
    let mut v = json!({ "message": e.to_string() });
    match e {
        Error::Rpc { code, data, .. } => {
            v["kind"] = json!("rpc");
            v["code"] = json!(code);
            if let Some(d) = data {
                v["data"] = d.clone();
            }
        }
        Error::Http {
            status,
            www_authenticate,
            ..
        } => {
            v["kind"] = json!("http");
            v["status"] = json!(status);
            if let Some(w) = www_authenticate {
                v["www_authenticate"] = json!(w);
            }
        }
        Error::Transport(_) => v["kind"] = json!("transport"),
        Error::Auth(_) => v["kind"] = json!("auth"),
        Error::Config(_) => v["kind"] = json!("config"),
        Error::Usage(_) => v["kind"] = json!("usage"),
    }
    json!({ "error": v })
}
