//! Tool calls the server runs in the background, and the invocations that come
//! back for them.
//!
//! Protocol 2025-11-25 lets a `tools/call` carry `params.task`, in which case
//! the server answers with a task object at once and runs the tool afterwards.
//! `call --task` starts one and polls it to the end, which turns a call that
//! would have sat on a socket for ten minutes into one that reports as it goes;
//! `call --detach` starts one, prints the id, and exits, which is the shape an
//! agent wants when the next thing it does is something else.
//!
//! # Where the state is
//!
//! Nowhere here. A task belongs to the server that minted it, and `tasks/get`
//! is the only thing that knows what state it is in. What this file keeps is a
//! note - `tasks.json` beside `probes.json`, written the same way, one entry per
//! task under the server that holds it - saying that an id exists, which tool it
//! was, and how long the server said it would keep it. The note is never read as
//! the truth about a task; it is read to know which ids to ask about, and to put
//! a tool name beside a status that has none.
//!
//! So mcpdial dying takes nothing with it. The note survives, because it is a
//! file; the task survives for as long as its server does, because it was never
//! ours. A note whose ttl has passed is stale by definition - the server is
//! entitled to have forgotten the task by then - and
//! [`Store::tasks`](mcpdial::Store::tasks) drops those on the way out rather
//! than by a sweep nobody is running.
//!
//! # Where the daemon comes in
//!
//! For an HTTP server the process holding the task is somewhere else, and
//! `--detach` needs nothing from us. For a stdio server it is the child process
//! this invocation spawned, which dies with the invocation: detaching from it
//! would print an id that nothing can ever answer for. That is what
//! [`crate::daemon`] already solves - `mcpdial start NAME` keeps one stdio
//! server alive across invocations behind `run/NAME.sock` - so `--detach`
//! against a stdio server requires that daemon rather than forking a second
//! background thing beside it.
//!
//! # What goes wrong
//!
//! - *The server died.* Its tasks died with it. A later `tasks get ID` gets
//!   whatever the fresh session says, which for a task that is gone is a
//!   not-found error; the note is dropped when that happens, so the id is not
//!   offered again.
//! - *Two invocations, one file.* Every write is a read-change-write under the
//!   same [`FileLock`](mcpdial::config) `probes.json` uses, so a note written by
//!   one invocation is never lost by another writing at the same moment.
//! - *A task id that does not exist.* One request, one refusal, and a hint that
//!   says a fresh session may simply not be the session that started it.
//! - *Finished tasks.* Dropped from the note as soon as anything here sees them
//!   terminal, and dropped anyway once their ttl has passed.

use crate::cmd::info_hint;
use crate::diagnose::{advertises, missing_capability, shell_word};
use crate::failure::{Failure, EXIT_ERROR};
use crate::notices::Notices;
use crate::output::Output;
use crate::present::Presenter;
use crate::render::{print_hint, print_json, printed_result};
use mcpdial::client::{Connection, Options};
use mcpdial::config::TaskRecord;
use mcpdial::protocol::METHOD_NOT_FOUND;
use mcpdial::{Error, KnownVersion, Store};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// The revision these requests belong to.
///
/// 2025-11-25 put tasks in the base protocol under the shape spelled here.
/// 2026-07-28 moved them into an extension with a different one, so this is a
/// gate on the one revision rather than a floor: a session on the newer
/// revision is told the extension is not spoken yet instead of being sent
/// requests it would have to guess at.
const SPEAKS_TASKS: KnownVersion = KnownVersion::V2025_11_25;

/// How long a task is asked to live when `--ttl` does not say. Long enough that
/// coming back after a few other commands still finds it; the server answers
/// with what it actually agreed to, and that is what gets noted.
const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// What a poll waits when the server names no `pollInterval`, and the longest
/// it will wait when the server names a silly one.
const DEFAULT_POLL: Duration = Duration::from_secs(1);
const MAX_POLL: Duration = Duration::from_secs(30);

/// The statuses a task never leaves.
const TERMINAL: [&str; 3] = ["completed", "failed", "cancelled"];

#[derive(clap::Args)]
pub struct Flags {
    #[arg(help = crate::cli::TARGET_HELP)]
    target: String,
    #[command(subcommand)]
    what: Option<Which>,
}

#[derive(clap::Subcommand)]
enum Which {
    /// Show one task's status without waiting for it
    Get {
        /// A task id from `mcpdial call ... --detach`
        id: String,
    },
    /// Wait for one task to finish, then print its result as `call` would
    Result {
        /// A task id from `mcpdial call ... --detach`
        id: String,
    },
    /// Ask the server to stop one task
    Cancel {
        /// A task id from `mcpdial call ... --detach`
        id: String,
    },
}

/// How `call` is to run the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// `tools/call` and wait on the socket, as every call did before this. The
    /// ttl rides along even here, for the one call that becomes a task without
    /// being asked: a tool that will not run in the foreground at all.
    Now { ttl: Duration },
    /// Start a task, poll it to the end, print the result it holds.
    Wait { ttl: Duration },
    /// Start a task, print its id, leave it running.
    Detach { ttl: Duration },
}

/// The flags `call` was given, as one decision. `--task` and `--detach` are
/// exclusive at the parser, so at most one is set.
pub fn planned(task: bool, detach: bool, ttl: Option<u64>) -> Plan {
    let ttl = ttl.map_or(DEFAULT_TTL, Duration::from_secs);
    match (task, detach) {
        (_, true) => Plan::Detach { ttl },
        (true, _) => Plan::Wait { ttl },
        _ => Plan::Now { ttl },
    }
}

/// The call `call` was about to make when it reached this file.
pub struct Wanted<'a> {
    pub tool: &'a str,
    pub arguments: Value,
    /// The tools `call` had already listed, when it had a reason to. It is what
    /// says a tool insists on a task before a doomed request finds out.
    pub known: &'a [Value],
}

/// Where a result goes when one of these commands produces one: the same three
/// knobs `call` prints its own result through.
pub struct Printing<'a> {
    pub out: &'a Output,
    pub save_dir: Option<&'a Path>,
    pub json: bool,
}

/// What a planned call produced.
pub enum Outcome {
    /// A `CallToolResult`, from the tool itself or from `tasks/result`.
    Result(Value),
    /// A task left running, as the server described it.
    Detached(Value),
}

/// Run `tool` the way `plan` says, upgrading a plain call to a task when the
/// tool turns out to insist on one.
///
/// The upgrade is not a guess: a tool whose `execution.taskSupport` is
/// `required` refuses a plain call with `-32601` before it runs anything, so
/// retrying costs one refused request and never repeats work.
pub fn call(
    ui: &dyn Presenter,
    store: &Store,
    conn: &mut Connection,
    wanted: Wanted<'_>,
    plan: Plan,
    notices: &mut Notices<'_>,
) -> Result<Outcome, Failure> {
    let tool = wanted.tool;
    let ttl = match plan {
        Plan::Now { ttl } if insists_on_a_task(wanted.known, tool) && offers_tasks(conn) => {
            said_it_wants_a_task(ui, tool);
            ttl
        }
        Plan::Now { ttl } => {
            let sent = conn
                .session
                .call_tool_watching(tool, wanted.arguments.clone(), notices);
            notices.finish();
            if !refused_without_a_task(&sent, conn, tool) {
                return Ok(Outcome::Result(sent?));
            }
            said_it_wants_a_task(ui, tool);
            ttl
        }
        Plan::Wait { ttl } | Plan::Detach { ttl } => ttl,
    };
    let detaching = matches!(plan, Plan::Detach { .. });
    if detaching {
        refuse_a_detach_nothing_can_answer(store, conn)?;
    }
    started(ui, store, conn, wanted, ttl, notices, detaching)
}

/// Start the task and either leave it or see it through.
fn started(
    ui: &dyn Presenter,
    store: &Store,
    conn: &mut Connection,
    wanted: Wanted<'_>,
    ttl: Duration,
    notices: &mut Notices<'_>,
    detaching: bool,
) -> Result<Outcome, Failure> {
    speaks_tasks(conn)?;
    let tool = wanted.tool;
    let task = conn
        .session
        .call_tool_as_task(tool, wanted.arguments, ttl)
        .map_err(|e| missing_capability(e, "background tasks", &info_hint(&conn.name)))?;
    let Some(id) = id_of(&task) else {
        return Err(Failure::hinted(
            Error::transport(format!(
                "{tool} was started as a task and the server answered without a taskId: {task}"
            )),
            "Call it without --task to run it in the foreground.",
        ));
    };
    let _ = store.remember_task(
        &conn.name,
        &id,
        TaskRecord {
            tool: tool.to_string(),
            started_at: mcpdial::config::now(),
            ttl: agreed_ttl(&task).unwrap_or(ttl).as_secs(),
        },
    );
    if detaching {
        return Ok(Outcome::Detached(task));
    }
    seen_through(ui, store, conn, &id, &task, notices).map(Outcome::Result)
}

/// Poll until the task is terminal, then fetch what it produced.
fn seen_through(
    ui: &dyn Presenter,
    store: &Store,
    conn: &mut Connection,
    id: &str,
    started: &Value,
    notices: &mut Notices<'_>,
) -> Result<Value, Failure> {
    let mut state = started.clone();
    while !is_terminal(&state) {
        if status_of(&state) == "input_required" {
            ui.progress_end();
            return Err(Failure::hinted(
                Error::transport(format!(
                    "task {id} is waiting for input, which nothing polling it can supply"
                )),
                format!(
                    "Run the tool without --task to answer what it asks, or watch it with \
                     `mcpdial tasks {} get {id}`.",
                    shell_word(&conn.name)
                ),
            ));
        }
        show_progress(ui, &state);
        std::thread::sleep(poll_interval(&state));
        state = conn.session.get_task(id).map_err(|e| gone(e, conn, id))?;
    }
    ui.progress_end();
    let ended = status_of(&state).to_string();
    let result = conn.session.task_result(id, notices);
    notices.finish();
    let _ = store.forget_tasks(&conn.name, &[id.to_string()]);
    let mut result = result.map_err(|e| gone(e, conn, id))?;
    // A task the server gave up on is a failed call whatever its result says,
    // and `call` reads that off `isError` alone.
    if ended != "completed" {
        result["isError"] = json!(true);
        if result.get("content").is_none() {
            result["content"] = json!([{ "type": "text", "text": ended_without_a_result(&state) }]);
        }
    }
    Ok(result)
}

/// A task left running: the id on stdout for a shell to capture, and the whole
/// object under `--json` so an agent has the poll interval and the ttl without
/// a second request.
pub fn print_detached(ui: &dyn Presenter, task: &Value, json: bool) {
    if json {
        print_json(ui, task);
        return;
    }
    ui.line(&id_of(task).unwrap_or_default());
}

/// `tasks`: the listing, or one of the three things done to a single task.
pub fn run(
    ui: &dyn Presenter,
    store: &Store,
    opts: &Options,
    printing: Printing<'_>,
    notices: &mut Notices<'_>,
    flags: Flags,
) -> Result<u8, Failure> {
    let mut conn = crate::cmd::dial(store, opts, &flags.target)?;
    speaks_tasks(&conn)?;
    let json = printing.json;
    match flags.what {
        None => listed(ui, store, &mut conn, json),
        Some(Which::Get { id }) => shown(ui, store, &mut conn, &id, json),
        Some(Which::Result { id }) => collected(ui, store, printing, &mut conn, &id, notices),
        Some(Which::Cancel { id }) => cancelled(ui, store, &mut conn, &id, json),
    }
}

/// Every task the server will own up to, and every one this machine noted that
/// it will not.
///
/// `tasks/list` is optional, and a server that scopes tasks to a session has
/// nothing to list for a session that just opened. The notes cover both: what
/// they name is asked about one id at a time, and an id the server has never
/// heard of is reported once and then dropped.
fn listed(
    ui: &dyn Presenter,
    store: &Store,
    conn: &mut Connection,
    json: bool,
) -> Result<u8, Failure> {
    let noted = store.tasks(&conn.name).unwrap_or_default();
    // A server that declares tasks but not `list` has nothing to list, and the
    // notes below are the whole answer. One that declares no tasks at all is
    // asked anyway: its own refusal is what says so, in the words every other
    // missing capability is reported in.
    let listed = match (declares(conn, "list"), offers_any_tasks(conn)) {
        (false, true) => Vec::new(),
        _ => conn
            .session
            .list_tasks()
            .map_err(|e| missing_capability(e, "background tasks", &info_hint(&conn.name)))?,
    };
    let mut tasks = listed;
    let mut forgotten: Vec<String> = Vec::new();
    for id in noted.keys() {
        if tasks.iter().any(|t| id_of(t).as_deref() == Some(id)) {
            continue;
        }
        match conn.session.get_task(id) {
            Ok(state) => tasks.push(state),
            Err(_) => forgotten.push(id.clone()),
        }
    }
    let over: Vec<String> = tasks
        .iter()
        .filter(|t| is_terminal(t))
        .filter_map(id_of)
        .chain(forgotten.iter().cloned())
        .collect();
    let _ = store.forget_tasks(&conn.name, &over);
    for task in &mut tasks {
        name_the_tool(task, &noted);
    }
    tasks.sort_by_key(id_of);

    if json {
        print_json(
            ui,
            &json!({ "server": conn.name, "tasks": tasks, "forgotten": forgotten }),
        );
        return Ok(0);
    }
    if tasks.is_empty() {
        ui.err_line(&format!("no tasks running on {}", conn.name));
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = tasks
        .iter()
        .map(|t| {
            vec![
                id_of(t).unwrap_or_else(|| "?".into()),
                status_of(t).to_string(),
                string_at(t, "tool"),
                string_at(t, "statusMessage"),
            ]
        })
        .collect();
    ui.paged(|| ui.table(&["ID", "STATUS", "TOOL", "MESSAGE"], &rows));
    if !forgotten.is_empty() {
        ui.note(&format!(
            "the server no longer knows {}; dropped from this machine's notes",
            forgotten.join(", ")
        ));
    }
    Ok(0)
}

fn shown(
    ui: &dyn Presenter,
    store: &Store,
    conn: &mut Connection,
    id: &str,
    json: bool,
) -> Result<u8, Failure> {
    let mut state = fetched(store, conn, id)?;
    // The note is left alone even for a finished task: `result` is what usually
    // comes next, and the tool it names is what its media is filed under.
    name_the_tool(&mut state, &store.tasks(&conn.name).unwrap_or_default());
    if json {
        print_json(ui, &state);
    } else {
        ui.line(&format!("task {id}"));
        ui.line(&format!("status {}", status_of(&state)));
        for (label, key) in [("tool", "tool"), ("message", "statusMessage")] {
            let value = string_at(&state, key);
            if !value.is_empty() {
                ui.line(&format!("{label} {value}"));
            }
        }
        if !is_terminal(&state) {
            print_hint(
                ui,
                &format!(
                    "`mcpdial tasks {} result {id}` waits for it and prints what it produced.",
                    shell_word(&conn.name)
                ),
                json,
            );
        }
    }
    Ok(0)
}

/// Wait for one task and print its result the way `call` prints one.
///
/// The media a finished task holds is filed under the tool's name when this
/// machine remembers starting it, so `--save-dir` names the file the same way
/// whether the call waited or came back for it.
fn collected(
    ui: &dyn Presenter,
    store: &Store,
    printing: Printing<'_>,
    conn: &mut Connection,
    id: &str,
    notices: &mut Notices<'_>,
) -> Result<u8, Failure> {
    let stem = store
        .tasks(&conn.name)
        .unwrap_or_default()
        .get(id)
        .map_or_else(|| id.to_string(), |note| note.tool.clone());
    let state = fetched(store, conn, id)?;
    let mut result = seen_through(ui, store, conn, id, &state, notices)?;
    let (failed, _) = printed_result(
        ui,
        printing.out,
        &mut result,
        printing.json,
        printing.save_dir,
        &stem,
    )?;
    Ok(if failed { EXIT_ERROR } else { 0 })
}

fn cancelled(
    ui: &dyn Presenter,
    store: &Store,
    conn: &mut Connection,
    id: &str,
    json: bool,
) -> Result<u8, Failure> {
    let state = conn
        .session
        .cancel_task(id)
        .map_err(|e| gone(e, conn, id))?;
    let _ = store.forget_tasks(&conn.name, &[id.to_string()]);
    if json {
        print_json(ui, &state);
    } else {
        ui.line(&format!("cancelled {id}"));
    }
    Ok(0)
}

// -- the small facts about a task ------------------------------------------

/// One task's state, and the note dropped when the server says it has no such
/// task. A server that has no tasks at all is a different answer and leaves the
/// note where it is: the id may yet mean something to the server it was started
/// against.
fn fetched(store: &Store, conn: &mut Connection, id: &str) -> Result<Value, Failure> {
    match conn.session.get_task(id) {
        Ok(state) => Ok(state),
        Err(e) => {
            let failure = gone(e, conn, id);
            if !matches!(&failure.error, Error::Rpc { code, .. } if *code == METHOD_NOT_FOUND) {
                let _ = store.forget_tasks(&conn.name, &[id.to_string()]);
            }
            Err(failure)
        }
    }
}

/// A task the server will not answer for. Every reason it might not is the same
/// from here - it ended and was swept, its ttl passed, or this is not the
/// session that started it - and the hint says so rather than picking one.
fn gone(e: Error, conn: &Connection, id: &str) -> Failure {
    if matches!(&e, Error::Rpc { code, .. } if *code == METHOD_NOT_FOUND) {
        return missing_capability(e, "background tasks", &info_hint(&conn.name));
    }
    let over_http = conn.config.http.is_some();
    let session_scoped = match over_http {
        true => {
            " A server that scopes tasks to an HTTP session will not know an id started \
                 by another invocation."
        }
        false => "",
    };
    Failure::hinted(
        e,
        format!(
            "{id} is not a task {} is holding. `mcpdial tasks {}` lists the ones it is.\
             {session_scoped}",
            conn.name,
            shell_word(&conn.name)
        ),
    )
}

fn id_of(task: &Value) -> Option<String> {
    task["taskId"].as_str().map(str::to_string)
}

fn status_of(task: &Value) -> &str {
    task["status"].as_str().unwrap_or("")
}

fn is_terminal(task: &Value) -> bool {
    TERMINAL.contains(&status_of(task))
}

fn string_at(task: &Value, key: &str) -> String {
    task[key].as_str().unwrap_or("").to_string()
}

/// What the server agreed to keep the task for, which is not always what was
/// asked. `ttl` is milliseconds on the wire.
fn agreed_ttl(task: &Value) -> Option<Duration> {
    task["ttl"].as_u64().map(Duration::from_millis)
}

fn poll_interval(task: &Value) -> Duration {
    task["pollInterval"]
        .as_u64()
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_POLL)
        .clamp(Duration::from_millis(1), MAX_POLL)
}

/// The tool a task is running, which no task object carries: only the machine
/// that started it knows, so it is added under its own key beside the server's
/// own, never over one of them.
fn name_the_tool(task: &mut Value, noted: &BTreeMap<String, TaskRecord>) {
    let Some(id) = id_of(task) else { return };
    let Some(note) = noted.get(&id) else { return };
    if let Some(object) = task.as_object_mut() {
        object.entry("tool").or_insert_with(|| json!(note.tool));
    }
}

/// What a task that ended badly says when it has no result of its own to say it
/// with.
fn ended_without_a_result(state: &Value) -> String {
    let ended = status_of(state);
    match string_at(state, "statusMessage") {
        message if message.is_empty() => format!("the task {ended}"),
        message => format!("the task {ended}: {message}"),
    }
}

fn show_progress(ui: &dyn Presenter, state: &Value) {
    let message = string_at(state, "statusMessage");
    let status = status_of(state);
    match message.is_empty() {
        true => ui.progress(status),
        false => ui.progress(&format!("{status}: {message}")),
    }
}

// -- what the server has to be able to do -----------------------------------

/// Whether this session can carry the requests at all, which is a fact about
/// the revision rather than about the server's capabilities.
fn speaks_tasks(conn: &Connection) -> Result<(), Failure> {
    let running = conn.session.version();
    if running == SPEAKS_TASKS {
        return Ok(());
    }
    let name = &conn.name;
    let why = match running > SPEAKS_TASKS {
        true => format!(
            "{running} moved tasks into an extension with its own shape, which mcpdial does \
             not speak yet"
        ),
        false => format!("tasks arrived in {SPEAKS_TASKS} and {name} is running on {running}"),
    };
    Err(Failure::hinted(
        Error::usage(why),
        format!(
            "`mcpdial info {}` shows the agreed revision.",
            shell_word(name)
        ),
    ))
}

/// Whether the server said a `tools/call` may carry a task at all.
fn offers_tasks(conn: &Connection) -> bool {
    conn.server_info["capabilities"]["tasks"]["requests"]["tools"]
        .get("call")
        .is_some()
}

/// One of the optional halves of the tasks capability: `list` and `cancel` are
/// each declared or not, and a server declaring neither still serves `get` and
/// `result`.
fn declares(conn: &Connection, part: &str) -> bool {
    conn.server_info["capabilities"]["tasks"]
        .get(part)
        .is_some_and(|c| c != &Value::Bool(false))
}

fn offers_any_tasks(conn: &Connection) -> bool {
    advertises(&conn.server_info, "tasks")
}

/// A tool that will not run in the foreground at all.
fn insists_on_a_task(tools: &[Value], tool: &str) -> bool {
    tools
        .iter()
        .find(|t| t["name"].as_str() == Some(tool))
        .is_some_and(|t| t["execution"]["taskSupport"] == "required")
}

/// Whether the refusal that came back is the one a `required` tool makes at a
/// call carrying no task, as against any other `-32601`.
fn refused_without_a_task(sent: &Result<Value, Error>, conn: &mut Connection, tool: &str) -> bool {
    let Err(Error::Rpc { code, .. }) = sent else {
        return false;
    };
    if *code != METHOD_NOT_FOUND || !offers_tasks(conn) {
        return false;
    }
    let tools = conn.list_tools().unwrap_or_default();
    insists_on_a_task(&tools, tool)
}

fn said_it_wants_a_task(ui: &dyn Presenter, tool: &str) {
    ui.note(&format!(
        "{tool} only runs as a task; starting one and waiting for it"
    ));
}

/// A stdio server's task cannot outlive the process this invocation spawned, so
/// detaching from one is refused rather than answered with an id that dies on
/// the way out. `start` is what makes that process outlive us.
fn refuse_a_detach_nothing_can_answer(store: &Store, conn: &Connection) -> Result<(), Failure> {
    if conn.config.stdio.is_none() || mcpdial::daemon::is_running(store, &conn.name) {
        return Ok(());
    }
    Err(Failure::hinted(
        Error::usage(format!(
            "{} is a stdio server dialed for this command alone; a task detached from it would \
             end when this process does",
            conn.name
        )),
        match store.server(&conn.name).ok().flatten().is_some() {
            true => format!(
                "`mcpdial start {}` keeps it running so a detached task survives, or use --task \
                 to wait for this one.",
                shell_word(&conn.name)
            ),
            false => "Save it with `mcpdial add`, `mcpdial start` it, or use --task to wait for \
                      this one."
                .to_string(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: &str) -> Value {
        json!({"taskId": "t-1", "status": status})
    }

    #[test]
    fn only_the_three_terminal_statuses_end_a_poll() {
        assert!(is_terminal(&task("completed")));
        assert!(is_terminal(&task("failed")));
        assert!(is_terminal(&task("cancelled")));
        assert!(!is_terminal(&task("working")));
        assert!(!is_terminal(&task("input_required")));
        // A status this build has never heard of is not terminal: waiting for a
        // server that has moved on beats calling a live task finished.
        assert!(!is_terminal(&task("thinking-about-it")));
    }

    #[test]
    fn a_silly_poll_interval_is_brought_back_into_range() {
        assert_eq!(poll_interval(&json!({})), DEFAULT_POLL);
        assert_eq!(
            poll_interval(&json!({"pollInterval": 2500})),
            Duration::from_millis(2500)
        );
        assert_eq!(
            poll_interval(&json!({"pollInterval": 86_400_000u64})),
            MAX_POLL
        );
        assert_eq!(
            poll_interval(&json!({"pollInterval": 0})),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn the_tool_is_added_beside_what_the_server_said_and_never_over_it() {
        let noted = BTreeMap::from([(
            "t-1".to_string(),
            TaskRecord {
                tool: "slow".into(),
                started_at: 0,
                ttl: 60,
            },
        )]);
        let mut mine = task("working");
        name_the_tool(&mut mine, &noted);
        assert_eq!(mine["tool"], "slow");
        assert_eq!(mine["status"], "working");

        // A server that grows a `tool` key of its own keeps it.
        let mut theirs = json!({"taskId": "t-1", "status": "working", "tool": "theirs"});
        name_the_tool(&mut theirs, &noted);
        assert_eq!(theirs["tool"], "theirs");

        // A task nobody here started is left exactly as it arrived.
        let mut stranger = json!({"taskId": "t-9", "status": "working"});
        name_the_tool(&mut stranger, &noted);
        assert_eq!(stranger.get("tool"), None);
    }

    #[test]
    fn the_flags_settle_into_one_plan() {
        assert_eq!(planned(false, false, None), Plan::Now { ttl: DEFAULT_TTL });
        assert_eq!(planned(true, false, None), Plan::Wait { ttl: DEFAULT_TTL });
        // --ttl means the same thing on a plain call, which a tool that only
        // runs as a task turns into one anyway.
        assert_eq!(
            planned(false, false, Some(5)),
            Plan::Now {
                ttl: Duration::from_secs(5)
            }
        );
        assert_eq!(
            planned(false, true, Some(30)),
            Plan::Detach {
                ttl: Duration::from_secs(30)
            }
        );
    }

    #[test]
    fn a_tool_that_says_nothing_about_tasks_is_not_one_that_requires_them() {
        let tools = json!([
            {"name": "echo"},
            {"name": "maybe", "execution": {"taskSupport": "optional"}},
            {"name": "slow", "execution": {"taskSupport": "required"}},
        ]);
        let tools = tools.as_array().unwrap();
        assert!(!insists_on_a_task(tools, "echo"));
        assert!(!insists_on_a_task(tools, "maybe"));
        assert!(!insists_on_a_task(tools, "nope"));
        assert!(insists_on_a_task(tools, "slow"));
    }

    #[test]
    fn a_task_that_ended_badly_says_what_it_said_on_the_way_out() {
        assert_eq!(
            ended_without_a_result(&json!({"status": "failed"})),
            "the task failed"
        );
        assert_eq!(
            ended_without_a_result(&json!({"status": "failed", "statusMessage": "out of disk"})),
            "the task failed: out of disk"
        );
    }
}
