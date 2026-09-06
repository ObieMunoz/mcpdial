//! One stdio server kept alive across invocations. `mcpdial start NAME` runs a
//! background process that owns the server's session and listens on
//! `run/NAME.sock` under the config directory; every command that would dial
//! NAME relays over that socket instead, so successive calls share one process
//! and the state it holds.
//!
//! The wire format is the stdio framing itself, one JSON-RPC message per line
//! each way, so the foreground side is [`Framed`] with a socket in place of the
//! child's pipes. The daemon answers the handshake it opened with (`initialize`
//! or `server/discover`) from the result it already holds, swallows
//! `notifications/initialized`, forwards everything else to the server, so the
//! other handshake meets the server's own refusal, and hands a request the
//! server makes mid-call to the foreground process, which is the one that can
//! act on it. Callers are served one at a time: a second one waits at the
//! socket until the first hangs up.
//!
//! Only Unix domain sockets are implemented; on other platforms `start` and
//! `stop` say so and every other command dials as before.

use crate::config::Store;
#[cfg(not(unix))]
use crate::protocol::{Error, Result};
use std::path::PathBuf;

pub const ENV_NO_DAEMON: &str = "MCPDIAL_NO_DAEMON";

/// The control request that ends the daemon: the one method never forwarded.
pub const STOP_METHOD: &str = "mcpdial/stop";

/// The JSON-RPC error code a relayed transport failure arrives under: the
/// server's timeout or death, reported to the caller as a reply to its request.
pub const RELAY_ERROR: i64 = -32000;

/// The socket a daemon for `name` listens on, whether or not one is running.
pub fn socket_path(store: &Store, name: &str) -> PathBuf {
    store.run_dir().join(format!("{name}.sock"))
}

/// `$MCPDIAL_NO_DAEMON` set to anything but empty or `0`.
pub fn disabled_by_env() -> bool {
    std::env::var(ENV_NO_DAEMON).is_ok_and(|v| !v.is_empty() && v != "0")
}

/// What the socket file says about the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// A process is listening.
    Running,
    /// The file is there but nothing answers: the daemon was killed.
    Stale,
    Absent,
}

pub fn is_running(store: &Store, name: &str) -> bool {
    state(store, name) == State::Running
}

#[cfg(not(unix))]
fn not_supported() -> Error {
    Error::usage("start and stop are not supported on this platform yet; every command dials")
}

#[cfg(unix)]
pub use unix::{attach, serve, start, state, stop, SocketTransport};

#[cfg(not(unix))]
pub fn state(_store: &Store, _name: &str) -> State {
    State::Absent
}

#[cfg(not(unix))]
pub fn start(
    _store: &Store,
    _name: &str,
    _idle: Option<std::time::Duration>,
    _opts: &crate::client::Options,
) -> Result<u32> {
    Err(not_supported())
}

#[cfg(not(unix))]
pub fn stop(_store: &Store, _name: &str, _opts: &crate::client::Options) -> Result<()> {
    Err(not_supported())
}

#[cfg(not(unix))]
pub fn serve(
    _store: &Store,
    _name: &str,
    _opts: &crate::client::Options,
    _idle: Option<std::time::Duration>,
) -> Result<()> {
    Err(not_supported())
}

#[cfg(unix)]
mod unix {
    use super::{socket_path, State, RELAY_ERROR, STOP_METHOD};
    use crate::client::{self, Options};
    use crate::config::Store;
    use crate::protocol::{check, classify, reply, reply_error, request, Error, Incoming, Result};
    use crate::transport::stdio::{Framed, StdioTransport};
    use crate::transport::{silent, Logger, Transport};
    use serde_json::{json, Value};
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread;
    use std::time::{Duration, Instant};

    /// Only a listener refuses nothing: a socket file whose process is gone
    /// refuses the connection, and one that was never there cannot be opened.
    pub fn state(store: &Store, name: &str) -> State {
        let path = socket_path(store, name);
        if !path.exists() {
            return State::Absent;
        }
        match UnixStream::connect(&path) {
            Ok(_) => State::Running,
            Err(_) => State::Stale,
        }
    }

    // -- the foreground side -------------------------------------------------

    /// The socket to a running daemon, ready for `initialize`, or `None` when
    /// there is nothing to attach to. A stale socket is removed on the way,
    /// with a note, so the caller dials as if it had never been there.
    pub fn attach(
        store: &Store,
        name: &str,
        timeout: Duration,
        log: Option<Logger>,
    ) -> Result<Option<SocketTransport>> {
        let path = socket_path(store, name);
        if !path.exists() {
            return Ok(None);
        }
        match UnixStream::connect(&path) {
            Ok(stream) => Ok(Some(SocketTransport::new(name, stream, timeout, log)?)),
            Err(_) => {
                let _ = fs::remove_file(&path);
                eprintln!(
                    "note: {name} is not running; its stale socket was removed. Dialing instead."
                );
                Ok(None)
            }
        }
    }

    /// A session relayed through the daemon: the stdio framing on a socket.
    pub struct SocketTransport {
        name: String,
        stream: UnixStream,
        framed: Framed,
    }

    impl SocketTransport {
        fn new(
            name: &str,
            stream: UnixStream,
            timeout: Duration,
            log: Option<Logger>,
        ) -> Result<Self> {
            let clone = |what: &str| {
                stream.try_clone().map_err(|e| {
                    Error::transport(format!("cannot open the socket for {what}: {e}"))
                })
            };
            let framed = Framed::new(
                clone("writing")?,
                clone("reading")?,
                ("socket", "socket"),
                timeout,
                log.unwrap_or_else(silent),
            );
            Ok(Self {
                name: name.to_string(),
                stream,
                framed,
            })
        }
    }

    impl Transport for SocketTransport {
        fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
            let Self { name, framed, .. } = self;
            framed.exchange(
                payload,
                &mut || Error::transport(format!("the daemon for {name} closed the connection")),
                &mut |_| None,
            )
        }

        fn close(&mut self) {
            self.framed.close_writer();
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    // -- start and stop ------------------------------------------------------

    /// Spawn the daemon for `name` and wait until it is listening. The daemon is
    /// this binary again, running the hidden `daemon` subcommand in its own
    /// process group with nothing but a pipe back to us, on which it reports
    /// either its readiness or why it could not start.
    pub fn start(store: &Store, name: &str, idle: Option<Duration>, opts: &Options) -> Result<u32> {
        let r = client::resolve(store, name)?;
        if !r.saved {
            return Err(Error::usage(
                "start needs a saved server name; add the server first",
            ));
        }
        if r.config.stdio.is_none() {
            return Err(Error::usage(
                "start only applies to stdio servers; an HTTP server has no process to keep alive",
            ));
        }
        if state(store, name) == State::Running {
            return Err(Error::usage(format!("{name} is already running")));
        }
        let exe = std::env::current_exe()
            .map_err(|e| Error::config(format!("cannot locate the mcpdial binary: {e}")))?;
        let mut cmd = Command::new(exe);
        // Only the flags given here travel; the daemon reads the server's own
        // saved timeout and version for itself, as any dial would.
        if let Some(timeout) = opts.timeout {
            cmd.arg("--timeout").arg(timeout.as_secs_f64().to_string());
        }
        if let Some(version) = opts.protocol_version {
            cmd.arg("--protocol-version").arg(version.as_str());
        }
        cmd.arg("daemon").arg(name);
        if let Some(idle) = idle {
            cmd.arg("--idle").arg(idle.as_secs_f64().to_string());
        }
        // Its own process group, so ^C at this terminal does not reach it.
        cmd.process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::transport(format!("could not start the daemon: {e}")))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let report: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
        if report.get("pid").is_some() {
            return Ok(child.id());
        }
        if let Some(err) = report.get("error") {
            let message = err["message"]
                .as_str()
                .unwrap_or("the daemon could not start");
            return Err(match err["kind"].as_str() {
                Some("usage") => Error::usage(message),
                Some("config") => Error::config(message),
                _ => Error::transport(message),
            });
        }
        let status = child
            .wait()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "unknown status".into());
        Err(Error::transport(format!(
            "the daemon exited ({status}) before it was ready"
        )))
    }

    /// Ask the daemon to exit, and wait until its socket is gone.
    pub fn stop(store: &Store, name: &str, opts: &Options) -> Result<()> {
        let timeout = opts.timeout_for(&client::resolve(store, name)?)?;
        let path = socket_path(store, name);
        let stream = match state(store, name) {
            State::Absent => return Err(Error::usage(format!("{name} is not running"))),
            State::Stale => {
                let _ = fs::remove_file(&path);
                return Err(Error::usage(format!(
                    "{name} is not running; its stale socket was removed"
                )));
            }
            State::Running => UnixStream::connect(&path)
                .map_err(|e| Error::transport(format!("cannot connect to {name}: {e}")))?,
        };
        let mut t = SocketTransport::new(name, stream, timeout, None)?;
        check(t.send(&request(STOP_METHOD, 1, None))?)?;
        let deadline = Instant::now() + timeout;
        while path.exists() {
            if Instant::now() >= deadline {
                return Err(Error::transport(format!(
                    "{name} acknowledged the stop but its socket is still there after {}s",
                    timeout.as_secs_f64()
                )));
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    // -- the daemon ----------------------------------------------------------

    /// The daemon body: dial the server once, listen, and relay until told to
    /// stop, until `idle` passes with nobody connected, or until the server
    /// dies. Prints one JSON line on stdout once the socket is open, which is
    /// what [`start`] waits for; an error before that point is the caller's to
    /// print.
    pub fn serve(store: &Store, name: &str, opts: &Options, idle: Option<Duration>) -> Result<()> {
        let r = client::resolve(store, name)?;
        if !r.saved || r.config.stdio.is_none() {
            return Err(Error::usage("the daemon serves saved stdio servers only"));
        }
        let quiet = Options {
            verbose: false,
            no_daemon: true,
            ..opts.clone()
        };
        let mut session = client::open_stdio(&r, &quiet)?;
        let held = Held {
            method: session.version().handshake().method(),
            result: session.server_info.clone(),
        };
        let path = socket_path(store, name);
        let listener = bind(store, name, &path)?;
        println!("{}", json!({ "pid": std::process::id() }));
        let _ = std::io::stdout().flush();

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming().map_while(std::result::Result::ok) {
                if tx.send(stream).is_err() {
                    break;
                }
            }
        });
        loop {
            let stream = match idle {
                Some(idle) => match rx.recv_timeout(idle) {
                    Ok(s) => s,
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(s) => s,
                    Err(_) => break,
                },
            };
            match serve_one(stream, &mut session.transport, &held) {
                Next::Caller => {}
                Next::Stop | Next::ServerGone => break,
            }
        }
        session.close();
        let _ = fs::remove_file(&path);
        Ok(())
    }

    /// The socket, in a directory only its owner can enter. A stale socket from
    /// a daemon that was killed is replaced; a live one is left alone.
    fn bind(store: &Store, name: &str, path: &Path) -> Result<UnixListener> {
        let dir = store.run_dir();
        create_private_dir(&dir)
            .map_err(|e| Error::config(format!("cannot create {}: {e}", dir.display())))?;
        match state(store, name) {
            State::Running => return Err(Error::usage(format!("{name} is already running"))),
            State::Stale => {
                let _ = fs::remove_file(path);
            }
            State::Absent => {}
        }
        UnixListener::bind(path)
            .map_err(|e| Error::transport(format!("cannot listen on {}: {e}", path.display())))
    }

    fn create_private_dir(dir: &Path) -> std::io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }

    enum Next {
        /// This caller hung up; wait for the next.
        Caller,
        Stop,
        ServerGone,
    }

    /// The handshake the daemon's own session opened with, and what it
    /// answered: replayed to each caller that asks the same way.
    struct Held {
        method: &'static str,
        result: Value,
    }

    /// One caller, start to finish. Its handshake is answered from the result
    /// already in hand, so the server sees one however many callers come and
    /// go; everything else it sends goes to the server, and whatever the server
    /// asks mid-request goes back to it.
    fn serve_one(stream: UnixStream, server: &mut StdioTransport, held: &Held) -> Next {
        let Ok(read_half) = stream.try_clone() else {
            return Next::Caller;
        };
        let mut reader = BufReader::new(read_half);
        let mut writer = stream;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return Next::Caller,
                Ok(_) => {}
            }
            let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let id = msg.get("id").filter(|id| !id.is_null());
            match (msg["method"].as_str(), id) {
                (Some(method), Some(id)) if method == held.method => {
                    if send_line(&mut writer, &reply(id, held.result.clone())).is_err() {
                        return Next::Caller;
                    }
                }
                (Some("notifications/initialized"), None) => {}
                (Some(STOP_METHOD), Some(id)) => {
                    let _ = send_line(&mut writer, &reply(id, json!({})));
                    return Next::Stop;
                }
                _ => {
                    let outcome = server.exchange(&msg, &mut |from_server| {
                        if send_line(&mut writer, from_server).is_err() {
                            return None;
                        }
                        match classify(from_server, None) {
                            Incoming::ServerRequest { id, .. } => await_answer(&mut reader, id),
                            _ => None,
                        }
                    });
                    match outcome {
                        Ok(Some(answer)) => {
                            if send_line(&mut writer, &answer).is_err() {
                                return Next::Caller;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            if let Some(id) = id {
                                let _ = send_line(
                                    &mut writer,
                                    &reply_error(id, RELAY_ERROR, &e.to_string()),
                                );
                            }
                            if !server.is_alive() {
                                return Next::ServerGone;
                            }
                        }
                    }
                }
            }
        }
    }

    /// How long to give the caller to answer something the server asked. It
    /// answers at once when it is there at all, so this only bounds a caller
    /// that vanished without closing its socket.
    const CALLER_PATIENCE: Duration = Duration::from_secs(30);

    /// The caller's reply to a request the server made, read off the socket
    /// while the server waits. `None` when the caller is gone or silent, and
    /// the default answer is owed instead.
    fn await_answer(reader: &mut BufReader<UnixStream>, id: &Value) -> Option<Value> {
        let _ = reader.get_ref().set_read_timeout(Some(CALLER_PATIENCE));
        let mut line = String::new();
        let found = loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break None,
                Ok(_) => {}
            }
            match serde_json::from_str::<Value>(line.trim()) {
                Ok(msg) if matches!(classify(&msg, Some(id)), Incoming::Response) => {
                    break Some(msg)
                }
                _ => continue,
            }
        };
        let _ = reader.get_ref().set_read_timeout(None);
        found
    }

    fn send_line(writer: &mut UnixStream, msg: &Value) -> std::io::Result<()> {
        writeln!(writer, "{msg}")?;
        writer.flush()
    }
}
