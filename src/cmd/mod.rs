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
