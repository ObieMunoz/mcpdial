//! `browse`: the catalog as a checklist, the way LazyVim's extras and Mason
//! read. Tick what you want, press Enter, and it is saved and dialed. What is
//! already saved starts ticked, found by the provenance `add` records; untick
//! it and it is removed after one confirmation.
//!
//! `fzf`, when on `PATH`, is the list: `--multi --reverse` with a preview pane.
//! Without it a small picker of this crate's own takes over, behind the `rich`
//! feature, and is the one place the terminal plan lets a command take the
//! whole screen. A pipe, `--json` or `--plain` gets the entries as objects,
//! exactly what `catalog --json` prints, so the agent surface holds.

use crate::pick::on_path;
use crate::present::Presenter;
use crate::{listing_row, Failure, EXIT_ERROR, LISTING_HEADERS};
use mcpdial::catalog::{self, Entry};
use mcpdial::client::{self, Options, Status};
use mcpdial::config::{self, ServerConfig};
use mcpdial::registry::{self, Pick, Registry, Resolved};
use mcpdial::{Error, Store};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};

pub struct Flags {
    /// The whole registry index instead of the catalog.
    pub all: bool,
    pub offline: bool,
    /// The hidden `--preview KEY`: fzf's preview pane asking about one entry.
    pub preview: Option<String>,
    /// A person at a terminal on both ends; anything else gets the objects.
    pub interactive: bool,
}

/// Where the preview pane reads from while fzf runs: one text per key,
/// written before fzf starts and removed when it is done.
const PREVIEW_FILE: &str = "browse-preview.json";

pub fn run(ui: &dyn Presenter, store: &Store, opts: &Options, flags: Flags) -> Result<u8, Failure> {
    if let Some(key) = &flags.preview {
        ui.out(&preview_for(store, key));
        return Ok(0);
    }
    let fzf = on_path("fzf");
    // The whole registry is fzf's to filter; the picker of our own stops at
    // the catalog, which is what the issue promised and what fits a screen.
    let all = flags.all && (fzf.is_some() || !flags.interactive);
    if flags.all && !all {
        ui.note("the whole registry needs fzf on PATH; showing the catalog");
    }
    let servers = store.servers()?;
    let list = load(ui, store, opts, all, flags.offline, &servers)?;
    if !flags.interactive {
        ui.json(&list.objects);
        return Ok(0);
    }
    let checked: Vec<bool> = list.items.iter().map(|i| i.installed.is_some()).collect();
    let picked = match fzf {
        Some(fzf) => pick_with_fzf(store, &fzf, &list.items, &checked)?,
        None => pick_here(&list.items, checked)?,
    };
    match picked {
        Some(checked) => apply(ui, store, opts, &list.items, &checked),
        None => {
            ui.err_line("nothing changed");
            Ok(0)
        }
    }
}

// -- the list ---------------------------------------------------------------------

/// One line of the list, whichever list it is.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Item {
    /// The catalog id, or the registry name: what `add` and `--preview` take.
    key: String,
    category: String,
    name: String,
    transport: String,
    auth: String,
    summary: String,
    /// The name it is saved under, when it is.
    installed: Option<String>,
    origin: Origin,
}

#[derive(Debug, Clone, PartialEq)]
enum Origin {
    Catalog(Box<Entry>),
    /// The registry's own object for the server, from the local index.
    Registry(Value),
}

struct List {
    items: Vec<Item>,
    /// What a pipe gets, already serialized: the catalog entries as `catalog
    /// --json` prints them, or the index's server objects.
    objects: String,
}

fn pretty(objects: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(objects).expect("a JSON value is serializable")
}

fn load(
    ui: &dyn Presenter,
    store: &Store,
    opts: &Options,
    all: bool,
    offline: bool,
    servers: &BTreeMap<String, ServerConfig>,
) -> Result<List, Failure> {
    if all {
        let sync = if offline {
            registry::Sync::Offline
        } else {
            registry::Sync::Auto
        };
        let registry = Registry::from_env(opts.timeout_or_default(), &opts.user_agent);
        let mut progress = |n: usize| ui.progress(&format!("fetching the registry: {n} servers"));
        let indexed = registry::index(store, &registry, sync, &mut progress);
        ui.progress_end();
        let (index, note) = indexed?;
        if let Some(note) = note {
            ui.note(&note);
        }
        let items = index
            .servers
            .iter()
            .map(|e| registry_item(&e["server"], servers))
            .collect();
        return Ok(List {
            items,
            objects: pretty(&index.servers),
        });
    }
    let loaded = catalog::load(
        store,
        &catalog::Source::from_env(),
        offline,
        opts.timeout_or_default(),
        &opts.user_agent,
    )?;
    if opts.verbose {
        ui.err_line(&format!("catalog: {}", loaded.origin));
    }
    let items = catalog::grouped(&loaded.entries)
        .into_iter()
        .flat_map(|(_, group)| group)
        .map(|e| catalog_item(e, servers))
        .collect();
    Ok(List {
        items,
        objects: pretty(&loaded.entries),
    })
}

fn catalog_item(e: &Entry, servers: &BTreeMap<String, ServerConfig>) -> Item {
    Item {
        key: e.id.clone(),
        category: e.category.clone(),
        name: e.name.clone(),
        transport: e.transport.as_str().to_string(),
        auth: e.auth.as_str().to_string(),
        summary: e.summary.clone(),
        installed: installed_as(servers, Some(&e.id), e.registry.as_deref()).map(String::from),
        origin: Origin::Catalog(Box::new(e.clone())),
    }
}

fn registry_item(server: &Value, servers: &BTreeMap<String, ServerConfig>) -> Item {
    let name = server["name"].as_str().unwrap_or("?");
    let title = server["title"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .unwrap_or_else(|| short_name(name));
    let transport = match registry::transports(server) {
        t if t.is_empty() => "-".to_string(),
        t => t.join(", "),
    };
    Item {
        key: name.to_string(),
        category: String::new(),
        name: title,
        transport,
        auth: declared_need(server).to_string(),
        summary: server["description"]
            .as_str()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        installed: installed_as(servers, None, Some(name)).map(String::from),
        origin: Origin::Registry(server.clone()),
    }
}

/// What a registry entry says it needs: a required environment variable, a
/// required header, or nothing declared. An OAuth login is not declared, so
/// `-` means "nothing the entry says", not "nothing".
fn declared_need(server: &Value) -> &'static str {
    let required = |list: &Value| {
        list.as_array()
            .into_iter()
            .flatten()
            .any(|x| x["isRequired"] == true)
    };
    let items = |v: &Value| v.as_array().cloned().unwrap_or_default();
    if items(&server["packages"])
        .iter()
        .any(|p| required(&p["environmentVariables"]))
    {
        "env"
    } else if items(&server["remotes"])
        .iter()
        .any(|r| required(&r["headers"]))
    {
        "api-key"
    } else {
        "-"
    }
}

/// The name an entry is saved under, if it is: by the catalog id `add
/// --catalog` records, else by the registry name for an entry that has one.
pub(crate) fn installed_as<'a>(
    servers: &'a BTreeMap<String, ServerConfig>,
    catalog_id: Option<&str>,
    registry_name: Option<&str>,
) -> Option<&'a str> {
    catalog_id
        .and_then(|id| config::saved_from_catalog(servers, id))
        .or_else(|| {
            let wanted = registry_name?;
            servers
                .iter()
                .find(|(_, c)| {
                    c.source
                        .as_ref()
                        .is_some_and(|s| s.registry.as_deref() == Some(wanted))
                })
                .map(|(name, _)| name.as_str())
        })
}

/// The list as lines, aligned: the box, the id, the name, the transport, what
/// it asks for, the summary, and the saved name for one that is installed.
pub(crate) fn render(items: &[Item], checked: &[bool]) -> Vec<String> {
    let width = |pick: fn(&Item) -> &str, cap: usize| {
        items
            .iter()
            .map(|i| pick(i).chars().count().min(cap))
            .max()
            .unwrap_or(0)
    };
    let (kw, nw) = (width(|i| &i.key, 48), width(|i| &i.name, 32));
    let (tw, aw) = (width(|i| &i.transport, 24), width(|i| &i.auth, 7));
    items
        .iter()
        .zip(checked)
        .map(|(i, &on)| {
            let mut line = format!(
                "[{}] {:<kw$}  {:<nw$}  {:<tw$}  {:<aw$}  {}",
                if on { 'x' } else { ' ' },
                crate::truncate_at(&i.key, kw),
                crate::truncate_at(&i.name, nw),
                crate::truncate_at(&i.transport, tw),
                i.auth,
                crate::truncate_at(&i.summary, 80),
            );
            if let Some(saved) = &i.installed {
                line.push_str(&format!("  (saved as {saved})"));
            }
            line.trim_end().to_string()
        })
        .collect()
}

// -- fzf --------------------------------------------------------------------------

/// The list in fzf, installed entries first with their boxes ticked. Marking a
/// row flips it: an installed one is removed, another is added. `None` when fzf
/// was left with Esc.
fn pick_with_fzf(
    store: &Store,
    fzf: &Path,
    items: &[Item],
    checked: &[bool],
) -> Result<Option<Vec<bool>>, Failure> {
    let lines = render(items, checked);
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| !checked[i]);
    let mut input = String::new();
    for &i in &order {
        let item = &items[i];
        // The key, hidden, for the preview pane and for reading the answer
        // back; then the line itself, which is what is shown and what a query
        // is matched against.
        input.push_str(&format!("{}\t{}\n", item.key, lines[i]));
    }
    let previews: BTreeMap<&str, String> = items
        .iter()
        .map(|i| (i.key.as_str(), preview_text(i)))
        .collect();
    let preview_path = store.dir.join(PREVIEW_FILE);
    std::fs::create_dir_all(&store.dir).ok();
    std::fs::write(
        &preview_path,
        serde_json::to_string(&previews).expect("strings serialize"),
    )
    .map_err(|e| Error::config(format!("{}: {e}", preview_path.display())))?;
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "mcpdial".to_string());
    let outcome = Command::new(fzf)
        .args([
            "--multi",
            "--reverse",
            "--delimiter",
            "\t",
            "--with-nth",
            "2",
            "--prompt",
            "browse> ",
            "--header",
            "Tab marks a row, Enter applies, Esc leaves. A marked [x] row is removed, a marked [ ] row is added.",
            "--preview",
            &format!("\"{exe}\" browse --preview {{1}}"),
            "--preview-window",
            "right:50%:wrap",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(input.as_bytes())?;
            child.wait_with_output()
        });
    let _ = std::fs::remove_file(&preview_path);
    let out = outcome.map_err(|e| Error::transport(format!("could not run fzf: {e}")))?;
    match out.status.code() {
        Some(0) => {}
        // 1 is no match, 130 is Esc or ^C: nothing chosen either way.
        Some(1) | Some(130) => return Ok(None),
        code => {
            return Err(Error::transport(format!(
                "fzf failed (exit {})",
                code.map_or("signal".to_string(), |c| c.to_string())
            ))
            .into())
        }
    }
    let mut flipped = checked.to_vec();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let key = line.split('\t').next().unwrap_or("");
        if let Some(i) = items.iter().position(|it| it.key == key) {
            flipped[i] = !flipped[i];
        }
    }
    Ok(Some(flipped))
}

/// What the pane shows for one key, from the file the parent wrote.
fn preview_for(store: &Store, key: &str) -> String {
    let path = store.dir.join(PREVIEW_FILE);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<BTreeMap<String, String>>(&text).ok())
        .and_then(|mut previews| previews.remove(key))
        .unwrap_or_else(|| format!("no preview for {key}\n"))
}

/// The pane: the summary, the transport, what adding it will ask for, and the
/// `add` command it stands for.
fn preview_text(item: &Item) -> String {
    let mut out = String::new();
    match &item.origin {
        Origin::Catalog(e) => {
            out.push_str(&format!(
                "{}  ({})\n{}\n{}\n\n",
                e.name, e.id, e.category, e.summary
            ));
            out.push_str(&format!("transport: {}\n", e.transport.as_str()));
            let asks = match e.auth {
                catalog::Auth::None => "nothing".to_string(),
                catalog::Auth::OAuth => {
                    format!("a login in the browser once saved: mcpdial login {}", e.id)
                }
                catalog::Auth::ApiKey => {
                    "an API key, in a header or an environment variable".to_string()
                }
                catalog::Auth::Env => {
                    "environment: a connection string, a host, a user and password".to_string()
                }
            };
            out.push_str(&format!("asks for:  {asks}\n"));
            if let Some(c) = &e.config {
                out.push_str(&format!("{}: {}\n", verb(c), c.location()));
                let vars: Vec<&str> = c.env.keys().map(String::as_str).collect();
                if !vars.is_empty() {
                    out.push_str(&format!("environment: {}\n", vars.join(", ")));
                }
            } else if let Some(name) = &e.registry {
                out.push_str(&format!("registry:  {name}\n"));
            }
            out.push_str(&format!("\nmcpdial add {} --catalog {}\n", e.id, e.id));
        }
        Origin::Registry(server) => {
            let name = server["name"].as_str().unwrap_or("?");
            out.push_str(&format!(
                "{}  {}\n",
                name,
                server["version"].as_str().unwrap_or("")
            ));
            if let Some(d) = server["description"].as_str() {
                out.push_str(&format!("{}\n", d.trim()));
            }
            out.push('\n');
            out.push_str(&format!("transports: {}\n", item.transport));
            match registry::convert(server, &Pick::Any, &[]) {
                Ok(r) => {
                    out.push_str(&format!("{}: {}\n", verb(&r.config), r.config.location()));
                    for note in &r.notes {
                        out.push_str(&format!("\n{note}\n"));
                    }
                }
                Err(e) => out.push_str(&format!("cannot be added: {e}\n")),
            }
            out.push_str(&format!(
                "\nmcpdial add {} --registry {}\n",
                short_name(name),
                name
            ));
        }
    }
    out
}

fn verb(c: &ServerConfig) -> &'static str {
    if c.stdio.is_some() {
        "runs"
    } else {
        "dials"
    }
}

// -- the picker of our own --------------------------------------------------------

/// One key the picker understands, whatever produced it.
#[cfg(any(feature = "rich", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Up,
    Down,
    Space,
    Enter,
    Esc,
    Backspace,
    Char(char),
}

#[cfg(any(feature = "rich", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Apply,
    Leave,
}

/// One line on the screen: a category, or the row of one item.
#[cfg(any(feature = "rich", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Line {
    Heading(String),
    Row(usize),
}

/// The checklist's state, with no terminal in it so it can be driven by tests.
#[cfg(any(feature = "rich", test))]
#[derive(Debug)]
pub(crate) struct Picker<'a> {
    items: &'a [Item],
    pub(crate) checked: Vec<bool>,
    /// The item under the cursor, as an index into `shown`.
    cursor: usize,
    filter: String,
    /// Keys go into the filter rather than moving the cursor.
    filtering: bool,
}

#[cfg(any(feature = "rich", test))]
impl<'a> Picker<'a> {
    pub(crate) fn new(items: &'a [Item], checked: Vec<bool>) -> Self {
        Self {
            items,
            checked,
            cursor: 0,
            filter: String::new(),
            filtering: false,
        }
    }

    /// The items the filter lets through, in list order.
    pub(crate) fn shown(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                needle.is_empty()
                    || format!("{} {} {} {}", it.key, it.name, it.category, it.summary)
                        .to_lowercase()
                        .contains(&needle)
            })
            .collect()
    }

    /// The screen's lines: each category once, above its rows.
    pub(crate) fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        let mut last: Option<&str> = None;
        for i in self.shown() {
            let category = self.items[i].category.as_str();
            if !category.is_empty() && last != Some(category) {
                out.push(Line::Heading(category.to_string()));
                last = Some(category);
            }
            out.push(Line::Row(i));
        }
        out
    }

    pub(crate) fn current(&self) -> Option<usize> {
        self.shown().get(self.cursor).copied()
    }

    pub(crate) fn filter(&self) -> &str {
        &self.filter
    }

    pub(crate) fn filtering(&self) -> bool {
        self.filtering
    }

    pub(crate) fn press(&mut self, key: Key) -> Option<Outcome> {
        if self.filtering {
            match key {
                Key::Enter | Key::Esc => self.filtering = false,
                Key::Backspace => {
                    self.filter.pop();
                }
                Key::Char(c) => self.filter.push(c),
                Key::Up | Key::Down | Key::Space => {
                    self.filtering = false;
                    return self.press(key);
                }
            }
            self.cursor = self.cursor.min(self.shown().len().saturating_sub(1));
            return None;
        }
        match key {
            Key::Up | Key::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                self.cursor = (self.cursor + 1).min(self.shown().len().saturating_sub(1))
            }
            Key::Space => {
                if let Some(i) = self.current() {
                    self.checked[i] = !self.checked[i];
                }
            }
            Key::Char('/') => self.filtering = true,
            Key::Enter => return Some(Outcome::Apply),
            Key::Esc | Key::Char('q') => return Some(Outcome::Leave),
            Key::Backspace => {
                self.filter.pop();
            }
            Key::Char(_) => {}
        }
        None
    }
}

#[cfg(feature = "rich")]
fn pick_here(items: &[Item], checked: Vec<bool>) -> Result<Option<Vec<bool>>, Failure> {
    picker::run(items, checked).map_err(|e| Error::transport(format!("terminal: {e}")).into())
}

#[cfg(not(feature = "rich"))]
fn pick_here(_: &[Item], _: Vec<bool>) -> Result<Option<Vec<bool>>, Failure> {
    Err(Failure::hinted(
        Error::usage("browse needs fzf on PATH, or a build with the rich feature"),
        "`mcpdial catalog` lists the entries; `mcpdial add NAME --catalog ID` saves one",
    ))
}

/// The screen: the checklist drawn on stderr in raw mode, in the alternate
/// screen, until Enter or Esc.
#[cfg(feature = "rich")]
mod picker {
    use super::{render, Item, Key, Line, Outcome, Picker};
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::style::{Attribute, Print, SetAttribute};
    use crossterm::terminal::{self, Clear, ClearType};
    use crossterm::{cursor, execute, queue};
    use std::io::{self, Write};

    /// Raw mode and the alternate screen, undone however the picker ends.
    struct Screen;

    impl Screen {
        fn enter() -> io::Result<Self> {
            terminal::enable_raw_mode()?;
            let mut err = io::stderr();
            if let Err(e) = execute!(err, terminal::EnterAlternateScreen, cursor::Hide) {
                let _ = terminal::disable_raw_mode();
                return Err(e);
            }
            Ok(Self)
        }
    }

    impl Drop for Screen {
        fn drop(&mut self) {
            let _ = execute!(io::stderr(), cursor::Show, terminal::LeaveAlternateScreen);
            let _ = terminal::disable_raw_mode();
        }
    }

    pub(super) fn run(items: &[Item], checked: Vec<bool>) -> io::Result<Option<Vec<bool>>> {
        let _screen = Screen::enter()?;
        let mut picker = Picker::new(items, checked);
        let mut top = 0;
        loop {
            draw(&picker, &mut top)?;
            let Some(key) = next_key()? else {
                return Ok(None);
            };
            match picker.press(key) {
                Some(Outcome::Apply) => return Ok(Some(picker.checked)),
                Some(Outcome::Leave) => return Ok(None),
                None => {}
            }
        }
    }

    /// The next key a person pressed; `None` for ^C.
    fn next_key() -> io::Result<Option<Key>> {
        loop {
            let Event::Key(k) = event::read()? else {
                continue;
            };
            if k.kind == KeyEventKind::Release {
                continue;
            }
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            return Ok(Some(match k.code {
                KeyCode::Char('c') if ctrl => return Ok(None),
                // A line feed typed before raw mode was on reaches us as ^J.
                KeyCode::Char('j') if ctrl => Key::Enter,
                KeyCode::Up => Key::Up,
                KeyCode::Down => Key::Down,
                KeyCode::Char(' ') => Key::Space,
                KeyCode::Enter => Key::Enter,
                KeyCode::Esc => Key::Esc,
                KeyCode::Backspace => Key::Backspace,
                KeyCode::Char(c) => Key::Char(c),
                _ => continue,
            }));
        }
    }

    fn draw(picker: &Picker, top: &mut usize) -> io::Result<()> {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let (cols, rows) = (
            if cols == 0 { 80 } else { cols as usize },
            if rows == 0 { 24 } else { rows as usize },
        );
        let height = rows.saturating_sub(2).max(1);
        let lines = picker.lines();
        let rendered = render(picker.items, &picker.checked);
        let at = picker
            .current()
            .and_then(|i| lines.iter().position(|l| *l == Line::Row(i)));
        if let Some(at) = at {
            if at < *top {
                *top = at;
            } else if at >= *top + height {
                *top = at + 1 - height;
            }
        }
        let fit = |s: &str| -> String { s.chars().take(cols).collect() };
        let mut err = io::stderr();
        queue!(
            err,
            cursor::MoveTo(0, 0),
            Clear(ClearType::All),
            SetAttribute(Attribute::Bold),
            Print(fit(
                "browse   Space ticks, / filters, Enter applies, Esc leaves"
            )),
            SetAttribute(Attribute::Reset)
        )?;
        for (y, line) in lines.iter().skip(*top).take(height).enumerate() {
            queue!(err, cursor::MoveTo(0, (y + 1) as u16))?;
            match line {
                Line::Heading(h) => queue!(
                    err,
                    SetAttribute(Attribute::Bold),
                    Print(fit(h)),
                    SetAttribute(Attribute::Reset)
                )?,
                Line::Row(i) => {
                    let text = fit(&format!("  {}", rendered[*i]));
                    if picker.current() == Some(*i) {
                        queue!(
                            err,
                            SetAttribute(Attribute::Reverse),
                            Print(text),
                            SetAttribute(Attribute::Reset)
                        )?;
                    } else {
                        queue!(err, Print(text))?;
                    }
                }
            }
        }
        let status = if picker.filtering() || !picker.filter().is_empty() {
            format!("/{}", picker.filter())
        } else {
            format!(
                "{} ticked, {} shown",
                picker.checked.iter().filter(|&&c| c).count(),
                picker.shown().len()
            )
        };
        queue!(
            err,
            cursor::MoveTo(0, (rows - 1) as u16),
            Print(fit(&status))
        )?;
        err.flush()
    }
}

// -- applying ---------------------------------------------------------------------

/// Save every newly ticked entry, dial them together and show their rows;
/// remove every unticked one after a confirmation.
fn apply(
    ui: &dyn Presenter,
    store: &Store,
    opts: &Options,
    items: &[Item],
    checked: &[bool],
) -> Result<u8, Failure> {
    let to_add: Vec<&Item> = items
        .iter()
        .zip(checked)
        .filter(|(i, &on)| on && i.installed.is_none())
        .map(|(i, _)| i)
        .collect();
    let to_remove: Vec<&str> = items
        .iter()
        .zip(checked)
        .filter_map(|(i, &on)| (!on).then_some(i.installed.as_deref()).flatten())
        .collect();
    if to_add.is_empty() && to_remove.is_empty() {
        ui.err_line("nothing changed");
        return Ok(0);
    }

    let registry = Registry::from_env(opts.timeout_or_default(), &opts.user_agent);
    let mut ask = |label: &str| ask(ui, label);
    let mut saved = Vec::new();
    let mut failed = false;
    for item in to_add {
        match save_one(ui, store, &registry, item, &mut ask) {
            Ok(name) => saved.push(name),
            Err(f) => {
                f.report(ui);
                failed = true;
            }
        }
    }
    if !saved.is_empty() {
        let rows = client::listing_named(store, opts, &saved)?;
        ui.table(
            &LISTING_HEADERS,
            &rows.iter().map(listing_row).collect::<Vec<_>>(),
        );
        for row in rows.iter().filter(|r| r.status == Status::AuthRequired) {
            ui.err_line(&format!("mcpdial login {}", row.name));
        }
    }
    if !to_remove.is_empty() {
        let answer = ask(&format!("remove {}? [y/N] ", to_remove.join(", ")));
        if answer.is_some_and(|a| a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")) {
            for name in to_remove {
                store.remove_server(name)?;
                ui.err_line(&format!("removed {name}"));
            }
        } else {
            ui.err_line("kept them");
        }
    }
    Ok(if failed { EXIT_ERROR } else { 0 })
}

/// One entry: converted, its blanks asked for, saved under its own id, or
/// `id-2` when that is taken. Returns the saved name.
fn save_one(
    ui: &dyn Presenter,
    store: &Store,
    registry: &Registry,
    item: &Item,
    ask: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<String, Failure> {
    let convert = |given: &[Option<String>]| -> Result<Resolved, Error> {
        match &item.origin {
            Origin::Catalog(e) => {
                catalog::convert_given(e, catalog::lookup(e, registry)?.as_ref(), given)
            }
            Origin::Registry(server) => registry::convert_given(server, &Pick::Any, given),
        }
    };
    let mut resolved = convert(&[])?;
    if !resolved.missing.is_empty() {
        ui.err_line(&format!("{} leaves these to you:", item.key));
        let given = prompted_values(&resolved.slots, &resolved.missing, ask)?;
        resolved = convert(&given)?;
    }
    prompt_env(
        &mut resolved.config,
        |var| std::env::var_os(var).is_some(),
        ask,
    );
    let wanted = match &item.origin {
        Origin::Catalog(e) => e.id.clone(),
        Origin::Registry(server) => short_name(server["name"].as_str().unwrap_or("server")),
    };
    let name = free_name(&store.servers()?, &wanted);
    let summary = format!("{} {}", resolved.config.kind(), resolved.config.location());
    store.add_server(&name, resolved.config)?;
    ui.err_line(&format!("saved {name} ({summary})"));
    for note in &resolved.notes {
        ui.note(note);
    }
    Ok(name)
}

/// One answer from the person at the terminal: the label on stderr, a line
/// from stdin, trimmed. `None` at the end of input.
fn ask(ui: &dyn Presenter, label: &str) -> Option<String> {
    ui.err(label);
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(n) if n > 0 => Some(line.trim().to_string()),
        _ => {
            ui.err_line("");
            None
        }
    }
}

/// A value for every slot, asked for in order: a required one until it is
/// answered, an optional one once, where nothing means its default.
pub(crate) fn prompted_values(
    slots: &[String],
    missing: &[String],
    ask: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<Vec<Option<String>>, Failure> {
    let mut given = Vec::with_capacity(slots.len());
    for slot in slots {
        let required = missing.contains(slot);
        let value = loop {
            let answer = ask(&format!("  {slot}: "))
                .ok_or_else(|| Error::usage(format!("no value for {slot}")))?;
            if !answer.is_empty() {
                break Some(answer);
            }
            if !required {
                break None;
            }
        };
        given.push(value);
    }
    Ok(given)
}

/// Ask for each required variable saved as a bare `${VAR}` placeholder that the
/// environment does not hold. Nothing typed keeps the placeholder, to be set
/// before the server is dialed.
pub(crate) fn prompt_env(
    cfg: &mut ServerConfig,
    is_set: impl Fn(&str) -> bool,
    ask: &mut dyn FnMut(&str) -> Option<String>,
) {
    for (var, value) in cfg.env.iter_mut() {
        if *value != format!("${{{var}}}") || is_set(var) {
            continue;
        }
        let label = format!("  {var} (required; enter keeps ${{{var}}} to set later): ");
        if let Some(answer) = ask(&label).filter(|a| !a.is_empty()) {
            *value = answer;
        }
    }
}

/// `wanted`, or `wanted-2`, `wanted-3`... when that name is taken.
pub(crate) fn free_name(taken: &BTreeMap<String, ServerConfig>, wanted: &str) -> String {
    if !taken.contains_key(wanted) {
        return wanted.to_string();
    }
    (2..)
        .map(|n| format!("{wanted}-{n}"))
        .find(|c| !taken.contains_key(c))
        .expect("some suffix is free")
}

/// A saved name from a registry name: what follows the last `/`, lowercased,
/// anything but letters, digits, dots, dashes and underscores made a dash.
pub(crate) fn short_name(registry_name: &str) -> String {
    let tail = registry_name.rsplit('/').next().unwrap_or(registry_name);
    let name: String = tail
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() {
        "server".to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcpdial::catalog::{Auth, Transport};
    use serde_json::json;

    fn entry(id: &str, category: &str, registry: Option<&str>) -> Entry {
        Entry {
            id: id.into(),
            name: id.to_uppercase(),
            category: category.into(),
            summary: format!("The {id} server"),
            registry: registry.map(String::from),
            config: registry
                .is_none()
                .then(|| ServerConfig::http("https://x/mcp")),
            transport: Transport::Http,
            auth: Auth::None,
        }
    }

    fn saved_from(catalog: Option<&str>, registry: Option<&str>) -> ServerConfig {
        let mut c = ServerConfig::http("https://x/mcp");
        c.source = Some(config::Source {
            catalog: catalog.map(String::from),
            registry: registry.map(String::from),
            version: None,
        });
        c
    }

    #[test]
    fn an_entry_is_installed_by_its_catalog_id_or_its_registry_name() {
        let servers = BTreeMap::from([
            (
                "gh".to_string(),
                saved_from(Some("github"), Some("io.github.github/server")),
            ),
            (
                "ctx".to_string(),
                saved_from(None, Some("io.github.upstash/context7")),
            ),
            ("plain".to_string(), ServerConfig::http("https://x/mcp")),
        ]);
        assert_eq!(installed_as(&servers, Some("github"), None), Some("gh"));
        assert_eq!(
            installed_as(
                &servers,
                Some("context7"),
                Some("io.github.upstash/context7")
            ),
            Some("ctx"),
            "added with --registry, so by its registry name"
        );
        assert_eq!(
            installed_as(&servers, None, Some("io.github.github/server")),
            Some("gh")
        );
        assert_eq!(
            installed_as(&servers, Some("gitlab"), Some("com.gitlab/mcp")),
            None
        );
        assert_eq!(installed_as(&servers, None, None), None);

        let items = [
            catalog_item(
                &entry("github", "Source control", Some("io.github.github/server")),
                &servers,
            ),
            catalog_item(&entry("deepwiki", "Docs and search", None), &servers),
        ];
        assert_eq!(items[0].installed.as_deref(), Some("gh"));
        assert_eq!(items[1].installed, None);
    }

    #[test]
    fn rows_align_their_columns_and_name_where_an_installed_one_is_saved() {
        let servers = BTreeMap::from([("gh".to_string(), saved_from(Some("github"), None))]);
        let items = [
            catalog_item(
                &entry("github", "Source control", Some("io.github.github/server")),
                &servers,
            ),
            catalog_item(&entry("deepwiki", "Docs and search", None), &servers),
        ];
        let checked: Vec<bool> = items.iter().map(|i| i.installed.is_some()).collect();
        assert_eq!(
            render(&items, &checked),
            [
                "[x] github    GITHUB    http  none  The github server  (saved as gh)",
                "[ ] deepwiki  DEEPWIKI  http  none  The deepwiki server",
            ]
        );
        assert_eq!(
            render(&items, &[false, true])[1],
            "[x] deepwiki  DEEPWIKI  http  none  The deepwiki server",
            "the box follows the state, the tag follows the file"
        );
    }

    #[test]
    fn a_registry_row_reads_its_transports_and_what_it_declares() {
        let server = json!({
            "name": "io.github.acme/files", "version": "1.2.0", "title": "Acme Files",
            "description": "Serve one directory.\nMore.",
            "packages": [{"registryType": "npm", "identifier": "@acme/files", "version": "1.2.0",
                          "transport": {"type": "stdio"},
                          "environmentVariables": [{"name": "ACME_TOKEN", "isRequired": true}]}]
        });
        let item = registry_item(&server, &BTreeMap::new());
        assert_eq!(
            (
                item.key.as_str(),
                item.name.as_str(),
                item.transport.as_str(),
                item.auth.as_str()
            ),
            ("io.github.acme/files", "Acme Files", "stdio (npm)", "env")
        );
        assert_eq!(item.summary, "Serve one directory.");
        let preview = preview_text(&item);
        assert!(
            preview.contains("runs: npx -y @acme/files@1.2.0"),
            "{preview}"
        );
        assert!(preview.contains("ACME_TOKEN (required)"), "{preview}");
        assert!(
            preview.ends_with("mcpdial add files --registry io.github.acme/files\n"),
            "{preview}"
        );

        let remote = json!({"name": "x/y", "remotes": [{"type": "streamable-http", "url": "https://y/mcp",
                             "headers": [{"name": "Authorization", "isRequired": true}]}]});
        assert_eq!(declared_need(&remote), "api-key");
        assert_eq!(declared_need(&json!({"name": "x/z"})), "-");
        assert_eq!(
            registry_item(&json!({"name": "io.github.a/Big Server"}), &BTreeMap::new()).name,
            "big-server"
        );
    }

    #[test]
    fn the_catalog_preview_names_the_add_command_and_what_it_asks_for() {
        let mut e = entry("github", "Source control", Some("io.github.github/server"));
        e.auth = Auth::OAuth;
        let text = preview_text(&catalog_item(&e, &BTreeMap::new()));
        assert!(
            text.starts_with("GITHUB  (github)\nSource control\nThe github server\n"),
            "{text}"
        );
        assert!(
            text.contains("asks for:  a login in the browser once saved: mcpdial login github"),
            "{text}"
        );
        assert!(
            text.ends_with("\nmcpdial add github --catalog github\n"),
            "{text}"
        );

        let mut cfg = ServerConfig::stdio("uvx postgres-mcp");
        cfg.env
            .insert("DATABASE_URI".into(), "${DATABASE_URI}".into());
        let mut e = entry("postgres", "Databases", None);
        e.config = Some(cfg);
        e.transport = Transport::Stdio;
        e.auth = Auth::Env;
        let text = preview_text(&catalog_item(&e, &BTreeMap::new()));
        assert!(
            text.contains("runs: uvx postgres-mcp\nenvironment: DATABASE_URI\n"),
            "{text}"
        );
    }

    #[test]
    fn the_picker_moves_ticks_filters_and_leaves() {
        let servers = BTreeMap::new();
        let items = [
            catalog_item(&entry("github", "Source control", None), &servers),
            catalog_item(&entry("gitlab", "Source control", None), &servers),
            catalog_item(&entry("deepwiki", "Docs and search", None), &servers),
        ];
        let mut p = Picker::new(&items, vec![false; 3]);
        assert_eq!(
            p.lines(),
            [
                Line::Heading("Source control".into()),
                Line::Row(0),
                Line::Row(1),
                Line::Heading("Docs and search".into()),
                Line::Row(2)
            ]
        );
        assert_eq!(p.current(), Some(0));
        assert_eq!(p.press(Key::Up), None);
        assert_eq!(p.current(), Some(0), "the top stays the top");
        p.press(Key::Down);
        p.press(Key::Char('j'));
        p.press(Key::Down);
        assert_eq!(p.current(), Some(2), "and the bottom the bottom");
        p.press(Key::Space);
        assert_eq!(p.checked, [false, false, true]);

        p.press(Key::Char('/'));
        for c in "GIT".chars() {
            p.press(Key::Char(c));
        }
        assert!(p.filtering());
        assert_eq!(p.shown(), [0, 1], "case aside");
        assert_eq!(
            p.current(),
            Some(1),
            "the cursor stays inside what is shown"
        );
        p.press(Key::Backspace);
        p.press(Key::Enter);
        assert!(!p.filtering() && p.filter() == "GI");
        p.press(Key::Space);
        assert_eq!(p.checked, [false, true, true]);
        assert_eq!(p.press(Key::Enter), Some(Outcome::Apply));
        assert_eq!(p.press(Key::Esc), Some(Outcome::Leave));
        assert_eq!(p.press(Key::Char('q')), Some(Outcome::Leave));
    }

    #[test]
    fn prompts_ask_for_every_slot_in_order_and_insist_on_the_required_ones() {
        let slots = vec![
            "--port (optional): Port to listen on".to_string(),
            "root (required): Directory to serve".to_string(),
        ];
        let missing = vec![slots[1].clone()];
        let mut answers = vec!["", "", "/srv"].into_iter();
        let mut asked = Vec::new();
        let given = prompted_values(&slots, &missing, &mut |label: &str| {
            asked.push(label.to_string());
            answers.next().map(String::from)
        })
        .unwrap_or_else(|f| panic!("{}", f.error));
        assert_eq!(given, [None, Some("/srv".to_string())]);
        assert_eq!(
            asked,
            [
                "  --port (optional): Port to listen on: ",
                "  root (required): Directory to serve: ",
                "  root (required): Directory to serve: "
            ]
        );
        let Err(ended) = prompted_values(&slots, &missing, &mut |_: &str| None) else {
            panic!("the end of input is an error")
        };
        assert!(ended.error.to_string().contains("no value for --port"));

        let mut cfg = ServerConfig::stdio("x");
        cfg.env.insert("A".into(), "${A}".into());
        cfg.env.insert("B".into(), "${B}".into());
        cfg.env.insert("C".into(), "${C:-1}".into());
        cfg.env.insert("D".into(), "${D}".into());
        let mut asked = Vec::new();
        let mut answers = vec!["typed", ""].into_iter();
        prompt_env(&mut cfg, |var| var == "B", &mut |label: &str| {
            asked.push(label.to_string());
            answers.next().map(String::from)
        });
        assert_eq!(cfg.env["A"], "typed");
        assert_eq!(cfg.env["B"], "${B}", "set in the environment, not asked");
        assert_eq!(cfg.env["C"], "${C:-1}", "has a default, not asked");
        assert_eq!(cfg.env["D"], "${D}", "nothing typed keeps the placeholder");
        assert_eq!(asked.len(), 2);
    }

    #[test]
    fn names_come_from_the_id_with_a_suffix_on_a_clash() {
        let taken = BTreeMap::from([
            ("github".to_string(), ServerConfig::http("https://a/mcp")),
            ("github-2".to_string(), ServerConfig::http("https://b/mcp")),
        ]);
        assert_eq!(free_name(&taken, "gitlab"), "gitlab");
        assert_eq!(free_name(&taken, "github"), "github-3");
        assert_eq!(short_name("io.github.upstash/context7"), "context7");
        assert_eq!(short_name("com.example/My Server!"), "my-server");
        assert_eq!(short_name("//"), "server");
    }
}
