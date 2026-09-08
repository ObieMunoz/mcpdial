//! Finding out what is out there, and what a server offers.
//!
//! The registry and the catalog answer the first question, and a dialed
//! server's own listings answer the second.

#[derive(clap::Args)]
pub(crate) struct SearchFlags {
    /// Words that must all appear in an entry's name, title or description
    pub(crate) query: Vec<String>,
    /// How many matches to show
    #[arg(long, default_value_t = 20, value_name = "N")]
    pub(crate) limit: usize,
    /// Fetch the whole list again, even if the local copy is recent
    #[arg(long, conflicts_with = "offline")]
    pub(crate) refresh: bool,
    /// Search the local copy as it is, without touching the network
    #[arg(long)]
    pub(crate) offline: bool,
}
