//! stdio: spawn a process and speak newline-delimited JSON-RPC over its pipes.
//!
//! This works with all network egress blocked. Most MCP servers are stdio-only,
//! so for them "the connector is blocked" is close to meaningless: the server is
//! a local program, and you run local programs.

use super::{silent, Logger, Transport};
use crate::protocol::{Error, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

pub struct StdioTransport {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    timeout: Duration,
    log: Logger,
}

impl StdioTransport {
    /// Spawn `argv[0]` with the remaining arguments.
    pub fn spawn(argv: &[String], timeout: Duration, forward_stderr: bool, log: Option<Logger>) -> Result<Self> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| Error::usage("--stdio needs a command to run"))?;

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if forward_stderr { Stdio::inherit() } else { Stdio::null() })
            .spawn()
            .map_err(|e| Error::transport(format!("could not start {program:?}: {e}")))?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");

        // Read stdout on a thread so a hung server honours the timeout instead of
        // blocking forever on read_line. The channel closes when the server exits.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        Ok(Self { child, stdin, lines: rx, timeout, log: log.unwrap_or_else(silent) })
    }

    /// Spawn from a single shell-style string such as `"npx -y some-server /tmp"`.
    pub fn spawn_str(command: &str, timeout: Duration, forward_stderr: bool, log: Option<Logger>) -> Result<Self> {
        let argv = split_command(command)?;
        Self::spawn(&argv, timeout, forward_stderr, log)
    }
}

impl Transport for StdioTransport {
    fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
        let body = payload.to_string();
        (self.log)(&format!("-> stdin\n   {body}"));

        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Error::transport("stdin already closed"))?;
        writeln!(stdin, "{body}")
            .and_then(|_| stdin.flush())
            .map_err(|_| Error::transport("server closed stdin before accepting the request"))?;

        let Some(id) = payload.get("id") else {
            return Ok(None); // notification: nothing comes back
        };

        // Skip log lines and unrelated notifications until our id lands.
        loop {
            let line = match self.lines.recv_timeout(self.timeout) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::transport(format!(
                        "no reply after {}s",
                        self.timeout.as_secs_f64()
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::transport("server closed stdout before replying"))
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(msg) if msg.get("id") == Some(id) => {
                    (self.log)(&format!("<- stdout\n   {line}"));
                    return Ok(Some(msg));
                }
                Ok(_) => (self.log)(&format!("<- stdout (other message)\n   {line}")),
                Err(_) => (self.log)(&format!("<- stdout (ignored, not JSON)\n   {line}")),
            }
        }
    }

    fn close(&mut self) {
        drop(self.stdin.take());
        // Give a well-behaved server a moment to exit on EOF, then insist.
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Minimal POSIX-style word splitting: whitespace separates, single and double
/// quotes group, backslash escapes the next character outside single quotes.
pub fn split_command(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err(Error::usage("unterminated single quote in --stdio")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(e @ ('"' | '\\' | '$' | '`')) => cur.push(e),
                            Some(e) => {
                                cur.push('\\');
                                cur.push(e)
                            }
                            None => return Err(Error::usage("unterminated double quote in --stdio")),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err(Error::usage("unterminated double quote in --stdio")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(e) = chars.next() {
                    cur.push(e);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    if out.is_empty() {
        return Err(Error::usage("--stdio needs a command to run"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::split_command;

    #[test]
    fn splits_like_a_shell() {
        assert_eq!(split_command("a b  c").unwrap(), ["a", "b", "c"]);
        assert_eq!(split_command("npx -y pkg '/tmp/my dir'").unwrap(), ["npx", "-y", "pkg", "/tmp/my dir"]);
        assert_eq!(split_command(r#"x "say \"hi\"" y"#).unwrap(), ["x", "say \"hi\"", "y"]);
        assert_eq!(split_command(r"a\ b").unwrap(), ["a b"]);
        assert_eq!(split_command("''").unwrap(), [""]);
    }

    #[test]
    fn rejects_empty_and_unterminated() {
        assert!(split_command("   ").is_err());
        assert!(split_command("a 'b").is_err());
        assert!(split_command("a \"b").is_err());
    }
}
