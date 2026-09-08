//! `follow`: the resources a server says have changed, drained at the prompt.
//!
//! A notification arriving mid-call would put a line of ours between two of the
//! server's, so [`super::shell_watched`] sets it aside and this reads it back
//! where there is nothing to interrupt.

use super::input::Lists;
use super::shell_watched;
use super::status::Says;
use crate::notices::Notices;
use crate::present::Presenter;
use crate::{missing_item, print_value, Failure};
use mcpdial::session::render_resource;
use mcpdial::subscribe::{Mechanism, Sink, Subscriptions};
use mcpdial::{client, Error};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

/// The `subscribe URI [FILE]` line: what to follow, and where its contents go.
pub(crate) fn subscription_target(rest: &str) -> (&str, Sink) {
    let (uri, file) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    match file.trim() {
        "" => (uri, Sink::Shown),
        path => (uri, Sink::File(PathBuf::from(path))),
    }
}

/// Whether this server said it will report a resource changing.
///
/// Only the revisions with `resources/subscribe` declare it; 2026-07-28 folded
/// subscriptions into `subscriptions/listen`, which every server speaking that
/// revision has, so there is nothing there to declare and nothing to check.
fn advertises_subscribe(server_info: &Value) -> bool {
    server_info["capabilities"]["resources"]["subscribe"] == json!(true)
}

/// Register an interest in a resource with the server, where the revision has
/// somewhere to register it.
///
/// 2026-07-28 has no request for this: the interest travels in the filter of
/// the next `subscriptions/listen`, so the only thing to do here is remember it.
pub(crate) fn start_following(
    conn: &mut client::Connection,
    mechanism: Mechanism,
    uri: &str,
    info_cmd: &str,
) -> Result<(), Failure> {
    if let Mechanism::Listen = mechanism {
        return Ok(());
    }
    if !advertises_subscribe(&conn.server_info) {
        return Err(Failure::hinted(
            Error::usage("this server does not offer resource subscriptions"),
            format!("{info_cmd} shows what it does offer; `read URI` fetches a resource once."),
        ));
    }
    conn.session
        .subscribe_resource(uri)
        .map(|_| ())
        .map_err(|e| missing_item(e, "resources", "`resources`", info_cmd))
}

/// How long one `listen` holds the stream open, when nobody said.
const LISTEN_FOR: Duration = Duration::from_secs(5);

/// The longest one `listen` will hold it, whatever was typed: a mistyped bound
/// should cost a wait, not a session.
const LISTEN_AT_MOST: f64 = 3600.0;

pub(crate) fn listen_bound(rest: &str) -> Result<Duration, Failure> {
    if rest.is_empty() {
        return Ok(LISTEN_FOR);
    }
    match rest.parse::<f64>() {
        Ok(secs) if secs > 0.0 => Ok(Duration::from_secs_f64(secs.min(LISTEN_AT_MOST))),
        _ => Err(Failure::hinted(
            Error::usage(format!("{rest:?} is not a number of seconds")),
            format!(
                "usage: listen [SECONDS]   (default {}s)",
                LISTEN_FOR.as_secs()
            ),
        )),
    }
}

/// Everything the server has said since the last prompt, acted on now that
/// there is nothing to interrupt: whatever a stdio server pushed while nobody
/// was reading is taken first, then each list it says has changed is re-read
/// and each followed resource fetched again.
///
/// Answers how many of those failed, which counts with the failures of the
/// commands that were typed: a subscription is something the user asked for,
/// and one that cannot be kept is worth an exit code.
pub(crate) fn drain_subscriptions(
    ui: &dyn Presenter,
    says: Says,
    notices: &mut Notices<'_>,
    conn: &mut client::Connection,
    subs: &mut Subscriptions,
    lists: &mut Lists,
) -> u32 {
    let _ = shell_watched(notices, subs, |w| {
        conn.session.poll(w);
        Ok(())
    });
    let mut failures = 0;
    let reports = subs.apply(
        &mut conn.session,
        &mut lists.tools,
        &mut lists.resources,
        &mut lists.prompts,
    );
    for report in reports {
        match report {
            Ok(report) => {
                says.tell(ui, &report.line, &report.wire);
                if let Some(result) = report.shown {
                    show_resource(ui, says, &result);
                }
            }
            Err(e) => {
                failures += 1;
                let failure = Failure::hinted(
                    e,
                    "`subscriptions` lists what this session is following; \
                     `unsubscribe URI` stops one.",
                );
                match says {
                    Says::Wire => print_value(ui, &failure.to_json(), true),
                    _a_person_or_a_pipe => failure.report(ui),
                }
            }
        }
    }
    failures
}

/// A followed resource's new contents, on stdout, because that is what was
/// asked for: `subscribe URI` with nowhere to put them means show them.
fn show_resource(ui: &dyn Presenter, says: Says, result: &Value) {
    if let Says::Wire = says {
        print_value(ui, result, true);
        return;
    }
    let text = render_resource(result, &mut |_| Ok(None)).unwrap_or_default();
    ui.text(&text);
}
