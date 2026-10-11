//! stalias manages email aliases for Stalwart Mail Server and exposes them through
//! an Addy.io-compatible API, so password managers like Bitwarden can create them.

pub mod aliases;
pub mod auth;
pub mod config;
pub mod naming;
pub mod password;
pub mod stalwart;
pub mod state;
pub mod store;
pub mod web;
