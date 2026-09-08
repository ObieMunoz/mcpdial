//! `shell`: one session, many commands, and the state that survives between them.
//!
//! Every other command is a whole session - connect, `initialize`, do one thing,
//! exit - which is the right shape for a program and the wrong one for a server
//! holding a browser, a cursor or a REPL. This keeps one session open and reads
//! commands against it, so what the last command left behind is still there for
//! the next one.

mod command;
mod complete;
mod edit;
mod follow;
pub(crate) mod help;
mod history;
mod input;
mod status;

use crate::cmd::{elicitation, name_and_args, Ctx};
use crate::diagnose::{
    advertises, closest, find_tool, missing_capability, missing_item, shell_word, tool_usage,
};
use crate::failure::{refuse_denied, Failure, EXIT_ERROR};
use crate::media::{emit_rendered, emit_resource, file_stem, rendered, resource_stem, MediaFiles};
use crate::notices::Notices;
use crate::path::Filter;
use crate::present::{truncate_at, Health};
use crate::render::{listed, print_prompts, print_tools, print_value};
use crate::validate::parse_object;
use crate::{args, brief, path};
use command::{
    names_a_result, no_such_tool, print_rerun, shell_call, shell_call_hint, shell_failed,
    shell_fill, shell_filter, shell_named, shell_save, split_filter, split_word, takes_a_filter,
    with_filter, Live,
};
use edit::{edit_arguments, edit_target};
use follow::{drain_subscriptions, listen_bound, start_following, subscription_target};
use help::{
    EDIT_USAGE, FILTER_APPLIES_TO, RETRY_USAGE, SAVE_USAGE, SHELL_COMMANDS, SHELL_HELP,
    SHELL_SUMMARY, SHOW_USAGE,
};
use history::{History, Origin};
use input::{shell_tools, Input, Lent, Lists};
use mcpdial::notify::Notice;
use mcpdial::session::{render_messages, Watcher};
use mcpdial::subscribe::{Mechanism, Subscriptions};
use mcpdial::{client, Error};
use serde_json::{json, Value};
use status::{connected_line, dropped_the_session, expiring_line, Says};
use std::io::IsTerminal;
use std::rc::Rc;

/// One shell request with somebody listening to what the server says on the
/// way, and the updating line finished before whatever prints next, however it
/// went.
///
/// A `list_changed` or an update to a followed resource arrives in the middle
/// of a call's output, where acting on it would put a line of ours between two
/// lines of the server's. So it is put aside here and acted on at the prompt,
/// where there is nothing to interrupt. Everything else a server says on the
/// way is drawn exactly as it is drawn everywhere else.
fn shell_watched<T>(
    notices: &mut Notices<'_>,
    subs: &mut Subscriptions,
    request: impl FnOnce(&mut dyn Watcher) -> Result<T, Error>,
) -> Result<T, Error> {
    let outcome = request(&mut ShellWatch { notices, subs });
    notices.finish();
    outcome
}

struct ShellWatch<'a, 'u> {
    notices: &'a mut Notices<'u>,
    subs: &'a mut Subscriptions,
}

impl Watcher for ShellWatch<'_, '_> {
    fn wants_progress(&self) -> bool {
        self.notices.wants_progress()
    }

    fn interrupted(&mut self) {
        self.notices.interrupted();
    }

    fn notice(&mut self, notice: &Notice<'_>) {
        if !self.subs.record(notice) {
            self.notices.notice(notice);
        }
    }
}

pub(crate) fn run(cx: &mut Ctx<'_>, target: String, no_browser: bool) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &mut cx.opts;
    let out = &cx.out;
    let notices = &mut cx.notices;
    let save_dir = cx.save_dir.as_deref();
    let json = cx.json;
    opts.elicit = elicitation(ui, None, no_browser, json, true)?;
    let r = client::resolve(store, &target)?;
    let mut conn = client::connect(store, &r, opts)?;
    let si = &conn.server_info["serverInfo"];
    let interactive = std::io::stdin().is_terminal();
    // A saved server is prompted by its own name. An ad-hoc target is a
    // whole URL or command line, which makes a prompt that wraps the
    // terminal, so use what the server calls itself instead.
    let label = if r.saved {
        r.name.clone()
    } else {
        si["name"]
            .as_str()
            .map_or_else(|| truncate_at(&r.name, 24), String::from)
    };
    let mut input = Input::open(store, &r, &label, ui, interactive)?;
    let mut failures = 0u32;
    // Each list is fetched at most once, so a mistake can be answered
    // with the shape the server actually wants.
    let mut lists = Lists::default();
    // The session, for the length of a Tab and no longer; see [`Lent`].
    let lent = Lent::default();
    // What this session is following, and what has changed under it.
    let mut subs = Subscriptions::new(conn.session.version());
    let says = Says::choose(ui, json);
    if matches!(input, Input::Tty { .. }) {
        // One eager fetch: it gives Tab something to complete and warms
        // the same cache the hints read.
        let tools = shell_tools(&mut lists.tools, &mut conn).to_vec();
        if advertises(&conn.server_info, "resources") {
            lists.resources = conn.session.list_resources().ok();
            lists.templates = conn.session.list_resource_templates().ok();
        }
        if advertises(&conn.server_info, "prompts") {
            lists.prompts = conn.session.list_prompts().ok();
        }
        input.set_completions(
            &tools,
            lists.resources(),
            lists.templates(),
            lists.prompts(),
        );
        if advertises(&conn.server_info, "completions") {
            input.suggestions_from(Rc::new(lent.clone()));
        }
    }
    if interactive {
        ui.err_line(&connected_line(&conn.server_info, &lists));
    }
    // When the credential this session is holding runs out, so that the
    // prompt can say so before a call fails on it. Read once: a token
    // is not renewed under a session that is already using it.
    let token_expires_at = matches!(conn.auth, client::AuthUsed::Saved)
        .then(|| store.credential(&r.name).ok().flatten())
        .flatten()
        .and_then(|cred| cred.expires_at);
    // Whether the transport dropped under the last command, and whether
    // the token running out has been mentioned; each state is worth one
    // line, and the dot in the prompt carries it from then on.
    let mut dropped = false;
    let mut said_expiring = false;
    // Whether there is a person here to be shown a state and to wait
    // for a session to be dialed again. A piped shell keeps today's
    // behaviour to the letter, down to a stdio server that died staying
    // dead rather than being restarted under a script that would then
    // be talking to a process with none of the state it had built up.
    let at_a_terminal = matches!(input, Input::Tty { .. });
    let info_cmd = "`info`";
    // Every result this session prints, numbered, so a later line can
    // name one instead of running it again.
    let mut results = History::default();
    // The line a `retry` or an `edit` made, which stands in for one
    // nobody typed: the loop reads it before asking for another.
    let mut pending: Option<String> = None;
    loop {
        let health = Health::read(dropped, token_expires_at, mcpdial::config::now());
        if health == Health::Expiring && !std::mem::replace(&mut said_expiring, true) {
            let left = token_expires_at
                .unwrap_or_default()
                .saturating_sub(mcpdial::config::now());
            says.about_the_session(ui, "expiring", &expiring_line(&r, left));
        }
        let raw = match pending.take() {
            Some(line) => line,
            None => {
                // The session goes on loan to Tab for the length of the
                // read and comes straight back; see [`Lent`].
                let (returned, read) = lent.reading(conn, || input.next(ui, health));
                conn = returned;
                match read? {
                    Some(line) => line,
                    None => break,
                }
            }
        };
        let text = raw.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        // A `| ...` at the end of the line filters what that line
        // prints. It is read before the command in front of it runs, so
        // an expression nobody can read costs no request.
        let (text, expression) = split_filter(text);
        let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let rest = rest.trim();
        let long = rest == "--long" || rest == "-l";
        let filter = match expression.map(Filter::parse).transpose() {
            Ok(filter) => filter,
            Err(e) => {
                failures += 1;
                shell_failed(ui, &Failure::hinted(e, path::USAGE), json);
                continue;
            }
        };
        if filter.is_some() && !takes_a_filter(word) {
            failures += 1;
            let refused = Failure::hinted(
                Error::usage(match word.is_empty() {
                    true => "a filter needs a command in front of it".to_string(),
                    false => format!("{word} prints no result to filter"),
                }),
                FILTER_APPLIES_TO,
            );
            shell_failed(ui, &refused, json);
            continue;
        }
        // A session that dropped is dialed again here rather than where
        // it dropped: reconnecting under a prompt nobody has typed at
        // yet spends a person's wait on a session they may be about to
        // leave, and `quit` needs no server at all.
        if dropped && at_a_terminal && !matches!(word, "quit" | "exit") {
            match client::connect(store, &r, opts) {
                Ok(mut fresh) => {
                    // What the session had been told to answer with, and
                    // what it had been told to follow, were asked for by
                    // the person at the prompt: a new socket under them
                    // does not unask either.
                    fresh.elicit = conn.elicit.clone();
                    conn = fresh;
                    dropped = false;
                    let following: Vec<String> =
                        subs.following().map(|(uri, _)| uri.clone()).collect();
                    for uri in following {
                        start_following(&mut conn, subs.mechanism(), &uri, info_cmd).ok();
                    }
                    says.about_the_session(ui, "reconnected", &format!("reconnected to {label}"));
                }
                Err(e) => {
                    failures += 1;
                    shell_failed(ui, &Failure::from(e), json);
                    continue;
                }
            }
        }
        let outcome: Result<(), Failure> = match word {
            "quit" | "exit" => break,
            "help" if rest.is_empty() => {
                ui.err_line(SHELL_HELP);
                Ok(())
            }
            "help" => match refuse_denied(&r.config, &r.name, rest) {
                Err(denied) => Err(denied),
                Ok(()) => {
                    let tools = shell_tools(&mut lists.tools, &mut conn);
                    match find_tool(tools, rest) {
                        Some(t) => {
                            ui.err_line(&tool_usage(t, "call", ""));
                            Ok(())
                        }
                        None => Err(no_such_tool(tools, rest)),
                    }
                }
            },
            "schema" if rest.is_empty() => Err(Failure::hinted(
                Error::usage("schema needs a tool name"),
                "usage: schema TOOL   (`tools` lists what this server offers)",
            )),
            "schema" => match refuse_denied(&r.config, &r.name, rest) {
                Err(denied) => Err(denied),
                Ok(()) => {
                    let tools = shell_tools(&mut lists.tools, &mut conn);
                    match find_tool(tools, rest) {
                        Some(t) => {
                            ui.paged(|| print_value(ui, t, json));
                            Ok(())
                        }
                        None => Err(no_such_tool(tools, rest)),
                    }
                }
            },
            "info" => {
                ui.paged(|| print_value(ui, &conn.server_info, json));
                Ok(())
            }
            "tools" => conn.list_tools().map_err(Failure::from).map(|tools| {
                if json {
                    print_value(ui, &json!({ "tools": brief::tools(&tools, long) }), true);
                } else {
                    listed(ui, long, || {
                        ui.line(&format!("{} tool(s):", tools.len()));
                        print_tools(ui, &tools, long);
                    });
                }
                lists.tools = Some(tools);
            }),
            "resources" => match conn.session.list_resources() {
                Err(e) => Err(missing_capability(e, "resources", info_cmd)),
                Ok(found) => {
                    let listed = conn.session.list_resource_templates().unwrap_or_default();
                    if json {
                        ui.line(&format!(
                            "{}",
                            json!({
                                "resources": brief::resources(&found, long),
                                "resourceTemplates": brief::resources(&listed, long),
                            })
                        ));
                    } else {
                        ui.line(&format!("{} resource(s):", found.len()));
                        ui.resources(&found, long);
                        if !listed.is_empty() {
                            ui.line(&format!("\n{} template(s):", listed.len()));
                            ui.resources(&listed, long);
                        }
                    }
                    lists.resources = Some(found);
                    lists.templates = Some(listed);
                    Ok(())
                }
            },
            "read" if rest.is_empty() => Err(Failure::hinted(
                Error::usage("read needs a resource URI"),
                "usage: read URI   (`resources` lists what this server offers)",
            )),
            "read" => match shell_watched(notices, &mut subs, |w| {
                conn.session.read_resource_watching(rest, w)
            }) {
                Err(e) => Err(missing_item(e, "resources", "`resources`", info_cmd)),
                Ok(mut result) => {
                    let redirect = format!("mcpdial read {} {}", shell_word(&target), rest);
                    let files = MediaFiles {
                        dir: save_dir,
                        stem: resource_stem(rest),
                    };
                    ui.numbered(results.next_number());
                    let shown = match &filter {
                        Some(f) => ui.paged(|| shell_filter(ui, out, f, &result, json)),
                        None => ui.paged(|| {
                            emit_resource(ui, out, &mut result, json, true, &files, &redirect)
                        }),
                    };
                    // Recorded however it printed: a presenter that
                    // refuses bytes at a terminal leaves `save` as the
                    // way to have them.
                    results.record(
                        Origin::Read {
                            uri: rest.to_string(),
                        },
                        result,
                        String::new(),
                    );
                    shown
                }
            },
            "prompts" => match conn.session.list_prompts() {
                Err(e) => Err(missing_capability(e, "prompts", info_cmd)),
                Ok(found) => {
                    if json {
                        print_value(
                            ui,
                            &json!({ "prompts": brief::prompts(&found, long) }),
                            true,
                        );
                    } else {
                        ui.line(&format!("{} prompt(s):", found.len()));
                        print_prompts(ui, &found, long);
                    }
                    lists.prompts = Some(found);
                    Ok(())
                }
            },
            "prompt" => {
                let (name, args) = name_and_args(rest);
                if name.is_empty() {
                    Err(Failure::hinted(
                        Error::usage("prompt needs a name"),
                        "usage: prompt NAME {\"arg\": \"value\"}   (`prompts` lists what this server offers)",
                    ))
                } else {
                    args::shell_arguments(args, || Value::Null)
                        .and_then(|a| {
                            shell_watched(notices, &mut subs, |w| {
                                conn.session.get_prompt_watching(name, a, w)
                            })
                        })
                        .map_err(|e| missing_item(e, "prompts", "`prompts --long`", info_cmd))
                        .and_then(|mut result| {
                            let files = MediaFiles {
                                dir: save_dir,
                                stem: file_stem(name),
                            };
                            let text = rendered(ui, &mut result, json, &files, render_messages)?;
                            ui.numbered(results.next_number());
                            let shown = match &filter {
                                Some(f) => ui.paged(|| shell_filter(ui, out, f, &result, json)),
                                None => {
                                    ui.paged(|| emit_rendered(ui, out, &result, &text, json, true))
                                }
                            };
                            results.record(
                                Origin::Prompt {
                                    name: name.to_string(),
                                },
                                result,
                                text,
                            );
                            shown
                        })
                }
            }
            "call" => {
                let (tool, args) = name_and_args(rest);
                if tool.is_empty() {
                    Err(Failure::hinted(
                        Error::usage("call needs a tool name"),
                        "usage: call TOOL {\"arg\": \"value\"}   (`tools` lists what this server offers)",
                    ))
                } else if let Err(denied) = refuse_denied(&r.config, &r.name, tool) {
                    Err(denied)
                } else {
                    // Arguments that do not parse and arguments the server
                    // rejects mean the same thing to whoever typed the line:
                    // show them what this tool takes.
                    let parsed = args::shell_arguments(args, || {
                        find_tool(shell_tools(&mut lists.tools, &mut conn), tool)
                            .map_or(Value::Null, |t| t["inputSchema"].clone())
                    });
                    match parsed.and_then(|mut a| {
                        shell_fill(ui, &mut lists.tools, &mut conn, tool, &mut a).map(|()| a)
                    }) {
                        Err(e) => Err(Failure {
                            error: e,
                            hint: shell_call_hint(&mut lists.tools, &mut conn, tool, true),
                            tool: None,
                        }),
                        Ok(a) => {
                            let mut live = Live {
                                conn: &mut conn,
                                notices,
                                tools: &mut lists.tools,
                                results: &mut results,
                                subs: &mut subs,
                                filter: filter.as_ref(),
                            };
                            shell_call(ui, out, &mut live, tool, a, json, save_dir)
                        }
                    }
                }
            }
            // The numbered results of this session. A pipe never sees
            // the numbers, but a script that counted its own calls can
            // name them just the same.
            "show" => shell_named(
                ui,
                out,
                &results,
                if rest.is_empty() { "_" } else { rest },
                filter.as_ref(),
                json,
                &target,
            ),
            // `_` and `$3` name a result on their own, so that a filter
            // can follow one without anything being run again.
            reference if names_a_result(reference) => match rest.is_empty() {
                true => shell_named(ui, out, &results, reference, filter.as_ref(), json, &target),
                false => Err(Failure::hinted(
                    Error::usage(format!("{reference} names a result on its own")),
                    SHOW_USAGE,
                )),
            },
            "save" => {
                let (named, file) = split_word(rest);
                let named = if named.is_empty() { "_" } else { named };
                match results.find(named) {
                    Err(e) => Err(Failure::hinted(e, SAVE_USAGE)),
                    Ok(rec) => shell_save(ui, rec, file, json),
                }
            }
            "retry" => {
                let (head, tail) = split_word(rest);
                // `retry key=value` changes the last call; a first word
                // that is no pair names the tool to look back for.
                let named = (!head.is_empty() && !args::looks_like_pair(head)).then_some(head);
                let pairs = if named.is_some() { tail } else { rest };
                let previous = results.last_call(named);
                match named.or(previous.map(|(tool, _)| tool)) {
                    None => Err(Failure::hinted(
                        Error::usage("no call in this session yet to retry"),
                        RETRY_USAGE,
                    )),
                    Some(tool) => {
                        let base = previous.map_or_else(history::no_arguments, |(_, a)| a.clone());
                        let changes = if pairs.is_empty() {
                            Ok(history::no_arguments())
                        } else {
                            args::shell_arguments(pairs, || {
                                find_tool(shell_tools(&mut lists.tools, &mut conn), tool)
                                    .map_or(Value::Null, |t| t["inputSchema"].clone())
                            })
                        };
                        match changes {
                            Err(e) => Err(Failure {
                                error: e,
                                hint: shell_call_hint(&mut lists.tools, &mut conn, tool, true),
                                tool: None,
                            }),
                            Ok(changes) => {
                                let line = with_filter(
                                    format!("call {tool} {}", history::merged(&base, &changes)),
                                    expression,
                                );
                                print_rerun(ui, &line, json);
                                pending = Some(line);
                                Ok(())
                            }
                        }
                    }
                }
            }
            "edit" => match edit_target(&results, rest)
                .and_then(|(tool, was)| edit_arguments(&tool, &was).map(|edited| (tool, edited)))
            {
                Err(e) => Err(Failure::hinted(e, EDIT_USAGE)),
                Ok((tool, edited)) => {
                    let line = with_filter(format!("call {tool} {edited}"), expression);
                    print_rerun(ui, &line, json);
                    pending = Some(line);
                    Ok(())
                }
            },
            "subscribe" if rest.is_empty() => Err(Failure::hinted(
                Error::usage("subscribe needs a resource URI"),
                "usage: subscribe URI [FILE]   (`resources` lists what this server offers)",
            )),
            "subscribe" => {
                let (uri, sink) = subscription_target(rest);
                match start_following(&mut conn, subs.mechanism(), uri, info_cmd) {
                    Err(refused) => Err(refused),
                    Ok(()) => {
                        let replaced = subs.follow(uri, sink.clone());
                        let to = sink.describe();
                        if json {
                            ui.json(
                                &json!({"subscribed": {"uri": uri, "to": to,
                                        "via": subs.mechanism().method()}})
                                .to_string(),
                            );
                        } else {
                            let again = match replaced {
                                Some(_) => " (replacing what it was)",
                                None => "",
                            };
                            ui.line(&format!("following {uri} -> {to}{again}"));
                            if subs.mechanism().needs_listening() {
                                ui.aside("this revision delivers updates on a `listen` stream");
                            }
                        }
                        Ok(())
                    }
                }
            }
            "unsubscribe" if rest.is_empty() => Err(Failure::hinted(
                Error::usage("unsubscribe needs a resource URI"),
                "usage: unsubscribe URI   (`subscriptions` lists what this session follows)",
            )),
            "unsubscribe" => match subs.forget(rest) {
                Err(e) => Err(Failure::hinted(
                    e,
                    "`subscriptions` lists what this session is following.",
                )),
                Ok(_) => {
                    // The subscription is over here whatever the server
                    // makes of the cancellation, so the receipt goes out
                    // first and a refusal is reported after it: an update
                    // arriving from now on has nowhere left to go.
                    if json {
                        ui.json(
                            &json!({"unsubscribed": {"uri": rest,
                                    "via": subs.mechanism().method()}})
                            .to_string(),
                        );
                    } else {
                        ui.line(&format!("no longer following {rest}"));
                    }
                    match subs.mechanism() {
                        // 2026-07-28 cancels a subscription by not asking
                        // for it in the next `listen`, so there is nothing
                        // to send.
                        Mechanism::Listen => Ok(()),
                        Mechanism::Subscribe => conn
                            .session
                            .unsubscribe_resource(rest)
                            .map(|_| ())
                            .map_err(|e| missing_capability(e, "resources", info_cmd)),
                    }
                }
            },
            "subscriptions" => {
                let following: Vec<(String, String)> = subs
                    .following()
                    .map(|(uri, sink)| (uri.clone(), sink.describe()))
                    .collect();
                if json {
                    let rows: Vec<Value> = following
                        .iter()
                        .map(|(uri, to)| json!({"uri": uri, "to": to}))
                        .collect();
                    print_value(
                        ui,
                        &json!({"subscriptions": rows,
                                "via": subs.mechanism().method()}),
                        true,
                    );
                } else {
                    ui.line(&format!("{} subscription(s):", following.len()));
                    for (uri, to) in &following {
                        ui.line(&format!("  {uri} -> {to}"));
                    }
                }
                Ok(())
            }
            // A revision with `resources/subscribe` pushes its updates
            // onto whatever stream is open, so they are already here by
            // the next prompt and there is no stream to hold open.
            "listen" if !subs.mechanism().needs_listening() => Err(Failure::hinted(
                Error::usage(format!(
                    "this server speaks {}, which has no subscriptions/listen",
                    conn.session.version()
                )),
                "its updates arrive with the next command's reply; \
                 `subscribe URI` is all this session needs.",
            )),
            "listen" => match listen_bound(rest) {
                Err(bad) => Err(bad),
                Ok(bound) => {
                    let filter = subs.filter();
                    subs.opening_a_stream();
                    let held = shell_watched(notices, &mut subs, |w| {
                        conn.session.listen(filter, bound, w)
                    });
                    held.map_err(Failure::from).map(|closed| {
                        let seconds = bound.as_secs_f64();
                        if json {
                            ui.json(
                                &json!({"listened": {"seconds": seconds,
                                        "acknowledged": subs.acknowledged(),
                                        "closed": closed}})
                                .to_string(),
                            );
                        } else if !subs.acknowledged() {
                            ui.line(&format!("no acknowledgement in {seconds}s"));
                        } else if closed {
                            ui.line("the server closed the subscription");
                        } else {
                            ui.line(&format!("listened for {seconds}s"));
                        }
                    })
                }
            },
            "elicit" if rest.is_empty() => Err(Failure::hinted(
                Error::usage("elicit needs a JSON object of answers"),
                "usage: elicit {\"confirm\": true}   (used for every elicitation from here on)",
            )),
            "elicit" => parse_object(rest, "answers")
                .map_err(Failure::from)
                .map(|values| {
                    let values = values.as_object().cloned().unwrap_or_default();
                    conn.elicit.set(values.clone());
                    if json {
                        ui.json(&json!({ "elicit": values }).to_string());
                    } else {
                        ui.line(&format!("{} answer(s) ready to elicit with", values.len()));
                    }
                }),
            "raw" => {
                let (method, params) = name_and_args(rest);
                if method.is_empty() {
                    Err(Failure::hinted(
                        Error::usage("raw needs a method"),
                        "usage: raw METHOD {\"json\": \"params\"}   e.g. raw tools/list",
                    ))
                } else {
                    parse_object(params, "params")
                        .and_then(|p| conn.session.request(method, Some(p)))
                        .map_err(Failure::from)
                        .and_then(|result| {
                            ui.numbered(results.next_number());
                            let shown = match &filter {
                                Some(f) => ui.paged(|| shell_filter(ui, out, f, &result, json)),
                                None => {
                                    ui.paged(|| print_value(ui, &result, json));
                                    Ok(())
                                }
                            };
                            results.record(
                                Origin::Raw {
                                    method: method.to_string(),
                                },
                                result,
                                String::new(),
                            );
                            shown
                        })
                }
            }
            // Only reachable when line editing is off, since a terminal
            // reader consumes these itself.
            other if other.starts_with('\u{1b}') => Err(Failure::hinted(
                Error::usage("that was an escape sequence, not a command"),
                "arrow keys and line editing need a terminal on both stdin and stdout",
            )),
            other => {
                let tools = shell_tools(&mut lists.tools, &mut conn);
                let unknown = || Error::usage(format!("unknown command {other:?}"));
                let near_tool = closest(other, tools.iter().filter_map(|t| t["name"].as_str()))
                    .and_then(|near| find_tool(tools, near));
                Err(if let Some(t) = find_tool(tools, other) {
                    // The commonest mistake: typing a tool name on its own.
                    Failure::hinted(
                        Error::usage(format!("{other} is a tool, not a command")),
                        tool_usage(t, "call", ""),
                    )
                } else if let Some(near) = closest(other, SHELL_COMMANDS.iter().copied()) {
                    Failure::hinted(unknown(), format!("did you mean {near}?"))
                } else if let Some(t) = near_tool {
                    Failure::hinted(
                        unknown(),
                        format!(
                            "did you mean the tool {}?\n{}",
                            t["name"].as_str().unwrap_or("?"),
                            tool_usage(t, "call", "")
                        ),
                    )
                } else {
                    Failure::hinted(unknown(), SHELL_SUMMARY)
                })
            }
        };
        if let Err(f) = outcome {
            failures += 1;
            dropped |= dropped_the_session(&f.error);
            shell_failed(ui, &f, json);
        }
        // Between two commands, where a line of ours cannot land in the
        // middle of a line of the server's.
        failures += drain_subscriptions(ui, says, notices, &mut conn, &mut subs, &mut lists);
        input.set_completions(
            lists.tools(),
            lists.resources(),
            lists.templates(),
            lists.prompts(),
        );
    }
    input.save_history();
    Ok(if failures > 0 && !interactive {
        EXIT_ERROR
    } else {
        0
    })
}
