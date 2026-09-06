//! A bare `mcpdial` at a terminal, and the half that matters more: a bare
//! `mcpdial` anywhere else.
//!
//! The picker waits for a person, so every test here is on a wall clock. A
//! pseudo-terminal with nobody typing is still a terminal, and anything that
//! stops to ask a question there never finishes; a test without a clock would
//! not fail, it would hang the suite. So each run below is given [`BOUND`] to
//! finish inside, and a run that is still going when the clock runs out is
//! killed and fails with whatever it had printed by then.
//!
//! The runs that must never ask anything are given an open pipe on stdin that
//! nothing is ever written to and nobody ever closes, so that a read of it
//! blocks for ever. That is the strongest form of the agent contract: not that
//! the bytes look right, but that they arrive at all.

mod common;

use common::{echo_command, mcpdial, temp_home};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Long enough for a debug build to probe a server, list its tools and finish a
/// call on a busy machine, and far short of for ever, which is how long a
/// picker with nobody in front of it takes.
const BOUND: Duration = Duration::from_secs(30);

/// Longer than [`BOUND`], so a stdin that is supposed to stay silent has not
/// gone quiet on its own before the clock runs out.
const SILENCE: Duration = Duration::from_secs(45);

/// The line clap prints for a bare `mcpdial`, which is what every program,
/// pipe and `--json` has always received and must go on receiving.
const TODAYS_USAGE: &str = "Usage: mcpdial [OPTIONS] <COMMAND>";

/// What the picker puts on the screen, none of which a pipe may ever see.
const SERVER_QUESTION: &str = "server (1-";
const TOOL_QUESTION: &str = "tool (1-";
const ARGUMENT_QUESTION: &str = "message (string, required)";

fn home_with_echo(tag: &str) -> PathBuf {
    let home = temp_home(tag);
    let saved = mcpdial(&home)
        .args(["add", "echo", "--stdio", &echo_command(), "--no-probe"])
        .output()
        .expect("spawn mcpdial");
    assert!(saved.status.success(), "{saved:?}");
    home
}

/// Everything a run printed, on either stream, and what it exited with.
struct Finished {
    code: i32,
    output: String,
    took: Duration,
}

/// Runs `child` to its end, or kills it once [`BOUND`] is up and fails. `held`
/// is its stdin, kept open for as long as it lives: a read of that blocks for
/// ever, which is exactly what the picker would do here.
fn within_bound(mut child: Child, held: Option<ChildStdin>, printed: &Printed) -> Finished {
    let started = Instant::now();
    let mut waited = None;
    while started.elapsed() < BOUND {
        if let Some(status) = child.try_wait().expect("wait for mcpdial") {
            waited = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let took = started.elapsed();
    printed.drained();
    let Some(status) = waited else {
        child.kill().ok();
        child.wait().ok();
        panic!(
            "still running after {BOUND:?}: it stopped to ask something nobody \
             can answer.\nwhat it had printed:\n{}",
            printed.text()
        );
    };
    drop(held);
    Finished {
        code: status.code().unwrap_or(-1),
        output: printed.text(),
        took,
    }
}

/// Both of a child's streams, drained as they come, so that a child filling a
/// pipe buffer is never mistaken for one that stopped to ask something.
struct Printed {
    kept: Arc<Mutex<Vec<u8>>>,
    readers: Mutex<Vec<JoinHandle<()>>>,
}

impl Printed {
    fn text(&self) -> String {
        let raw = String::from_utf8_lossy(&self.kept.lock().unwrap()).replace('\r', "");
        // clap names the command after argv[0], so its usage line reads
        // `mcpdial.exe` on Windows where it reads `mcpdial` everywhere else,
        // exactly as `tests/contract.rs` normalises it.
        without_escapes(&raw).replace("mcpdial.exe", "mcpdial")
    }

    /// Waits, briefly, for both streams to reach their end: a command that has
    /// exited has printed everything it is going to, but the threads reading it
    /// may not have caught up.
    fn drained(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self
                .readers
                .lock()
                .unwrap()
                .iter()
                .all(JoinHandle::is_finished)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for `needle` to be printed, and fails once [`BOUND`] is up.
    fn wait_for(&self, needle: &str) -> String {
        let started = Instant::now();
        loop {
            let so_far = self.text();
            if so_far.contains(needle) {
                return so_far;
            }
            assert!(
                started.elapsed() < BOUND,
                "{needle:?} was not printed inside {BOUND:?}; what was:\n{so_far}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The escape sequences a terminal's output is full of, taken back out, so
/// that an assertion is about what was written rather than how it was painted.
/// At a pseudo-terminal clap colours its own usage error and `Rich` dims the
/// line it echoes, and neither is a difference worth failing over.
fn without_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // A control sequence: parameters, then one byte that ends it.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // An operating system command, ended by a bell or by ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn both_streams(child: &mut Child) -> Printed {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let mut readers = Vec::new();
    for stream in [
        Box::new(child.stdout.take().expect("stdout is piped")) as Box<dyn Read + Send>,
        Box::new(child.stderr.take().expect("stderr is piped")),
    ] {
        let filling = Arc::clone(&kept);
        let mut stream = stream;
        readers.push(std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(read) = stream.read(&mut buf) {
                if read == 0 {
                    break;
                }
                filling.lock().unwrap().extend_from_slice(&buf[..read]);
            }
        }));
    }
    Printed {
        kept,
        readers: Mutex::new(readers),
    }
}

/// What a run printed on stdout alone, which for the bare form must stay empty.
fn stdout_of(home: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let mut cmd = mcpdial(home);
    cmd.args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("spawn mcpdial");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `args` run with an open pipe on stdin that nothing is ever written to.
fn with_a_silent_pipe_on_stdin(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Finished {
    let mut cmd = mcpdial(home);
    cmd.args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcpdial");
    let held = child.stdin.take();
    let printed = both_streams(&mut child);
    within_bound(child, held, &printed)
}

/// `script` gives what it runs a pseudo-terminal for stdin and stdout both, so
/// `Rich` is chosen and the picker is possible. BSD `script` takes the command
/// as arguments; util-linux wants one string after `-c`, and `-e` to hand the
/// command's exit status on. `PATH` is a directory of this run's own with
/// nothing in it, so `fzf` is absent however the machine is set up and the
/// numbered list is what answers.
fn pty(home: &Path, line: &str, env: &[(&str, &str)]) -> Command {
    let bsd = cfg!(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ));
    let empty = home.join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    let mut cmd = Command::new("/usr/bin/script");
    cmd.arg("-q");
    if bsd {
        cmd.args(["/dev/null", "/bin/sh", "-c", line]);
    } else {
        cmd.args(["-e", "-c", line, "/dev/null"]);
    }
    cmd.env("MCPDIAL_HOME", home)
        .env("PATH", &empty)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("MCPDIAL_PLAIN")
        .env_remove("MCPDIAL_JSON")
        .env_remove("MCPDIAL_TIMEOUT")
        .env_remove("NO_COLOR")
        .env("TERM", "xterm")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd
}

/// The command line that reaches mcpdial inside the pseudo-terminal, with the
/// binary named in full because a test's PATH is emptied out from under it.
fn call_line(args: &[&str]) -> String {
    std::iter::once(env!("CARGO_BIN_EXE_mcpdial"))
        .chain(args.iter().copied())
        .map(|word| format!("'{}'", word.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn under_pty(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Finished {
    let mut child = pty(home, &call_line(args), env)
        .spawn()
        .expect("spawn script");
    let held = child.stdin.take();
    let printed = both_streams(&mut child);
    within_bound(child, held, &printed)
}

fn no_pty() -> bool {
    if cfg!(unix) {
        return false;
    }
    eprintln!("skipped: needs `script` to make a pseudo-terminal");
    true
}

/// The rule the whole feature lives under. A bare `mcpdial` with anything but a
/// person on the other end is clap's usage error on stderr, nothing at all on
/// stdout and exit 2 - and it arrives, rather than waiting for a keystroke that
/// is not coming.
#[test]
fn a_bare_mcpdial_under_a_pipe_is_todays_usage_error_and_never_waits() {
    let home = home_with_echo("pick-pipe");
    for (args, env) in [
        (vec![], vec![]),
        (vec!["--json"], vec![]),
        (vec!["--plain"], vec![]),
        (vec![], vec![("MCPDIAL_PLAIN", "1")]),
        (vec![], vec![("MCPDIAL_JSON", "1")]),
        (vec![], vec![("TERM", "dumb")]),
    ] {
        let done = with_a_silent_pipe_on_stdin(&home, &args, &env);
        assert_eq!(done.code, 2, "{args:?} {env:?}: {}", done.output);
        assert!(
            done.output.contains(TODAYS_USAGE),
            "{args:?} {env:?}: {}",
            done.output
        );
        for asked in [SERVER_QUESTION, TOOL_QUESTION, ARGUMENT_QUESTION] {
            assert!(
                !done.output.contains(asked),
                "{args:?} {env:?} offered {asked:?} to a pipe: {}",
                done.output
            );
        }
        assert!(
            done.took < BOUND,
            "{args:?} {env:?} took {:?}, which is the bound",
            done.took
        );
        assert_eq!(
            stdout_of(&home, &args, &env),
            "",
            "{args:?} {env:?} wrote to stdout, which clap's usage error never has"
        );
    }
}

/// `mcpdial pick`, the explicit spelling of the bare form, refuses a pipe
/// rather than reading one: exit 2, and the commands that do the same job
/// without a person.
#[test]
fn pick_under_a_pipe_refuses_instead_of_reading_it() {
    let home = home_with_echo("pick-explicit-pipe");
    for args in [
        vec!["pick"],
        vec!["--json", "pick"],
        vec!["--plain", "pick"],
    ] {
        let done = with_a_silent_pipe_on_stdin(&home, &args, &[]);
        assert_eq!(done.code, 2, "{args:?}: {}", done.output);
        assert!(
            done.output.contains("pick needs a"),
            "{args:?}: {}",
            done.output
        );
        assert!(
            !done.output.contains(SERVER_QUESTION),
            "{args:?} offered a list to a pipe: {}",
            done.output
        );
        assert!(done.took < BOUND, "{args:?} took {:?}", done.took);
    }
}

/// The dangerous half of the same rule. At a terminal `Rich` is chosen and
/// everything about the output changes; only the question of what is on stdin
/// stands between a piped-in caller and a wait with no end. The pipe here is
/// fed by a process that says nothing for longer than the bound.
#[test]
fn a_terminal_reading_a_silent_pipe_still_gets_the_usage_error() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("pick-tty-pipe");
    let piped_in = format!("sleep {} | {}", SILENCE.as_secs(), call_line(&[]));
    let mut child = pty(&home, &piped_in, &[]).spawn().expect("spawn script");
    let held = child.stdin.take();
    let printed = both_streams(&mut child);
    let shown = printed.wait_for(TODAYS_USAGE);
    assert!(!shown.contains(SERVER_QUESTION), "{shown}");
    // The sleep still holds the pipeline open, so the shell has not exited and
    // never will inside the bound; the command inside it is what was watched.
    child.kill().ok();
    child.wait().ok();
    drop(held);
}

/// Every way of asking for the piped bytes outright is still the usage error at
/// a terminal on both ends, where nothing but the flag stands in the way.
#[test]
fn nothing_that_asked_for_plain_output_is_ever_offered_a_list() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("pick-flags");
    for (args, env) in [
        (vec!["--json"], vec![]),
        (vec!["--plain"], vec![]),
        (vec![], vec![("MCPDIAL_PLAIN", "1")]),
        (vec![], vec![("TERM", "dumb")]),
    ] {
        let done = under_pty(&home, &args, &env);
        assert_eq!(done.code, 2, "{args:?} {env:?}: {}", done.output);
        assert!(
            done.output.contains(TODAYS_USAGE),
            "{args:?} {env:?}: {}",
            done.output
        );
        assert!(
            !done.output.contains(SERVER_QUESTION),
            "{args:?} {env:?} offered a list it must not offer: {}",
            done.output
        );
        assert!(
            done.took < BOUND,
            "{args:?} {env:?} took {:?}, the bound",
            done.took
        );
    }
}

/// And what it is all for: at a terminal the servers are offered, then the
/// tools, then what the tool takes, the call is made, and the line that would
/// have made it outright is printed so it can go into a script.
#[test]
fn at_a_terminal_a_picked_call_runs_and_prints_the_command_it_ran() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("pick-tty");
    if !cfg!(feature = "rich") {
        // The agent-only build has no `Rich` to ask with, so a terminal is a
        // pipe here too and the bare form is the usage error it always was.
        let done = under_pty(&home, &[], &[]);
        assert_eq!(done.code, 2, "{}", done.output);
        assert!(done.output.contains(TODAYS_USAGE), "{}", done.output);
        assert!(!done.output.contains(SERVER_QUESTION), "{}", done.output);
        assert!(done.took < BOUND, "took {:?}, the bound", done.took);
        return;
    }
    for args in [vec![], vec!["pick"]] {
        let mut child = pty(&home, &call_line(&args), &[])
            .spawn()
            .expect("spawn script");
        let mut typing = child.stdin.take().expect("stdin is piped");
        let printed = both_streams(&mut child);
        // Each answer waits for its own question to be on the screen, so what
        // is asserted below is an answer to it rather than a line that raced
        // past it. `echo` is the first tool the echo server lists.
        for (question, answer) in [
            (SERVER_QUESTION, "1"),
            (TOOL_QUESTION, "1"),
            (ARGUMENT_QUESTION, "hello there"),
        ] {
            printed.wait_for(question);
            writeln!(typing, "{answer}").unwrap();
            typing.flush().unwrap();
        }
        let done = within_bound(child, Some(typing), &printed);
        assert_eq!(done.code, 0, "{args:?}: {}", done.output);
        assert!(
            done.output.contains("echo  connected"),
            "{args:?}: the server was offered with the status of its last probe: {}",
            done.output
        );
        assert!(
            done.output.contains("Echo a message back."),
            "{args:?}: the tool was offered with the first line of what it is for: {}",
            done.output
        );
        assert!(
            done.output.contains("Echo: hello there"),
            "{args:?}: the answer was sent as the argument: {}",
            done.output
        );
        assert!(
            done.output
                .contains("mcpdial call echo echo 'message=hello there'"),
            "{args:?}: the call it made is printed as a command line: {}",
            done.output
        );
    }
}

/// Leaving the picker sends nothing and is not a failure: `^D` at the first
/// question says so, and it says it inside the bound rather than hanging on.
#[test]
fn leaving_the_picker_sends_nothing_and_exits_cleanly() {
    if no_pty() || !cfg!(feature = "rich") {
        return;
    }
    let home = home_with_echo("pick-left");
    let mut child = pty(&home, &call_line(&["pick"]), &[])
        .spawn()
        .expect("spawn script");
    let mut typing = child.stdin.take().expect("stdin is piped");
    let printed = both_streams(&mut child);
    printed.wait_for(SERVER_QUESTION);
    // ^D on an empty line is end-of-file to the line editor.
    typing.write_all(&[0x04]).unwrap();
    typing.flush().unwrap();
    let done = within_bound(child, Some(typing), &printed);
    assert_eq!(done.code, 0, "{}", done.output);
    assert!(done.output.contains("nothing picked"), "{}", done.output);
    assert!(!done.output.contains(TOOL_QUESTION), "{}", done.output);
}
