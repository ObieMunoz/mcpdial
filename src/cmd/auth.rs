//! Getting a token, holding one, and giving one back.
//!
//! The browser step happens once: `login` saves what it gets, and every later
//! command reads it. `token` is the same store reached by hand, for a server
//! that mints credentials somewhere this cannot see.

#[derive(clap::Args)]
pub(crate) struct LoginFlags {
    /// A saved name, or an http(s):// URL
    pub(crate) target: String,
    /// authorization-code (a browser, once) or client-credentials (a confidential
    /// client's --client-id and secret, no human)
    #[arg(
        long,
        value_name = "GRANT",
        default_value = "authorization-code",
        value_parser = ["authorization-code", "client-credentials"]
    )]
    pub(crate) grant: String,
    /// Space-separated scopes (default: whatever the server advertises)
    #[arg(long)]
    pub(crate) scope: Option<String>,
    /// Fixed loopback port for the redirect (default: any free port)
    #[arg(long)]
    pub(crate) port: Option<u16>,
    /// Use a pre-registered client id instead of a client metadata document or
    /// dynamic registration
    #[arg(long)]
    pub(crate) client_id: Option<String>,
    /// Present this client ID metadata document as the client id, instead of the
    /// one the project publishes, whether or not the server advertises support
    #[arg(long, value_name = "URL", conflicts_with_all = ["client_id", "no_client_metadata"])]
    pub(crate) client_metadata_url: Option<String>,
    /// Register dynamically even when the server accepts client metadata documents
    #[arg(long)]
    pub(crate) no_client_metadata: bool,
    /// Read that client's secret from stdin. Never from an argument.
    #[arg(long, requires = "client_id")]
    pub(crate) client_secret: bool,
    /// Read that client's secret from $VAR instead of stdin
    #[arg(
        long,
        value_name = "VAR",
        requires = "client_id",
        conflicts_with = "client_secret"
    )]
    pub(crate) client_secret_env: Option<String>,
    /// Loopback host in the redirect URI: 127.0.0.1 (default, with a localhost
    /// fallback if the server refuses it) or localhost
    #[arg(long, value_name = "HOST", value_parser = ["127.0.0.1", "localhost"])]
    pub(crate) redirect_host: Option<String>,
    /// Print the URL but do not try to open a browser
    #[arg(long)]
    pub(crate) no_browser: bool,
}
