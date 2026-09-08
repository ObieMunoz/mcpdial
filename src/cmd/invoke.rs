//! Running something on a server: a tool, a prompt, or a raw method.
//!
//! These are the commands with a result worth printing, which is why they are
//! the ones `--save-dir`, `--max-chars` and `--output` apply to.

use crate::cli::TARGET_HELP;
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct ToolsFlags {
    /// A saved name, an http(s):// URL, or stdio:<command>; every saved server when omitted
    pub(crate) target: Option<String>,
    /// Show full descriptions and parameters
    #[arg(short, long)]
    pub(crate) long: bool,
    /// Include the tools the server's allow and deny lists hide, marked (denied)
    #[arg(long, requires = "target")]
    pub(crate) all: bool,
    /// Write the tools in full to FILE, which must not exist, instead of listing them
    #[arg(long, value_name = "FILE", requires = "target", conflicts_with_all = ["long", "all"])]
    pub(crate) snapshot: Option<PathBuf>,
    /// Report how the tools differ from a snapshot; exit 3 when a caller would break
    #[arg(long, value_name = "FILE", requires = "target",
          conflicts_with_all = ["snapshot", "long", "all"])]
    pub(crate) check: Option<PathBuf>,
    /// With --check: hold every snapshotted tool to the object the snapshot holds
    #[arg(long, requires = "check")]
    pub(crate) strict: bool,
}

#[derive(clap::Args)]
pub(crate) struct CallFlags {
    #[arg(help = TARGET_HELP)]
    pub(crate) target: String,
    /// A tool name from `mcpdial tools TARGET`
    pub(crate) tool: String,
    /// One JSON object (inline, @file, or - for stdin), or key=value pairs
    pub(crate) arguments: Vec<String>,
    /// Answers for anything the server elicits mid-call: a JSON object or @file
    #[arg(long, value_name = "JSON")]
    pub(crate) elicit: Option<String>,
    /// Print a url-mode elicitation's address instead of opening a browser
    #[arg(long)]
    pub(crate) no_browser: bool,
    /// Refuse to call when TOOL has drifted from this snapshot; exit 3 with the differences
    #[arg(long, value_name = "FILE")]
    pub(crate) check: Option<PathBuf>,
    /// With --check: hold TOOL to the object the snapshot holds
    #[arg(long, requires = "check")]
    pub(crate) strict: bool,
    /// Have the server run TOOL in the background, and poll until it finishes
    #[arg(long, conflicts_with = "detach")]
    pub(crate) task: bool,
    /// Start TOOL in the background, print the task id, and exit
    #[arg(long)]
    pub(crate) detach: bool,
    /// Seconds the server is asked to keep a --task or --detach task for (default 3600)
    #[arg(long, value_name = "SECS")]
    pub(crate) ttl: Option<u64>,
}

#[derive(clap::Args)]
pub(crate) struct PromptFlags {
    #[arg(help = TARGET_HELP)]
    pub(crate) target: String,
    /// A prompt name from `mcpdial prompts TARGET`
    pub(crate) name: String,
    /// One JSON object (inline, @file, or - for stdin), or key=value pairs
    pub(crate) arguments: Vec<String>,
    /// Answers for anything the server elicits mid-call: a JSON object or @file
    #[arg(long, value_name = "JSON")]
    pub(crate) elicit: Option<String>,
    /// Print a url-mode elicitation's address instead of opening a browser
    #[arg(long)]
    pub(crate) no_browser: bool,
}
