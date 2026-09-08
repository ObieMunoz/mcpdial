//! Where a shell line comes from, and what Tab has to offer it.
//!
//! A terminal gets rustyline with history and completion; a pipe gets lines.
//! The difference decides more than input: a piped shell fetches no listing it
//! was not asked for, because nothing is going to complete against it.

use super::complete::{ShellHelper, Suggests};
use crate::present::{Health, Presenter};
use crate::{field_values, present};
use mcpdial::{client, Error, Store};
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// How long Tab waits for a server's suggestions before offering what it can
/// answer on its own.
///
/// `completion/complete` is a network round trip inside a keystroke. Two
/// seconds is already longer than help-while-typing is worth, and long enough
/// for a server that was ever going to answer; past it the line editor goes on
/// without the server, and a server that never answers at all costs one Tab two
/// seconds rather than the session.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(2);

/// The open session, lent to Tab completion for as long as the line editor is
/// reading one line.
///
/// rustyline hands a completer a `&self` and nothing else, so the session that
/// a `completion/complete` needs cannot be borrowed into it. The shell moves the
/// session in before each read and takes it back the moment the read returns;
/// nothing runs between those two points but the editor, and what the editor
/// calls only ever borrows what it finds here.
#[derive(Default, Clone)]
pub(crate) struct Lent(Rc<RefCell<Option<client::Connection>>>);

impl Lent {
    /// One read of the line editor, with the session on loan for its length.
    pub(crate) fn reading<R>(
        &self,
        conn: client::Connection,
        read: impl FnOnce() -> R,
    ) -> (client::Connection, R) {
        *self.0.borrow_mut() = Some(conn);
        let outcome = read();
        let handed_back = self.0.borrow_mut().take();
        let conn = handed_back.expect("what is lent is borrowed, never taken");
        (conn, outcome)
    }
}

impl Suggests for Lent {
    /// The server's own suggestions, bounded by [`COMPLETION_TIMEOUT`] and
    /// answered with nothing at all when none come back. A failure here is
    /// silence on purpose: the alternative is an error printed into the middle
    /// of the line being typed. `-v` traces this request and its failure like
    /// every other.
    fn values(&self, reference: Value, argument: Value, context: Option<Value>) -> Vec<String> {
        let mut lent = self.0.borrow_mut();
        let Some(conn) = lent.as_mut() else {
            return Vec::new();
        };
        conn.session
            .complete_within(COMPLETION_TIMEOUT, reference, argument, context)
            .map(|found| found.values)
            .unwrap_or_default()
    }
}

/// Where shell input comes from. A terminal gets line editing, history and
/// completion; anything else is read a line at a time exactly as before, which
/// is what scripts and pipes depend on.
pub(crate) enum Input {
    Tty {
        editor: Box<rustyline::Editor<ShellHelper, rustyline::history::DefaultHistory>>,
        history: std::path::PathBuf,
        /// What the server is called at this prompt. The prompt itself is built
        /// fresh for each line, because the status dot in it is only true for
        /// as long as the state it was read from.
        label: String,
        interrupts: u8,
    },
    Pipe {
        stdin: std::io::Stdin,
        /// Printed before each line when a human is typing into a redirected stdout.
        prompt: Option<String>,
    },
}

impl Input {
    /// A terminal reader where a line editor has both its ends, which is
    /// [`Presenter::edits_lines`]: rustyline draws its prompt on stdout and
    /// takes keys from stdin. Anything else keeps today's behaviour - prompts
    /// on stderr, nothing else - and `interactive` is the input side alone,
    /// saying only whether somebody is typing into that redirected stdout.
    pub(crate) fn open(
        store: &Store,
        r: &client::Resolved,
        label: &str,
        ui: &dyn Presenter,
        interactive: bool,
    ) -> Result<Self, Error> {
        if !ui.edits_lines() {
            // A redirected stdout is the agent's, whatever is on stdin, so what
            // it is prompted with is the plain name and bracket it has always
            // been given rather than anything a state could move.
            return Ok(Input::Pipe {
                stdin: std::io::stdin(),
                prompt: interactive.then(|| present::Plain.shell_prompt(label, Health::Fine)),
            });
        }
        let config = rustyline::Config::builder()
            .completion_type(rustyline::CompletionType::List)
            .auto_add_history(false)
            .build();
        let mut editor = rustyline::Editor::with_config(config)
            .map_err(|e| Error::usage(format!("cannot start line editing: {e}")))?;
        editor.set_helper(Some(ShellHelper::default()));
        let history = store.history_path(r.saved.then_some(r.name.as_str()));
        editor.load_history(&history).ok();
        Ok(Input::Tty {
            editor: Box::new(editor),
            history,
            label: label.to_string(),
            interrupts: 0,
        })
    }

    /// The next line, or `None` when the session should end.
    ///
    /// `health` is read afresh for every line, and the prompt built from it
    /// here, which is what keeps the dot honest without anything ever writing
    /// over a line somebody is halfway through typing: rustyline draws the
    /// prompt itself, at the one moment there is no half-typed line to spoil.
    pub(crate) fn next(
        &mut self,
        ui: &dyn Presenter,
        health: Health,
    ) -> Result<Option<String>, Error> {
        match self {
            Input::Pipe { stdin, prompt } => {
                if let Some(prompt) = prompt {
                    ui.err(prompt);
                }
                let mut line = String::new();
                match stdin
                    .read_line(&mut line)
                    .map_err(|e| Error::usage(e.to_string()))?
                {
                    0 => Ok(None),
                    _ => Ok(Some(line)),
                }
            }
            Input::Tty {
                editor,
                label,
                interrupts,
                ..
            } => loop {
                let prompt = ui.shell_prompt(label, health);
                match editor.readline(&prompt) {
                    Ok(line) => {
                        *interrupts = 0;
                        if !line.trim().is_empty() {
                            editor.add_history_entry(line.as_str()).ok();
                        }
                        return Ok(Some(line));
                    }
                    // One ^C abandons the line being typed; a second one leaves,
                    // which is what ^C did before there was a line to abandon.
                    Err(rustyline::error::ReadlineError::Interrupted) => {
                        *interrupts += 1;
                        if *interrupts > 1 {
                            return Ok(None);
                        }
                        ui.err_line("(^C again, or `quit`, to exit)");
                    }
                    Err(rustyline::error::ReadlineError::Eof) => return Ok(None),
                    Err(e) => return Err(Error::usage(e.to_string())),
                }
            },
        }
    }

    /// Offer what the session has learned to Tab completion.
    pub(crate) fn set_completions(
        &mut self,
        tools: &[Value],
        resources: &[Value],
        templates: &[Value],
        prompts: &[Value],
    ) {
        if let Input::Tty { editor, .. } = self {
            if let Some(helper) = editor.helper_mut() {
                helper.tools = tools.to_vec();
                helper.resources = field_values(resources, "uri");
                helper.templates = field_values(templates, "uriTemplate");
                helper.prompts = prompts.to_vec();
            }
        }
    }

    /// Where Tab's server-side suggestions come from, for a server that declares
    /// it has any. Installed once: what it holds is the session itself, which
    /// outlives every listing.
    pub(crate) fn suggestions_from(&mut self, suggests: Rc<dyn Suggests>) {
        if let Input::Tty { editor, .. } = self {
            if let Some(helper) = editor.helper_mut() {
                helper.suggests = Some(suggests);
            }
        }
    }

    pub(crate) fn save_history(&mut self) {
        if let Input::Tty {
            editor, history, ..
        } = self
        {
            if let Some(dir) = history.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            editor.save_history(history).ok();
        }
    }
}

/// The tool list, kept for explaining mistakes: fetched at most once per session,
/// and best effort, so a server that will not list its tools just gets no hint.
pub(crate) fn shell_tools<'a>(
    cache: &'a mut Option<Vec<Value>>,
    conn: &mut client::Connection,
) -> &'a [Value] {
    cache
        .get_or_insert_with(|| conn.list_tools().unwrap_or_default())
        .as_slice()
}

/// The three lists a shell session holds: for Tab completion, for `help TOOL`,
/// and for explaining a name the server does not have. A `list_changed`
/// notification is a server saying one of these is no longer what it was.
#[derive(Default)]
pub(crate) struct Lists {
    pub(crate) tools: Option<Vec<Value>>,
    pub(crate) resources: Option<Vec<Value>>,
    pub(crate) templates: Option<Vec<Value>>,
    pub(crate) prompts: Option<Vec<Value>>,
}

impl Lists {
    pub(crate) fn tools(&self) -> &[Value] {
        self.tools.as_deref().unwrap_or_default()
    }
    pub(crate) fn resources(&self) -> &[Value] {
        self.resources.as_deref().unwrap_or_default()
    }
    pub(crate) fn templates(&self) -> &[Value] {
        self.templates.as_deref().unwrap_or_default()
    }
    pub(crate) fn prompts(&self) -> &[Value] {
        self.prompts.as_deref().unwrap_or_default()
    }
}
