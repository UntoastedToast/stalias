//! Thin JMAP client for the parts of Stalwart's management API that stalias needs.

mod client;
mod error;
mod model;

pub use client::Client;
pub use error::Error;
pub use model::{Account, Alias, Domain};
