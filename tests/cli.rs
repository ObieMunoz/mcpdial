//! The end-to-end tests, by subject.
//!
//! One binary, not one per file: an integration test is its own executable and
//! links the whole crate, so a topic gets a module here rather than a target of
//! its own. `tests/common` is the fake MCP and OAuth server they all drive.

mod common;

#[path = "cli/agent.rs"]
mod agent;
#[path = "cli/confidential.rs"]
mod confidential;
#[path = "cli/elicitation.rs"]
mod elicitation;
#[path = "cli/listings.rs"]
mod listings;
#[path = "cli/oauth.rs"]
mod oauth;
#[path = "cli/protocol.rs"]
mod protocol;
#[path = "cli/resources.rs"]
mod resources;
#[path = "cli/servers.rs"]
mod servers;
#[path = "cli/shell.rs"]
mod shell;
#[path = "cli/snapshots.rs"]
mod snapshots;
#[path = "cli/transport.rs"]
mod transport;
