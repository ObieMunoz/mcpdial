//! Background tasks end to end: what `--task` and `--detach` send, what the
//! `tasks` subcommand reads back, and what happens to the note in `tasks.json`
//! at each step.
//!
//! Every run here is bounded by the wall clock, because every one of them polls:
//! a task that never leaves `working` would keep a client asking for ever, and
//! that is a failing test rather than a stalled suite.

mod common;

use common::{echo_command, mcpdial, run, temp_home, with_a_silent_pipe_on_stdin};
use serde_json::Value;
use std::path::Path;

/// The echo server as a saved name, speaking the revision that has tasks.
fn add_echo(home: &Path, name: &str) {
    let echo = echo_command();
    let o = run(mcpdial(home).args(["add", name, "--stdio", &echo, "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
}

/// One run with the tasks mode on, given a wall clock to finish inside.
fn with_tasks(home: &Path, how: &str, args: &[&str]) -> common::Finished {
    with_a_silent_pipe_on_stdin(
        mcpdial(home)
            .env("ECHO_SERVER_TASKS", how)
            .args(["--timeout", "20"])
            .args(args),
    )
}

fn tasks_json(home: &Path) -> Value {
    let text = std::fs::read_to_string(home.join("tasks.json")).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

fn noted(home: &Path, server: &str) -> Value {
    tasks_json(home)["tasks"][server].clone()
}

#[test]
fn a_task_augmented_call_is_polled_to_the_end_and_prints_what_it_produced() {
    let home = temp_home("task-wait");
    add_echo(&home, "echo");

    let done = with_tasks(
        &home,
        "1",
        &["call", "echo", "echo", r#"{"message":"hi"}"#, "--task"],
    );
    assert_eq!(done.code, 0, "{}", done.output);
    assert!(done.output.contains("Echo: hi"), "{}", done.output);
    // The note is written when the task starts and dropped when it ends, so a
    // call that waited leaves nothing behind for a later invocation to chase.
    assert_eq!(noted(&home, "echo"), Value::Null, "{}", tasks_json(&home));
}

#[test]
fn a_tool_that_only_runs_as_a_task_is_started_as_one_without_being_asked() {
    let home = temp_home("task-required");
    add_echo(&home, "echo");

    let done = with_tasks(&home, "1", &["call", "echo", "slow", r#"{"message":"ok"}"#]);
    assert_eq!(done.code, 0, "{}", done.output);
    assert!(done.output.contains("Echo: ok"), "{}", done.output);
    assert!(
        done.output.contains("slow only runs as a task"),
        "the upgrade is announced: {}",
        done.output
    );
}

#[test]
fn a_task_id_the_server_never_minted_is_a_failure_that_names_the_listing() {
    let home = temp_home("task-unknown");
    add_echo(&home, "echo");

    let done = with_tasks(&home, "1", &["tasks", "echo", "get", "t-9"]);
    assert_eq!(done.code, 1, "{}", done.output);
    assert!(done.output.contains("t-9 is not a task"), "{}", done.output);
    assert!(
        done.output.contains("mcpdial tasks echo"),
        "{}",
        done.output
    );
}

#[test]
fn a_server_that_speaks_the_revision_but_offers_no_tasks_says_so_in_the_usual_words() {
    let home = temp_home("task-none");
    add_echo(&home, "echo");

    let done = with_tasks(&home, "none", &["tasks", "echo"]);
    assert_eq!(done.code, 1, "{}", done.output);
    assert!(
        done.output.contains("offers no background tasks"),
        "{}",
        done.output
    );
}

#[test]
fn a_server_on_an_older_revision_is_told_which_revision_tasks_arrived_in() {
    let home = temp_home("task-old");
    add_echo(&home, "echo");

    // No mode at all: the echo server answers 2025-06-18, which has no tasks.
    let done = with_a_silent_pipe_on_stdin(mcpdial(&home).args(["tasks", "echo"]));
    assert_eq!(done.code, 2, "{}", done.output);
    assert!(
        done.output.contains("tasks arrived in 2025-11-25"),
        "{}",
        done.output
    );
}

#[test]
fn a_stdio_server_dialed_for_one_command_will_not_be_detached_from() {
    let home = temp_home("task-detach-refused");
    add_echo(&home, "echo");

    let done = with_tasks(&home, "1", &["call", "echo", "echo", "{}", "--detach"]);
    assert_eq!(done.code, 2, "{}", done.output);
    assert!(
        done.output.contains("would end when this process does"),
        "{}",
        done.output
    );
    assert!(
        done.output.contains("mcpdial start echo"),
        "the daemon is what makes a detached task survive: {}",
        done.output
    );
    assert_eq!(
        noted(&home, "echo"),
        Value::Null,
        "nothing was started, so nothing is noted"
    );
}

#[test]
fn a_note_that_outlived_its_ttl_is_neither_asked_about_nor_kept() {
    let home = temp_home("task-stale");
    add_echo(&home, "echo");
    // A task started at the epoch and kept for a second: the server is entitled
    // to have forgotten it, so nothing is spent asking about it.
    std::fs::write(
        home.join("tasks.json"),
        r#"{"tasks": {"echo": {"t-99": {"tool": "echo", "started_at": 1, "ttl": 1}}}}"#,
    )
    .unwrap();

    let done = with_tasks(&home, "1", &["tasks", "echo"]);
    assert_eq!(done.code, 0, "{}", done.output);
    assert!(!done.output.contains("t-99"), "{}", done.output);
    assert_eq!(noted(&home, "echo"), Value::Null, "{}", tasks_json(&home));
}

// -- what only a server that outlives one invocation can be asked ------------
//
// A detached task needs a server that is still there when the next command
// runs, and `start` is the only thing that provides one. Unix alone, because
// that is where the daemon is.

#[cfg(unix)]
mod detached {
    use super::*;
    // `Out` is wanted by this half alone, so it is imported here rather than
    // beside the rest: a use that only exists on unix is an unused import
    // everywhere else, and `-D warnings` makes that a failed build.
    use crate::common::Out;
    use std::time::{Duration, Instant};

    /// A config directory short enough for `run/NAME.sock` to fit the 104 bytes
    /// macOS allows a Unix socket path.
    fn short_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A daemon with an idle limit, so one a failing test leaves behind goes
    /// away on its own.
    fn start(home: &Path) -> Out {
        run(mcpdial(home)
            .env("ECHO_SERVER_TASKS", "1")
            .args(["start", "echo", "--idle", "120"]))
    }

    fn stop(home: &Path) {
        let _ = run(mcpdial(home).args(["stop", "echo"]));
        let deadline = Instant::now() + Duration::from_secs(20);
        while home.join("run/echo.sock").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `--detach` prints the id and nothing else on stdout, so a shell can take
    /// it as it comes.
    fn detach(home: &Path, extra: &[&str]) -> Out {
        let mut args = vec!["call", "echo", "echo", r#"{"message":"later"}"#, "--detach"];
        args.extend_from_slice(extra);
        run(mcpdial(home)
            .env("ECHO_SERVER_TASKS", "1")
            .args(["--timeout", "20"])
            .args(args))
    }

    #[test]
    fn a_started_server_holds_a_task_until_another_invocation_comes_for_it() {
        let home = short_home("detach");
        add_echo(&home, "echo");
        let o = start(&home);
        assert_eq!(o.code, 0, "{}", o.stderr);

        let o = detach(&home, &["--ttl", "30"]);
        assert_eq!(o.code, 0, "{}", o.stderr);
        let id = o.stdout.trim().to_string();
        assert!(!id.is_empty(), "stdout is the id alone: {:?}", o.stdout);

        // The note says what the id is, which no `tasks/get` result carries, and
        // the ttl the server agreed to rather than the one that was asked for.
        let note = &noted(&home, "echo")[&id];
        assert_eq!(note["tool"], "echo", "{}", tasks_json(&home));
        assert_eq!(note["ttl"], 30, "{}", tasks_json(&home));

        // A second invocation, a fresh session, the same server: the listing
        // finds it and puts the tool beside it.
        let done = with_a_silent_pipe_on_stdin(
            mcpdial(&home)
                .env("ECHO_SERVER_TASKS", "1")
                .args(["--json", "tasks", "echo"]),
        );
        assert_eq!(done.code, 0, "{}", done.output);
        let listed: Value = serde_json::from_str(done.output.trim()).expect("one JSON document");
        let mine = listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["taskId"] == id.as_str())
            .unwrap_or_else(|| panic!("{id} is not in {}", done.output));
        assert_eq!(mine["tool"], "echo", "{}", done.output);

        // And a third comes back for what it produced, which ends the note.
        let done =
            with_a_silent_pipe_on_stdin(mcpdial(&home).env("ECHO_SERVER_TASKS", "1").args([
                "--timeout",
                "20",
                "tasks",
                "echo",
                "result",
                &id,
            ]));
        assert_eq!(done.code, 0, "{}", done.output);
        assert!(done.output.contains("Echo: later"), "{}", done.output);
        assert_eq!(
            noted(&home, "echo")[&id],
            Value::Null,
            "{}",
            tasks_json(&home)
        );

        stop(&home);
    }

    #[test]
    fn a_cancelled_task_is_dropped_from_the_notes() {
        let home = short_home("cancel");
        add_echo(&home, "echo");
        let o = start(&home);
        assert_eq!(o.code, 0, "{}", o.stderr);

        let o = detach(&home, &[]);
        assert_eq!(o.code, 0, "{}", o.stderr);
        let id = o.stdout.trim().to_string();

        let done = with_a_silent_pipe_on_stdin(
            mcpdial(&home)
                .env("ECHO_SERVER_TASKS", "1")
                .args(["tasks", "echo", "cancel", &id]),
        );
        assert_eq!(done.code, 0, "{}", done.output);
        assert!(
            done.output.contains(&format!("cancelled {id}")),
            "{}",
            done.output
        );
        assert_eq!(
            noted(&home, "echo")[&id],
            Value::Null,
            "{}",
            tasks_json(&home)
        );

        stop(&home);
    }

    #[test]
    fn two_invocations_detaching_at_once_both_end_up_in_the_notes() {
        let home = short_home("race");
        add_echo(&home, "echo");
        let o = start(&home);
        assert_eq!(o.code, 0, "{}", o.stderr);

        // The daemon serves one caller at a time, so the two overlap where it
        // matters here: in the read-change-write of `tasks.json`.
        let racers: Vec<Out> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2).map(|_| scope.spawn(|| detach(&home, &[]))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let ids: Vec<String> = racers
            .iter()
            .map(|o| {
                assert_eq!(o.code, 0, "{}", o.stderr);
                o.stdout.trim().to_string()
            })
            .collect();
        assert_ne!(ids[0], ids[1], "the server minted one id per call");

        let notes = noted(&home, "echo");
        for id in &ids {
            assert_eq!(
                notes[id]["tool"],
                "echo",
                "{id} was written over by the other invocation: {}",
                tasks_json(&home)
            );
        }

        stop(&home);
    }
}
