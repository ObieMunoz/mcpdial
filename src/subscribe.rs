//! What a session is following, and what a server's notifications about it mean.
//!
//! Two revisions of the protocol answer the same question differently. Up to
//! 2025-11-25 a client sends `resources/subscribe` per resource and the server
//! pushes `notifications/resources/updated` on whatever stream happens to be
//! open. 2026-07-28 removed both methods and put one long-lived
//! `subscriptions/listen` stream in their place, opted into per notification
//! type. Which of the two applies is read off the version the session settled
//! on; everything above that line - what is being followed, where its contents
//! go, what has happened and has not been acted on - is the same either way and
//! lives here.
//!
//! Nothing here writes to a screen. What arrives is put aside as a fact, and
//! whoever owns the output decides when the moment is right to say so: an
//! asynchronous line drawn in the middle of a command's output would land in
//! the middle of what a pipe is reading.

use crate::notify::{Body, Listing, Notice, RESOURCE_UPDATED_METHOD};
use crate::protocol::{Error, KnownVersion, Result};
use crate::session::{self, Session};
use crate::transport::Transport;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// How a server says that something changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mechanism {
    /// 2025-11-25 and earlier: `resources/subscribe` registers the interest,
    /// and updates ride whatever stream is open when they happen.
    Subscribe,
    /// 2026-07-28: no subscribe method at all. One `subscriptions/listen`
    /// stream carries every notification type the client opted in to, and
    /// carries them only while it is being read.
    Listen,
}

impl Mechanism {
    pub fn of(version: KnownVersion) -> Self {
        match version.is_modern() {
            true => Mechanism::Listen,
            false => Mechanism::Subscribe,
        }
    }

    /// The request that carries it, for saying which of the two a session is on.
    pub fn method(self) -> &'static str {
        match self {
            Mechanism::Subscribe => "resources/subscribe",
            Mechanism::Listen => "subscriptions/listen",
        }
    }

    /// Whether updates arrive only while a stream is deliberately held open,
    /// which is what makes `listen` worth typing rather than automatic.
    pub fn needs_listening(self) -> bool {
        matches!(self, Mechanism::Listen)
    }
}

/// Where a followed resource's new contents go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sink {
    /// Onto stdout, at the next prompt.
    Shown,
    /// Into this file, replacing it whole each time.
    File(PathBuf),
}

impl Sink {
    /// How the subscription reads back in a listing.
    pub fn describe(&self) -> String {
        match self {
            Sink::Shown => "shown".to_string(),
            Sink::File(path) => path.display().to_string(),
        }
    }
}

/// One thing that has happened and has not been acted on yet.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Event {
    /// One of the three lists is no longer what it was.
    Changed(Listing),
    /// One followed resource has new contents to fetch.
    Updated(String),
}

impl Event {
    /// The notification this event came from, as `--json` puts it on stderr:
    /// the method, and the parameter that says which resource where there is
    /// one. A script reacts to this rather than to prose.
    pub fn wire(&self) -> Value {
        match self {
            Event::Changed(what) => json!({"notification": {"method": what.changed_method()}}),
            Event::Updated(uri) => json!({"notification": {
                "method": RESOURCE_UPDATED_METHOD, "params": {"uri": uri}}}),
        }
    }
}

/// What acting on one [`Event`] came to, for whoever is printing.
#[derive(Debug)]
pub struct Report {
    /// The one line a person reads.
    pub line: String,
    /// The same fact as the object `--json` puts on stderr.
    pub wire: Value,
    /// The `resources/read` result, when the resource's contents were meant for
    /// stdout rather than for a file.
    pub shown: Option<Value>,
}

/// Everything one session is following, and everything it has been told since
/// the last time anybody looked.
pub struct Subscriptions {
    mechanism: Mechanism,
    following: BTreeMap<String, Sink>,
    /// What has happened and has not been acted on.
    ///
    /// A set rather than a queue, on purpose. A server that sends a thousand
    /// `tools/list_changed` notifications during one call has still changed one
    /// list once, and the prompt owes the user one refresh rather than a
    /// thousand. An update for a resource nobody asked for is not kept at all,
    /// so what a flood can cost is bounded by what was subscribed to.
    pending: BTreeSet<Event>,
    /// Whether the server acknowledged the stream now being listened to.
    ///
    /// 2026-07-28 requires the acknowledgement to be the first message on a
    /// `subscriptions/listen` stream, so a stream that carried none is a server
    /// that has agreed to nothing - which is worth saying, because everything
    /// that follows would then be silence for a reason nobody could see.
    acknowledged: bool,
}

impl Subscriptions {
    pub fn new(version: KnownVersion) -> Self {
        Self {
            mechanism: Mechanism::of(version),
            following: BTreeMap::new(),
            pending: BTreeSet::new(),
            acknowledged: false,
        }
    }

    pub fn mechanism(&self) -> Mechanism {
        self.mechanism
    }

    /// A fresh stream is about to be opened, so nothing has acknowledged it yet.
    pub fn opening_a_stream(&mut self) {
        self.acknowledged = false;
    }

    pub fn acknowledged(&self) -> bool {
        self.acknowledged
    }

    pub fn is_empty(&self) -> bool {
        self.following.is_empty()
    }

    pub fn following(&self) -> impl Iterator<Item = (&String, &Sink)> {
        self.following.iter()
    }

    /// Start following `uri`, and say what was following it before.
    pub fn follow(&mut self, uri: &str, sink: Sink) -> Option<Sink> {
        self.following.insert(uri.to_string(), sink)
    }

    /// Stop following `uri`, or explain that it was never followed.
    ///
    /// Unsubscribing from something nobody subscribed to is a mistake worth
    /// naming rather than a no-op worth hiding: it usually means a typo in the
    /// URI, and a silent success would leave the real subscription running.
    pub fn forget(&mut self, uri: &str) -> Result<Sink> {
        self.pending.remove(&Event::Updated(uri.to_string()));
        self.following
            .remove(uri)
            .ok_or_else(|| Error::usage(format!("not subscribed to {uri}")))
    }

    /// Put a notification aside if it is one a subscription is about, and say
    /// whether it was taken.
    ///
    /// A notification that is not taken belongs to whoever renders progress and
    /// log lines, and is passed on untouched.
    pub fn record(&mut self, notice: &Notice<'_>) -> bool {
        match &notice.body {
            Body::ListChanged(what) => {
                self.pending.insert(Event::Changed(*what));
                true
            }
            Body::ResourceUpdated { uri } => {
                // A server may report an update for anything it likes. Only
                // what was asked for is kept, which is also what stops a
                // stranger's URIs from filling the queue.
                if self.following.contains_key(uri) {
                    self.pending.insert(Event::Updated(uri.clone()));
                }
                true
            }
            Body::Acknowledged { .. } => {
                self.acknowledged = true;
                true
            }
            Body::Progress { .. } | Body::Log { .. } => false,
        }
    }

    /// The `notifications` filter of a `subscriptions/listen` request: all three
    /// list changes, because a shell caches all three lists, and every resource
    /// being followed.
    pub fn filter(&self) -> Value {
        let mut filter = serde_json::Map::new();
        for what in Listing::ALL {
            filter.insert(what.listen_key().to_string(), json!(true));
        }
        filter.insert(
            "resourceSubscriptions".to_string(),
            json!(self.following.keys().collect::<Vec<_>>()),
        );
        Value::Object(filter)
    }

    /// Act on everything the server has said since the last prompt: re-read the
    /// lists it says have changed, fetch the resources it says are new, and
    /// hand back one report per event.
    ///
    /// A list that cannot be re-read is dropped rather than kept stale: the
    /// caches here are for Tab completion and for explaining mistakes, and a
    /// missing one costs a hint where a wrong one costs trust.
    pub fn apply(
        &mut self,
        session: &mut Session<Box<dyn Transport>>,
        tools: &mut Option<Vec<Value>>,
        resources: &mut Option<Vec<Value>>,
        prompts: &mut Option<Vec<Value>>,
    ) -> Vec<Result<Report>> {
        let mut reports = Vec::new();
        for event in std::mem::take(&mut self.pending) {
            let wire = event.wire();
            reports.push(match &event {
                Event::Changed(what) => {
                    let found = match what {
                        Listing::Tools => session.list_tools(),
                        Listing::Resources => session.list_resources(),
                        Listing::Prompts => session.list_prompts(),
                    }
                    .ok();
                    let line = match &found {
                        Some(items) => format!("{what} changed (now {})", items.len()),
                        None => format!("{what} changed"),
                    };
                    *match what {
                        Listing::Tools => &mut *tools,
                        Listing::Resources => &mut *resources,
                        Listing::Prompts => &mut *prompts,
                    } = found;
                    Ok(Report {
                        line,
                        wire,
                        shown: None,
                    })
                }
                Event::Updated(uri) => self.refresh(session, uri, wire),
            });
        }
        reports
    }

    /// One resource read again and put where its subscription says.
    fn refresh(
        &self,
        session: &mut Session<Box<dyn Transport>>,
        uri: &str,
        wire: Value,
    ) -> Result<Report> {
        let Some(sink) = self.following.get(uri) else {
            return Err(Error::usage(format!("not subscribed to {uri}")));
        };
        let result = session.read_resource(uri)?;
        let Sink::File(path) = sink else {
            return Ok(Report {
                line: format!("--- {uri} updated"),
                wire,
                shown: Some(result),
            });
        };
        let bytes = joined(&result)?;
        let written = bytes.len();
        save(path, &bytes)?;
        Ok(Report {
            line: format!("{uri} -> {} ({written} bytes)", path.display()),
            wire,
            shown: None,
        })
    }
}

/// Every part of a `resources/read` result as one run of bytes: the text as it
/// is, and each blob decoded, which is what a file kept in step with a resource
/// has to hold.
fn joined(result: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for body in session::resource_bodies(result)? {
        match body {
            session::ResourceBody::Text(text) => out.extend_from_slice(text.as_bytes()),
            session::ResourceBody::Bytes(bytes) => out.extend_from_slice(&bytes),
        }
    }
    Ok(out)
}

/// Replace a file's contents in one step.
///
/// A subscription exists to be read by something else while it is being
/// written, so the write goes to a sibling of the target and is renamed over
/// it: whatever is watching sees the old contents or the new ones, and never
/// half of either. A sibling rather than a temporary directory because a rename
/// is only atomic within one filesystem.
pub fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Err(Error::usage(format!(
            "{} is not a file this can be written to",
            path.display()
        )));
    };
    let beside = path.with_file_name(format!(".{name}.mcpdial-{}", std::process::id()));
    let failed =
        |what: &str, e: std::io::Error| Error::transport(format!("{what} {}: {e}", path.display()));
    std::fs::write(&beside, bytes).map_err(|e| failed("writing", e))?;
    std::fs::rename(&beside, path).map_err(|e| {
        let _ = std::fs::remove_file(&beside);
        failed("replacing", e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{self, MESSAGE_METHOD};

    fn notice_of(msg: &Value) -> Notice<'_> {
        notify::read(msg, None).expect("a notification worth reading")
    }

    fn changed(what: Listing) -> Value {
        json!({"jsonrpc": "2.0", "method": what.changed_method()})
    }

    fn updated(uri: &str) -> Value {
        json!({"jsonrpc": "2.0", "method": RESOURCE_UPDATED_METHOD, "params": {"uri": uri}})
    }

    fn following(uris: &[&str]) -> Subscriptions {
        let mut subs = Subscriptions::new(KnownVersion::LATEST_LEGACY);
        for uri in uris {
            subs.follow(uri, Sink::Shown);
        }
        subs
    }

    #[test]
    fn the_era_decides_which_mechanism_carries_updates() {
        assert_eq!(
            Mechanism::of(KnownVersion::V2026_07_28),
            Mechanism::Listen,
            "the revision that removed resources/subscribe"
        );
        for older in [KnownVersion::V2025_11_25, KnownVersion::V2025_03_26] {
            assert_eq!(Mechanism::of(older), Mechanism::Subscribe, "{older}");
        }
        assert_eq!(Mechanism::Subscribe.method(), "resources/subscribe");
        assert_eq!(Mechanism::Listen.method(), "subscriptions/listen");
        assert!(Mechanism::Listen.needs_listening());
        assert!(!Mechanism::Subscribe.needs_listening());
    }

    #[test]
    fn a_stream_with_no_acknowledgement_is_a_server_that_agreed_to_nothing() {
        let mut subs = following(&[]);
        subs.opening_a_stream();
        assert!(!subs.acknowledged());
        let ack = json!({"jsonrpc": "2.0", "method": crate::notify::ACKNOWLEDGED_METHOD,
                         "params": {"notifications": {"toolsListChanged": true}}});
        assert!(subs.record(&notice_of(&ack)));
        assert!(subs.acknowledged());
        subs.opening_a_stream();
        assert!(
            !subs.acknowledged(),
            "each stream is acknowledged on its own"
        );
    }

    #[test]
    fn a_flood_of_the_same_notification_is_one_refresh() {
        let mut subs = following(&["counter://n"]);
        let (changed, updated) = (changed(Listing::Tools), updated("counter://n"));
        for _ in 0..500 {
            assert!(subs.record(&notice_of(&changed)));
            assert!(subs.record(&notice_of(&updated)));
        }
        assert_eq!(
            subs.pending.len(),
            2,
            "a thousand notifications about two things are two things"
        );
    }

    #[test]
    fn an_update_for_something_nobody_asked_for_is_not_kept() {
        let mut subs = following(&["counter://n"]);
        let stranger = updated("counter://somebody-elses");
        assert!(
            subs.record(&notice_of(&stranger)),
            "it is still a subscription notification"
        );
        assert!(subs.pending.is_empty(), "and still none of our business");
    }

    #[test]
    fn progress_and_log_lines_belong_to_whoever_draws_them() {
        let mut subs = following(&[]);
        let log = json!({"jsonrpc": "2.0", "method": MESSAGE_METHOD,
                         "params": {"level": "warning", "data": "slow"}});
        assert!(!subs.record(&notice_of(&log)));
        assert!(subs.pending.is_empty());
    }

    #[test]
    fn forgetting_something_never_followed_says_so() {
        let mut subs = following(&["counter://n"]);
        assert_eq!(subs.forget("counter://n").unwrap(), Sink::Shown);
        let complaint = subs.forget("counter://n").unwrap_err().to_string();
        assert!(
            complaint.contains("not subscribed to counter://n"),
            "{complaint}"
        );
        assert!(subs.is_empty());
    }

    #[test]
    fn forgetting_drops_what_was_still_pending_for_it() {
        let mut subs = following(&["counter://n"]);
        assert!(subs.record(&notice_of(&updated("counter://n"))));
        subs.forget("counter://n").unwrap();
        assert!(
            subs.pending.is_empty(),
            "an update for a subscription that is over has nowhere to go"
        );
    }

    #[test]
    fn the_listen_filter_names_every_list_and_every_followed_uri() {
        let subs = following(&["file:///b", "file:///a"]);
        assert_eq!(
            subs.filter(),
            json!({
                "toolsListChanged": true,
                "resourcesListChanged": true,
                "promptsListChanged": true,
                "resourceSubscriptions": ["file:///a", "file:///b"],
            })
        );
    }

    #[test]
    fn what_json_puts_on_stderr_is_the_notification_that_caused_it() {
        assert_eq!(
            Event::Changed(Listing::Tools).wire(),
            json!({"notification": {"method": "notifications/tools/list_changed"}})
        );
        assert_eq!(
            Event::Updated("file:///a".into()).wire(),
            json!({"notification": {"method": "notifications/resources/updated",
                                    "params": {"uri": "file:///a"}}})
        );
    }

    #[test]
    fn a_saved_file_is_replaced_whole_and_leaves_nothing_beside_it() {
        let dir = std::env::temp_dir().join(format!("mcpdial-subs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("counter.txt");
        save(&path, b"one").unwrap();
        save(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
            .collect();
        assert_eq!(left, ["counter.txt"], "the sibling is renamed, not left");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_resource_read_is_joined_text_first_and_blobs_decoded() {
        let result = json!({"contents": [
            {"uri": "x", "text": "hello "},
            {"uri": "x", "blob": "d29ybGQ="},
        ]});
        assert_eq!(joined(&result).unwrap(), b"hello world");
    }
}
