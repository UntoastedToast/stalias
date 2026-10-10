//! Process-level configuration from environment variables.
//! Everything else (Stalwart connection, domains, tokens) is set up in the web UI.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Settings read from the environment at startup.
pub struct ServerConfig {
    /// Address to listen on: all interfaces on `PORT` (default 8080).
    pub listen: SocketAddr,
    /// Directory that holds `stalias.db` (default `/data`).
    pub data_dir: PathBuf,
    /// Set `TRUST_PROXY=true` when a reverse proxy in front sets `X-Forwarded-For`.
    pub trust_proxy: bool,
}

impl ServerConfig {
    /// Reads `PORT`, `DATA_DIR` and `TRUST_PROXY`.
    ///
    /// # Errors
    ///
    /// Returns a message when `PORT` is not a valid port.
    pub fn from_env() -> Result<Self, String> {
        let port: u16 = match std::env::var("PORT") {
            Ok(v) => v
                .parse()
                .map_err(|_| format!("PORT is not a valid port: {v}"))?,
            Err(_) => 8080,
        };
        Ok(Self {
            listen: SocketAddr::from(([0, 0, 0, 0], port)),
            data_dir: std::env::var_os("DATA_DIR")
                .map_or_else(|| PathBuf::from("/data"), PathBuf::from),
            trust_proxy: std::env::var("TRUST_PROXY")
                .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes")),
        })
    }
}
