//! stdio: spawn a process and speak newline-delimited JSON-RPC over its pipes.
//!
//! This works with all network egress blocked. Most MCP servers are stdio-only,
//! so for them "the connector is blocked" is close to meaningless: the server is
//! a local program, and you run local programs.

use super::{silent, Logger, Transport};
use crate::protocol::{answer, classify, Error, Incoming, Result};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How many trailing stderr lines to keep for the post-mortem.
const STDERR_TAIL: usize = 12;

pub struct StdioTransport {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// The server's most recent stderr lines, shown when it dies without replying.
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    timeout: Duration,
    log: Logger,
}

impl StdioTransport {
    /// Spawn `argv[0]` with the remaining arguments.
    pub fn spawn(
        argv: &[String],
        timeout: Duration,
        forward_stderr: bool,
        log: Option<Logger>,
    ) -> Result<Self> {
        Self::spawn_with(argv, &[], None, timeout, forward_stderr, log)
    }

    /// Spawn with extra environment variables and an optional working directory.
    pub fn spawn_with(
        argv: &[String],
        env: &[(String, String)],
        cwd: Option<&Path>,
        timeout: Duration,
        forward_stderr: bool,
        log: Option<Logger>,
    ) -> Result<Self> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| Error::usage("--stdio needs a command to run"))?;

        let mut cmd = Command::new(program);
        cmd.args(args).envs(env.iter().map(|(k, v)| (k, v)));
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::transport(format!("could not start {program:?}: {e}")))?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        // Always read stderr, so a server that dies on startup (a typo in an npm
        // package name, a missing binary, a bad flag) can explain itself.
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
        {
            let tail = stderr_tail.clone();
            thread::spawn(move || {
                for line in BufReader::new(stderr)
                    .lines()
                    .map_while(std::result::Result::ok)
                {
                    if forward_stderr {
                        eprintln!("{line}");
                    }
                    let mut t = tail.lock().unwrap();
                    if t.len() == STDERR_TAIL {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            });
        }

        // Read stdout on a thread so a hung server honours the timeout instead of
        // blocking forever on read_line. The channel closes when the server exits.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            lines: rx,
            stderr_tail,
            timeout,
            log: log.unwrap_or_else(silent),
        })
    }

    /// Spawn from a single shell-style string such as `"npx -y some-server /tmp"`.
    pub fn spawn_str(
        command: &str,
        timeout: Duration,
        forward_stderr: bool,
        log: Option<Logger>,
    ) -> Result<Self> {
        let argv = split_command(command)?;
        Self::spawn(&argv, timeout, forward_stderr, log)
    }
}

impl StdioTransport {
    /// Put one JSON-RPC message on the child's stdin, newline-framed.
    fn write_line(&mut self, body: &str) -> Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(Error::transport("stdin already closed"));
        };
        if writeln!(stdin, "{body}")
            .and_then(|_| stdin.flush())
            .is_ok()
        {
            return Ok(());
        }
        // A server that died on startup gets the write refused rather than the read,
        // and its exit status and stderr say far more than the broken pipe does.
        Err(self.post_mortem())
    }

    /// The server went away before answering. Say how it exited and what it said.
    fn post_mortem(&mut self) -> Error {
        // Give the stderr reader a moment to drain what the process wrote on its way out.
        let status = match self.child.wait_timeout_polling(Duration::from_millis(500)) {
            Some(st) => match st.code() {
                Some(c) => format!("exited with status {c}"),
                None => "was killed by a signal".to_string(),
            },
            None => "closed stdout but is still running".to_string(),
        };
        thread::sleep(Duration::from_millis(50));
        let tail = self.stderr_tail.lock().unwrap();
        let mut msg = format!("server {status} before replying");
        if tail.is_empty() {
            msg.push_str(" (it wrote nothing to stderr)");
        } else {
            msg.push_str(". Its last stderr lines:");
            for line in tail.iter() {
                msg.push_str("\n  | ");
                msg.push_str(line);
            }
        }
        Error::transport(msg)
    }
}

trait WaitTimeout {
    fn wait_timeout_polling(&mut self, dur: Duration) -> Option<std::process::ExitStatus>;
}

impl WaitTimeout for Child {
    fn wait_timeout_polling(&mut self, dur: Duration) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + dur;
        loop {
            if let Ok(Some(st)) = self.try_wait() {
                return Some(st);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Transport for StdioTransport {
    fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
        let body = payload.to_string();
        (self.log)(&format!("-> stdin\n   {body}"));
        self.write_line(&body)?;

        let Some(id) = payload.get("id") else {
            return Ok(None); // notification: nothing comes back
        };

        // Skip log lines and unrelated notifications until our id lands - but
        // answer anything the server asks on the way. It may well be blocked on
        // that answer, in which case waiting quietly deadlocks both ends.
        loop {
            let line = match self.lines.recv_timeout(self.timeout) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::transport(format!(
                        "no reply after {}s",
                        self.timeout.as_secs_f64()
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => return Err(self.post_mortem()),
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(line) else {
                (self.log)(&format!("<- stdout (ignored, not JSON)\n   {line}"));
                continue;
            };
            match classify(&msg, Some(id)) {
                Incoming::Response => {
                    (self.log)(&format!("<- stdout\n   {line}"));
                    return Ok(Some(msg));
                }
                Incoming::ServerRequest { id: theirs, method } => {
                    (self.log)(&format!("<- stdout (server request)\n   {line}"));
                    let reply = answer(theirs, method).to_string();
                    (self.log)(&format!("-> stdin\n   {reply}"));
                    self.write_line(&reply)?;
                }
                Incoming::Notification { .. } | Incoming::Foreign => {
                    (self.log)(&format!("<- stdout (other message)\n   {line}"))
                }
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
                            None => {
                                return Err(Error::usage("unterminated double quote in --stdio"))
                            }
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
        assert_eq!(
            split_command("npx -y pkg '/tmp/my dir'").unwrap(),
            ["npx", "-y", "pkg", "/tmp/my dir"]
        );
        assert_eq!(
            split_command(r#"x "say \"hi\"" y"#).unwrap(),
            ["x", "say \"hi\"", "y"]
        );
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
