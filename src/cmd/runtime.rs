//! Servers this process keeps running: the stdio daemon, and `serve`.

use crate::cli::TARGET_HELP;

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
