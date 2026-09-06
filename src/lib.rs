//! mcpdial - dial any MCP server from the shell.
//!
//! An MCP server is a JSON-RPC 2.0 endpoint reachable over one of two transports:
//! Streamable HTTP, or newline-delimited JSON on a subprocess's stdin/stdout. The
//! "connector" in a host application is a convenience, not part of the protocol.
//! If you can POST a body or spawn a process, you can drive the server directly.
//!
//! On top of that bare protocol this crate keeps a list of named servers, stores
//! the OAuth tokens it acquires so the browser step happens once, and can report
//! connection status and available tools across every server at once.
//!
//! ```no_run
//! use mcpdial::{HttpTransport, Session};
//!
//! let mut session = Session::new(HttpTransport::new("https://mcp.deepwiki.com/mcp"));
//! session.initialize()?;
//! for tool in session.list_tools()? {
//!     println!("{}", tool["name"]);
//! }
//! # Ok::<(), mcpdial::Error>(())
//! ```

pub mod catalog;
pub mod client;
pub mod config;
pub mod daemon;
pub mod export_config;
pub mod import_config;
pub mod oauth;
pub mod protocol;
pub mod registry;
pub mod search;
pub mod session;
pub mod transport;

pub use client::{connect, probe, probe_all, resolve, Connection, Probe, Status};
pub use config::{Credential, ServerConfig, Store};
pub use protocol::{decode_body, Error, KnownVersion, Result, CLIENT_NAME, PROTOCOL_VERSION};
pub use session::{render_content, Session};
pub use transport::http::{HttpTransport, USER_AGENT};
pub use transport::stdio::StdioTransport;
pub use transport::Transport;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
