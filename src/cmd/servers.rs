//! Saving servers, listing them, and moving them in and out of host configs.
//!
//! What `add` writes is what every later dial reads, so most of this file is
//! about settling a location, a transport and a set of lists before anything is
//! saved rather than after.

use crate::cli::SAVED_NAME_HELP;

#[derive(clap::Args)]
pub(crate) struct AddFlags {
    /// The name to save it under, and to dial it by from then on
    pub(crate) name: String,
    /// Streamable HTTP endpoint
    #[arg(
        long,
        value_name = "URL",
        conflicts_with_all = ["stdio", "registry", "catalog"],
        required_unless_present_any = ["stdio", "registry", "catalog"]
    )]
    pub(crate) http: Option<String>,
    /// An entry of the curated catalog, by its id (`mcpdial catalog` lists them)
    #[arg(long, value_name = "ID", conflicts_with_all = ["stdio", "registry"])]
    pub(crate) catalog: Option<String>,
    /// Command that speaks MCP on stdio
    #[arg(long, value_name = "CMD", conflicts_with = "registry")]
    pub(crate) stdio: Option<String>,
    /// A server in the MCP registry, by its registry name (io.github.owner/server)
    #[arg(long, value_name = "NAME")]
    pub(crate) registry: Option<String>,
    /// Run this package type from the registry entry rather than the first offered
    #[arg(
        long,
        value_name = "TYPE",
        value_parser = ["npm", "pypi", "oci"],
        requires = "registry",
        conflicts_with = "remote"
    )]
    pub(crate) package: Option<String>,
    /// Use the registry entry's remote endpoint rather than a package
    #[arg(long, requires = "registry")]
    pub(crate) remote: bool,
    /// A value the registry entry leaves to you, repeatable, in the order listed
    #[arg(long, value_name = "VALUE", requires = "registry")]
    pub(crate) arg: Vec<String>,
    /// Environment variable for the stdio process, repeatable
    #[arg(long, value_name = "KEY=VALUE")]
    pub(crate) env: Vec<String>,
    /// Working directory for the stdio process
    #[arg(long, value_name = "DIR")]
    pub(crate) cwd: Option<String>,
    /// Offer only tools matching this glob (`*`, `?`), repeatable
    #[arg(long, value_name = "PATTERN")]
    pub(crate) allow: Vec<String>,
    /// Hide and refuse tools matching this glob, repeatable; beats --allow
    #[arg(long, value_name = "PATTERN")]
    pub(crate) deny: Vec<String>,
    /// Replace a server already saved under this name
    #[arg(long)]
    pub(crate) force: bool,
    /// Save without dialing the server for its status
    #[arg(long)]
    pub(crate) no_probe: bool,
}

#[derive(clap::Args)]
pub(crate) struct SetFlags {
    #[arg(help = SAVED_NAME_HELP)]
    pub(crate) name: String,
    /// Replace the allow list with these globs (`*`, `?`), repeatable
    #[arg(long, value_name = "PATTERN", conflicts_with = "clear_allow")]
    pub(crate) allow: Vec<String>,
    /// Replace the deny list with these globs, repeatable; beats --allow
    #[arg(long, value_name = "PATTERN", conflicts_with = "clear_deny")]
    pub(crate) deny: Vec<String>,
    /// Remove the allow list, so every tool not denied is offered
    #[arg(long)]
    pub(crate) clear_allow: bool,
    /// Remove the deny list
    #[arg(long)]
    pub(crate) clear_deny: bool,
}

#[derive(clap::Args)]
pub(crate) struct ImportFlags {
    /// A host's config: JSON with an `mcpServers`, `servers` (VS Code) or `mcp`
    /// (OpenCode) object, or Codex's `config.toml`. Omit to scan the usual locations.
    pub(crate) file: Option<std::path::PathBuf>,
    /// Scan one host's locations only: vscode, codex, opencode, claude, cursor or windsurf
    #[arg(long, value_name = "HOST", conflicts_with = "file")]
    pub(crate) from: Option<mcpdial::import_config::Host>,
    /// Overwrite servers that already exist under the same name
    #[arg(long)]
    pub(crate) force: bool,
}

#[derive(clap::Args)]
pub(crate) struct ExportFlags {
    /// Servers to export; every saved server when none is named
    #[arg(value_name = "NAME")]
    pub(crate) names: Vec<String>,
    /// Shape to write: mcpservers (Claude, Cursor, Windsurf), vscode or codex
    #[arg(long, default_value = "mcpservers", value_name = "FORMAT")]
    pub(crate) format: mcpdial::export_config::Format,
    /// Print this host file with the exported servers merged in; it is never written
    #[arg(long, value_name = "FILE")]
    pub(crate) merge: Option<std::path::PathBuf>,
}

#[derive(clap::Args)]
pub(crate) struct LsFlags {
    /// Do not connect; just show the configuration
    #[arg(long)]
    pub(crate) no_probe: bool,
    /// Dial every server again instead of reusing a status from the last few minutes
    #[arg(long, conflicts_with = "no_probe")]
    pub(crate) refresh: bool,
}
