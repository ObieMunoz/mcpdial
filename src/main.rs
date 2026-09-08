use crate::failure::Failure;
use clap::Parser;
use cli::Cli;
use mcpdial::Store;
use present::Presenter;
use std::process::ExitCode;

mod args;
mod brief;
mod browse;
mod cli;
mod cmd;
mod diagnose;
mod env_defaults;
mod failure;
mod grep;
mod media;
mod notices;
mod output;
mod path;
mod pick;
mod present;
mod prompt;
mod render;
mod shell;
mod snapshot;
mod tasks;
mod validate;

fn main() -> ExitCode {
    let mut cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            return bare_mcpdial(e)
        }
        Err(e) => e.exit(),
    };
    let defaults = env_defaults::apply(&mut cli);
    let ui = <dyn Presenter>::choose(&cli);
    let json = cli.json;
    match defaults
        .map_err(Failure::from)
        .and_then(|()| cmd::dispatch(ui.as_ref(), cli))
    {
        Ok(code) => ExitCode::from(code),
        Err(f) => {
            if json {
                ui.err_line(&f.to_json().to_string());
            } else {
                f.report(ui.as_ref());
            }
            ExitCode::from(f.exit_code())
        }
    }
}

/// A bare `mcpdial`: [`pick::welcome`] for a person, and clap's usage error for
/// every pipe, program, `--json` and `--plain`, exactly as it has always been.
/// Never the picker and never a dial, whatever is saved - `mcpdial pick` is
/// where choosing a server and calling one of its tools lives, and asking for
/// it is what starts it.
fn bare_mcpdial(e: clap::Error) -> ExitCode {
    // Somewhere to read the global flags from. The argv behind this error
    // carries none of them - a flag without a subcommand is a different clap
    // error, which never reaches here - so the environment is the whole of
    // what there is to find, and MCPDIAL_JSON in it is a program asking.
    let mut cli = Cli::parse_from(["mcpdial", "pick"]);
    let _ = env_defaults::apply(&mut cli);
    if !pick::at_a_terminal(&cli) {
        e.exit();
    }
    let ui = <dyn Presenter>::choose(&cli);
    match Store::from_env()
        .map_err(Failure::from)
        .and_then(|store| pick::welcome(ui.as_ref(), &store))
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            f.report(ui.as_ref());
            ExitCode::from(f.exit_code())
        }
    }
}
