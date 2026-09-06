//! Prompting for a missing argument, and the far more important half: not
//! prompting.
//!
//! A prompt is the one thing in the terminal experience that can hang its
//! caller for ever. So every test here that is not about a person typing runs
//! the command with an open pipe on its stdin - one nothing is ever written to
//! and nobody ever closes - and gives it a wall clock to finish inside. A
//! command that stops to ask a question there never finishes, so it is the
//! clock that fails, whether or not the output happens to look right.

mod common;

use common::{echo_command, mcpdial, temp_home};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Long enough for a debug build to spawn a server and finish a call on a busy
/// machine, and far short of for ever, which is how long a prompt with nobody
/// in front of it takes.
const BOUND: Duration = Duration::from_secs(20);

/// Longer than [`BOUND`], so that a stdin which is supposed to stay silent has
/// not gone quiet on its own before the clock runs out. Short enough that the
/// process it belongs to is gone soon after the test is.
const SILENCE: Duration = Duration::from_secs(30);

/// The question `echo` is asked when nothing filled in `message`. Kept short so
/// that no terminal width can wrap it away from a search.
const QUESTION: &str = "message (string, required)";

/// What a missing `message` gets where there is nobody to ask: the server's own
/// complaint, and the usage line under it. These are the bytes the contract
/// snapshots hold.
const TODAYS_ERROR: &str = "Required at message";
const TODAYS_HINT: &str = "mcpdial call echo echo message=<string>";

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
/// is its stdin, kept open for as long as it lives and never written to: a read
/// of that blocks for ever, which is exactly what a prompt would do here.
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
            "still running after {BOUND:?}: it stopped to ask a question that nobody \
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

/// Both of a child's streams, drained as they come. Draining matters twice
/// over: a child that fills a pipe buffer must not be mistaken for one that
/// stopped to ask something, and a question has to be readable before it is
/// answered.
struct Printed {
    kept: Arc<Mutex<Vec<u8>>>,
    readers: Mutex<Vec<JoinHandle<()>>>,
}

impl Printed {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.kept.lock().unwrap()).replace('\r', "")
    }

    /// Waits, briefly, for both streams to reach their end. A command that has
    /// exited has printed everything it is going to, but the threads reading it
    /// may not have caught up, and what they have not read yet is not something
    /// to judge it by.
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

/// `args` run with an open pipe on stdin that nothing is ever written to.
fn with_a_silent_pipe_on_stdin(home: &Path, args: &[&str]) -> Finished {
    let mut child = mcpdial(home)
        .args(args)
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
/// `Rich` is chosen and a prompt is possible. BSD `script` takes the command as
/// arguments; util-linux wants one string after `-c`, and `-e` to hand the
/// command's exit status on. Either way the line runs under `sh`, so a pipeline
/// written into it is a pipeline.
fn pty(home: &Path, line: &str, env: &[(&str, &str)]) -> Command {
    let bsd = cfg!(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ));
    let mut cmd = Command::new("script");
    cmd.arg("-q");
    if bsd {
        cmd.args(["/dev/null", "sh", "-c", line]);
    } else {
        cmd.args(["-e", "-c", line, "/dev/null"]);
    }
    cmd.env("MCPDIAL_HOME", home)
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

fn under_pty(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Finished {
    let mut child = pty(home, &call_line(args), env)
        .spawn()
        .expect("spawn script");
    let held = child.stdin.take();
    let printed = both_streams(&mut child);
    within_bound(child, held, &printed)
}

/// The command line that reaches mcpdial inside the pseudo-terminal, with the
/// binary named in full because a test's PATH is not the developer's.
fn call_line(args: &[&str]) -> String {
    std::iter::once(env!("CARGO_BIN_EXE_mcpdial"))
        .chain(args.iter().copied())
        .map(|word| format!("'{}'", word.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn no_pty() -> bool {
    if cfg!(unix) {
        return false;
    }
    eprintln!("skipped: needs `script` to make a pseudo-terminal");
    true
}

/// The rule the whole feature lives under: a missing required argument with
/// anything but a person on stdin is today's error, and it arrives.
#[test]
fn a_pipe_gets_todays_error_and_never_waits_for_an_answer() {
    let home = home_with_echo("prompts-pipe");
    for args in [
        vec!["call", "echo", "echo"],
        vec!["call", "echo", "echo", "{}"],
        vec!["--json", "call", "echo", "echo"],
        vec!["--plain", "call", "echo", "echo"],
    ] {
        let done = with_a_silent_pipe_on_stdin(&home, &args);
        assert_eq!(done.code, 1, "{args:?}: {}", done.output);
        assert!(
            done.output.contains(TODAYS_ERROR),
            "{args:?}: {}",
            done.output
        );
        assert!(
            !done.output.contains(QUESTION),
            "{args:?} asked a question into a pipe: {}",
            done.output
        );
        assert!(
            done.took < BOUND,
            "{args:?} took {:?}, which is the bound",
            done.took
        );
    }
}

/// The dangerous half of the same rule. At a terminal `Rich` is chosen and
/// everything else about the output changes; only the question of what is on
/// stdin stands between a piped-in caller and a wait with no end. The pipe here
/// is fed by a process that says nothing for longer than the bound, so a
/// command that reads it does not finish, and this test does not either.
#[test]
fn a_terminal_reading_a_silent_pipe_still_gets_todays_error() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("prompts-tty-pipe");
    let piped_in = format!(
        "sleep {} | {}",
        SILENCE.as_secs(),
        call_line(&["call", "echo", "echo"])
    );
    let mut child = pty(&home, &piped_in, &[]).spawn().expect("spawn script");
    let held = child.stdin.take();
    let printed = both_streams(&mut child);
    let shown = printed.wait_for(TODAYS_ERROR);
    assert!(shown.contains(TODAYS_HINT), "{shown}");
    assert!(!shown.contains(QUESTION), "{shown}");
    // The sleep still holds the pipeline open, so the shell has not exited and
    // never will inside the bound; the command inside it is what was watched.
    child.kill().ok();
    child.wait().ok();
    drop(held);
}

/// `--json` and `--plain` never prompt, whatever the terminal. Each is run at a
/// terminal on both ends, where nothing but the flag stands in the way.
#[test]
fn nothing_that_asked_for_plain_output_is_ever_asked_a_question() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("prompts-flags");
    for (args, env) in [
        (vec!["--json", "call", "echo", "echo"], vec![]),
        (vec!["--plain", "call", "echo", "echo"], vec![]),
        (vec!["call", "echo", "echo"], vec![("MCPDIAL_PLAIN", "1")]),
        (vec!["call", "echo", "echo"], vec![("TERM", "dumb")]),
    ] {
        let done = under_pty(&home, &args, &env);
        assert_eq!(done.code, 1, "{args:?} {env:?}: {}", done.output);
        assert!(
            done.output.contains(TODAYS_ERROR),
            "{args:?} {env:?}: {}",
            done.output
        );
        assert!(
            !done.output.contains(QUESTION),
            "{args:?} {env:?} asked a question it must not ask: {}",
            done.output
        );
        assert!(
            done.took < BOUND,
            "{args:?} {env:?} took {:?}, the bound",
            done.took
        );
    }
}

/// And what it is all for: at a terminal the missing argument is asked for, the
/// answer goes into the call, and the line that would have made the same call
/// outright is printed so it can go into a script.
#[test]
fn a_terminal_is_asked_for_what_the_call_left_out() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("prompts-tty");
    if !cfg!(feature = "rich") {
        // The agent-only build has no `Rich` to ask with, so a terminal is a
        // pipe here too, and gets exactly what a pipe gets.
        let done = under_pty(&home, &["call", "echo", "echo"], &[]);
        assert!(done.output.contains(TODAYS_ERROR), "{}", done.output);
        assert!(!done.output.contains(QUESTION), "{}", done.output);
        assert!(done.took < BOUND, "took {:?}, the bound", done.took);
        return;
    }
    let mut child = pty(&home, &call_line(&["call", "echo", "echo"]), &[])
        .spawn()
        .expect("spawn script");
    let mut typing = child.stdin.take().expect("stdin is piped");
    let printed = both_streams(&mut child);
    // Answered only once the question is on the screen, so what is asserted
    // below is an answer to it and not a line that raced past it.
    printed.wait_for(QUESTION);
    writeln!(typing, "hello there").unwrap();
    typing.flush().unwrap();

    let done = within_bound(child, Some(typing), &printed);
    assert_eq!(done.code, 0, "{}", done.output);
    assert!(
        done.output.contains("Echo: hello there"),
        "the answer was sent as the argument: {}",
        done.output
    );
    assert!(
        done.output
            .contains("mcpdial call echo echo 'message=hello there'"),
        "the finished call is echoed as a command line: {}",
        done.output
    );
    assert!(
        !done.output.contains(TODAYS_ERROR),
        "nothing was left for the server to complain about: {}",
        done.output
    );
}

/// A call that is already complete is not interrupted to be asked about, and
/// neither is a tool that requires nothing.
#[test]
fn a_complete_call_at_a_terminal_is_left_alone() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("prompts-complete");
    for (args, printed) in [
        (vec!["call", "echo", "echo", "message=hi"], "Echo: hi"),
        (
            vec!["call", "echo", "echo", r#"{"message":"hi"}"#],
            "Echo: hi",
        ),
        (vec!["call", "echo", "count"], "count=1"),
    ] {
        let done = under_pty(&home, &args, &[]);
        assert_eq!(done.code, 0, "{args:?}: {}", done.output);
        assert!(done.output.contains(printed), "{args:?}: {}", done.output);
        assert!(
            !done.output.contains("required)") && !done.output.contains("enter to skip"),
            "{args:?} was asked something: {}",
            done.output
        );
        assert!(done.took < BOUND, "{args:?} took {:?}", done.took);
    }
}
