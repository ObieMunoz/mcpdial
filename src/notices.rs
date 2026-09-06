//! What a server says while a call is running, on its way to stderr.
//!
//! The rule this file exists to keep is the one in `present.rs`: what a pipe
//! reads is frozen. A progress line is company for a person waiting, so it goes
//! through [`Presenter::progress`], which only the terminal presenter renders -
//! a pipe, `--json` and `--plain` therefore cannot receive one whatever a
//! server sends. `--progress` is the deliberate exception: it was asked for, so
//! it prints wherever it was asked for, and one JSON object per line under
//! `--json` so that stdout stays parseable and stderr stays parseable too.

use crate::present::{self, Presenter};
use crate::Cli;
use mcpdial::notify::{Body, Level, Notice};
use mcpdial::session::Watcher;
use serde_json::json;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

/// How often the one updating line is redrawn. A server may send thousands of
/// notifications a second, and the call it belongs to must not slow to the
/// speed of a terminal because somebody is watching it.
const REDRAW_EVERY: Duration = Duration::from_millis(80);

/// Where the notifications arriving during one request are shown.
pub struct Notices<'a> {
    ui: &'a dyn Presenter,
    json: bool,
    /// `--progress`: one line per notification wherever stderr goes, so that a
    /// captured log holds what the server reported.
    lines: bool,
    /// Whether the one updating line has anywhere to go.
    spinner: bool,
    threshold: Level,
    last_drawn: Option<Instant>,
    /// A report the throttle held back. It is drawn before the line is
    /// finished, because the last thing a server said is the one worth leaving
    /// on the screen.
    held_back: Option<String>,
}

impl<'a> Notices<'a> {
    pub fn new(ui: &'a dyn Presenter, cli: &Cli) -> Self {
        Self {
            ui,
            json: cli.json,
            lines: cli.progress,
            spinner: !cli.progress && a_person_is_watching(cli),
            threshold: cli.log_level.unwrap_or(Level::DEFAULT),
            last_drawn: None,
            held_back: None,
        }
    }

    /// The request is over: the last report the throttle held back is shown,
    /// and then the updating line is finished before the result goes out, so
    /// the two never share a line.
    pub fn finish(&mut self) {
        if let Some(held) = self.held_back.take() {
            self.ui.progress(&held);
        }
        if self.last_drawn.take().is_some() {
            self.ui.progress_end();
        }
    }

    fn wire(notice: &Notice<'_>) -> String {
        json!({"notification": {
            "method": notice.message["method"],
            "params": notice.message["params"],
        }})
        .to_string()
    }

    /// One line on stderr, and the updating line cleared out of its way first.
    fn say(&mut self, notice: &Notice<'_>, prose: String) {
        self.finish();
        match self.json {
            true => self.ui.err_line(&Self::wire(notice)),
            false => self.ui.err_line(&prose),
        }
    }

    fn draw(&mut self, notice: &Notice<'_>) {
        let line = format!("working... {}", notice.summary());
        let now = Instant::now();
        let too_soon = self
            .last_drawn
            .is_some_and(|last| now.duration_since(last) < REDRAW_EVERY);
        if too_soon {
            self.held_back = Some(line);
            return;
        }
        self.last_drawn = Some(now);
        self.held_back = None;
        self.ui.progress(&line);
    }
}

impl Watcher for Notices<'_> {
    fn wants_progress(&self) -> bool {
        self.lines || self.spinner
    }

    fn notice(&mut self, notice: &Notice<'_>) {
        match &notice.body {
            Body::Progress { .. } if self.lines => {
                let prose = format!("progress: {}", notice.summary());
                self.say(notice, prose);
            }
            Body::Progress { .. } if self.spinner => self.draw(notice),
            Body::Progress { .. } => {}
            Body::Log { level, .. } => {
                if *level >= self.threshold {
                    let prose = notice.summary();
                    self.say(notice, prose);
                }
            }
        }
    }
}

/// Whether a person is watching this run go by.
///
/// It is the question `Presenter::choose` asks of stdout, and the one the line
/// itself needs answered as well: the updating line goes to stderr, and a
/// redirected stderr is a log file rather than a screen. It also decides
/// whether a `progressToken` is sent at all, which is why it is settled here
/// and not left to the presenter - a server told to report progress into a pipe
/// is being asked for bytes nobody will read.
fn a_person_is_watching(cli: &Cli) -> bool {
    let plain_by_env = std::env::var(present::ENV_PLAIN).is_ok_and(|v| !v.is_empty() && v != "0");
    let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
    cfg!(feature = "rich")
        && !cli.json
        && !cli.plain
        && !plain_by_env
        && !dumb
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}
