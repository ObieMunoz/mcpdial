//! Running the keychain tool the platform ships, and reporting what it said.

use crate::protocol::{Error, Result};
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// Run `tool` with `args`, writing `stdin` to it if there is any.
///
/// The secret goes down the pipe rather than into `args`, which any process on the
/// machine can read out of `ps`. Both streams are captured: the caller decides what
/// may be shown, and for a lookup stdout is the token itself.
pub fn run(tool: &str, args: &[&str], stdin: Option<&str>) -> Result<Output> {
    let mut child = Command::new(tool)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::config(format!(
                "the keychain needs {tool}, which is not on PATH; \
                 install it or keep credentials in the file store"
            )),
            _ => Error::config(format!("cannot run {tool}: {e}")),
        })?;
    if let Some(text) = stdin {
        let mut pipe = child.stdin.take().expect("stdin was piped");
        // A tool that has already given up closes the pipe, and writing to a
        // closed pipe is that refusal, not a failure of its own: let `wait`
        // report what it actually said.
        let _ = pipe.write_all(text.as_bytes());
        drop(pipe);
    }
    child
        .wait_with_output()
        .map_err(|e| Error::config(format!("cannot run {tool}: {e}")))
}

/// What a failing tool is reported as. `stderr` is its own diagnostic; stdout is
/// never quoted, because for a lookup that is the token.
pub fn failed(tool: &str, what: &str, out: &Output) -> Error {
    let detail = String::from_utf8_lossy(&out.stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        Error::config(format!("{tool} could not {what} ({})", out.status))
    } else {
        Error::config(format!("{tool} could not {what}: {detail}"))
    }
}
