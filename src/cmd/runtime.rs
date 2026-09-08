//! Servers this process keeps running: the stdio daemon, and `serve`.

use crate::cli::TARGET_HELP;
use crate::cmd::Ctx;
use crate::failure::{error_json, Failure, EXIT_ERROR};
use crate::render::{print_json, print_value};
use crate::validate::idle_duration;
use mcpdial::{daemon, serve};
use serde_json::json;

#[derive(clap::Args)]
pub(crate) struct ServeFlags {
    #[arg(help = TARGET_HELP)]
    pub(crate) target: String,
    /// Address to listen on; port 0 picks a free one and reports it
    #[arg(
        long,
        value_name = "ADDR",
        default_value = "127.0.0.1:0",
        conflicts_with = "stdio"
    )]
    pub(crate) listen: String,
    /// Speak MCP on this process's own stdin and stdout instead of listening
    #[arg(long)]
    pub(crate) stdio: bool,
    /// Allow --listen on an address other than loopback
    #[arg(long, conflicts_with = "stdio")]
    pub(crate) listen_any: bool,
    /// Env var holding the bearer token clients must present
    #[arg(long, value_name = "VAR", conflicts_with = "stdio")]
    pub(crate) bearer_env: Option<String>,
    /// Expose only tools matching this glob, on top of the server's own lists
    #[arg(long, value_name = "PATTERN")]
    pub(crate) allow: Vec<String>,
    /// Never expose tools matching this glob, repeatable; beats --allow
    #[arg(long, value_name = "PATTERN")]
    pub(crate) deny: Vec<String>,
}

pub(crate) fn start(cx: &mut Ctx<'_>, name: String, idle: Option<f64>) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
    let idle = idle_duration(idle)?;
    let pid = daemon::start(store, &name, idle, opts)?;
    if json {
        print_json(
            ui,
            &json!({
                "name": name, "pid": pid,
                "socket": daemon::socket_path(store, &name),
            }),
        );
    } else {
        ui.err_line(&format!("started {name} (pid {pid})"));
    }
    Ok(0)
}

pub(crate) fn stop(cx: &mut Ctx<'_>, name: String) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    daemon::stop(store, &name, opts)?;
    ui.err_line(&format!("stopped {name}"));
    Ok(0)
}

pub(crate) fn daemon(cx: &mut Ctx<'_>, name: String, idle: Option<f64>) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let idle = idle_duration(idle)?;
    match daemon::serve(store, &name, opts, idle) {
        Ok(()) => Ok(0),
        Err(e) => {
            print_value(ui, &error_json(&e), true);
            Ok(EXIT_ERROR)
        }
    }
}

pub(crate) fn serve(cx: &mut Ctx<'_>, flags: ServeFlags) -> Result<u8, Failure> {
    let ServeFlags {
        target,
        listen,
        stdio,
        listen_any,
        bearer_env,
        allow,
        deny,
    } = flags;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
    // `serve` runs until it is killed and takes what it is given, so it is
    // handed its own copy rather than borrowing one nothing will hand back.
    Ok(serve::run(
        store.clone(),
        opts.clone(),
        &target,
        serve::Settings {
            listen,
            stdio,
            listen_any,
            bearer_env,
            allow,
            deny,
            json,
        },
    )?)
}
