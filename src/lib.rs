pub mod protocol;
pub use protocol::{decode_body, Error, Result, CLIENT_NAME, PROTOCOL_VERSION};
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
