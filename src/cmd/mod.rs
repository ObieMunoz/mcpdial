//! One module per family of commands.
//!
//! Every subcommand's flags are a [`clap::Args`] struct beside the code that
//! runs it, which is the shape `grep` and `tasks` already had: what a command
//! accepts and what it does with it change together, so they live together.

pub(crate) mod auth;
pub(crate) mod discover;
pub(crate) mod invoke;
pub(crate) mod runtime;
pub(crate) mod servers;

use crate::cli::{Cmd, ConfigCmd, TokenCmd};
use crate::diagnose::shell_word;
use crate::failure::Failure;
use crate::notices::Notices;
use crate::output::Output;
use crate::present::Presenter;
use crate::shell;
use crate::validate::{parse_headers, read_json_arg};
use crate::{browse, grep, pick, present, tasks, Cli};
use clap::CommandFactory;
use mcpdial::client::Options;
use mcpdial::transport::trace::Trace;
use mcpdial::{client, daemon, Elicit, Error, Store, USER_AGENT};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

/// What every command is handed, whatever it goes on to do with it.
///
/// These are the global flags after the environment has had its say, settled
/// once rather than by each command in turn: where a result goes, how much of
/// it, whether it reads as prose or as JSON, and the store and dial options
/// behind it.
pub(crate) struct Ctx<'a> {
    pub(crate) ui: &'a dyn Presenter,
    pub(crate) store: Store,
    pub(crate) opts: Options,
    pub(crate) out: Output,
    pub(crate) notices: Notices<'a>,
    pub(crate) save_dir: Option<PathBuf>,
    pub(crate) timeout: Option<f64>,
    pub(crate) json: bool,
    pub(crate) plain: bool,
}

/// What this invocation can answer when a server asks for one more fact.
///
/// Nothing here may wait on a person who is not there, and whether one is there
/// is a question already answered: [`Presenter::asks`] is what says whether this
/// run is a person's or a program's, and a missing argument and an elicitation
/// are the same question put twice. [`Presenter::watched`] is asked as well,
/// because an elicitation writes its question on stderr above the prompts, and
/// prose nobody will read is not worth stalling a call for; `--json` says so
/// twice over, since it also decides what the report on stderr looks like.
pub(crate) fn elicitation(
    ui: &dyn Presenter,
    answers: Option<&str>,
    no_browser: bool,
    json: bool,
    shell: bool,
) -> Result<Elicit, Error> {
    let answers = answers
        .map(|text| read_json_arg(text, "elicit answers"))
        .transpose()?
        .and_then(|v| v.as_object().cloned());
    Ok(Elicit {
        answers,
        ask: ui.asks() && ui.watched() && !json,
        later: shell,
        browser: !no_browser,
        json,
    })
}

pub(crate) fn dial(
    store: &Store,
    opts: &Options,
    target: &str,
) -> Result<client::Connection, Failure> {
    let r = client::resolve(store, target)?;
    Ok(client::connect(store, &r, opts)?)
}

pub(crate) fn info_hint(target: &str) -> String {
    format!("`mcpdial info {}`", shell_word(target))
}

/// Where to look for the resource or prompt the server did have.
pub(crate) fn resources_hint(target: &str) -> String {
    format!("`mcpdial resources {}`", shell_word(target))
}

pub(crate) fn prompts_hint(target: &str) -> String {
    format!("`mcpdial prompts {} --long`", shell_word(target))
}

/// The name a credential is filed under, and whether the target names anything
/// mcpdial could dial: a saved server, a URL, or a `stdio:` command line.
pub(crate) fn credential_key(store: &Store, target: String) -> (String, bool) {
    match client::resolve(store, &target) {
        Ok(r) => (r.name, true),
        Err(_) => (target, false),
    }
}

pub(crate) fn name_and_args(rest: &str) -> (&str, &str) {
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
    match args.trim() {
        "" => (name, "{}"),
        args => (name, args),
    }
}

pub(crate) fn dispatch(ui: &dyn Presenter, cli: Cli) -> Result<u8, Failure> {
    let store = Store::from_env()?;
    let opts = Options {
        timeout: cli
            .timeout
            .map(|secs| Duration::from_secs_f64(secs.max(0.0))),
        user_agent: cli
            .user_agent
            .clone()
            .unwrap_or_else(|| USER_AGENT.to_string()),
        extra_headers: parse_headers(&cli.headers)?,
        token_env: cli.token_env.clone(),
        protocol_version: cli.protocol_version,
        verbose: cli.verbose,
        no_daemon: cli.no_daemon || daemon::disabled_by_env(),
        retry: !cli.no_retry,
        log_level: cli.log_level,
        trace: Trace::from_flag_or_env(cli.trace.as_deref())?,
        ..Options::default()
    };
    // Built before the command is matched, because matching moves it out of
    // `cli`; it borrows nothing from there.
    let notices = Notices::new(ui, &cli);
    if let Some(dir) = &cli.save_dir {
        let renders_media = matches!(
            cli.cmd,
            Cmd::Call { .. }
                | Cmd::Prompt { .. }
                | Cmd::Read { .. }
                | Cmd::Shell { .. }
                | Cmd::Tasks { .. }
        );
        if !renders_media {
            return Err(
                Error::usage("--save-dir applies to call, prompt, read, shell and tasks").into(),
            );
        }
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::usage(format!("--save-dir {}: {e}", dir.display())))?;
    }
    let plain = present::wants_plain(&cli);
    let mut cx = Ctx {
        ui,
        store,
        opts,
        out: Output::choose(&cli)?,
        notices,
        save_dir: cli.save_dir.clone(),
        timeout: cli.timeout,
        json: cli.json,
        plain,
    };

    match cli.cmd {
        Cmd::Add(flags) => servers::add(&mut cx, flags),

        Cmd::Set(flags) => servers::set(&mut cx, flags),

        Cmd::Search(flags) => discover::search(&mut cx, flags),

        Cmd::Import(flags) => servers::import(&mut cx, flags),

        Cmd::Export(flags) => servers::export(&mut cx, flags),

        Cmd::Shell { target, no_browser } => shell::run(&mut cx, target, no_browser),

        Cmd::Start { name, idle } => runtime::start(&mut cx, name, idle),

        Cmd::Stop { name } => runtime::stop(&mut cx, name),
        Cmd::Daemon { name, idle } => runtime::daemon(&mut cx, name, idle),

        Cmd::Schema { target, tool } => invoke::schema(&mut cx, target, tool),

        Cmd::Resources { target, long } => discover::resources(&mut cx, target, long),

        Cmd::Read { target, uri } => discover::read(&mut cx, target, uri),

        Cmd::Prompts { target, long } => discover::prompts(&mut cx, target, long),

        Cmd::Complete { target, of } => discover::complete(&mut cx, target, of),

        Cmd::Prompt(flags) => invoke::prompt(&mut cx, flags),

        Cmd::Guide => discover::guide(&mut cx),

        Cmd::Catalog { offline } => discover::catalog(&mut cx, offline),

        Cmd::Browse {
            all,
            offline,
            preview,
        } => browse::run(
            &cx,
            browse::Flags {
                all,
                offline,
                preview,
                interactive: !cx.plain && std::io::stdin().is_terminal(),
            },
        ),

        Cmd::Pick => {
            cx.opts.elicit = elicitation(cx.ui, None, false, cx.json, false)?;
            pick::run(&mut cx)
        }

        Cmd::Completions { shell } => {
            // Building the whole command tree to walk it recurses deeper than
            // the 1 MB stack a Windows main thread is given, and every global
            // flag added takes it deeper still, so the walk gets a stack of its
            // own rather than being one flag away from overflowing.
            const SCRIPT_STACK: usize = 8 * 1024 * 1024;
            let failed =
                |what: &str| Error::transport(format!("writing the {shell} script: {what}"));
            std::thread::Builder::new()
                .stack_size(SCRIPT_STACK)
                .spawn(move || {
                    let mut command = Cli::command();
                    let name = command.get_name().to_string();
                    clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
                })
                .map_err(|e| failed(&e.to_string()))?
                .join()
                .map_err(|_| failed("the writer stopped"))?;
            Ok(0)
        }

        Cmd::Rm { name } => servers::rm(&mut cx, name),

        Cmd::Ls(flags) => servers::ls(&mut cx, flags),

        Cmd::Tools(flags) => invoke::tools(&mut cx, flags),

        Cmd::Grep(flags) => grep::run(&cx, flags),

        Cmd::Info { target } => discover::info(&mut cx, target),

        Cmd::Call(flags) => invoke::call(&mut cx, flags),

        Cmd::Tasks(flags) => tasks::run(&mut cx, flags),

        Cmd::Raw {
            target,
            method,
            params,
        } => invoke::raw(&mut cx, target, method, params),

        Cmd::Serve(flags) => runtime::serve(&mut cx, flags),

        Cmd::Login(flags) => auth::login(&mut cx, flags),

        Cmd::Logout { target } | Cmd::Token(TokenCmd::Rm { name: target }) => {
            auth::logout(&mut cx, target)
        }

        Cmd::Token(TokenCmd::Set { name, env }) => auth::token_set(&mut cx, name, env),

        Cmd::Token(TokenCmd::Show { name }) => auth::token_show(&mut cx, name),

        Cmd::Config(ConfigCmd::Credentials { store: chosen }) => auth::credentials(&mut cx, chosen),
    }
}
