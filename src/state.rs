use std::sync::{Arc, RwLock};

use crate::aliases::AliasService;
use crate::auth::LoginLimiter;
use crate::password::{BreachCheck, PasswordProcessor};
use crate::stalwart::{self, Client};
use crate::store::{self, Store};

/// Everything that talks to Stalwart. Rebuilt whenever the connection settings change.
pub struct Backend {
    pub client: Arc<Client>,
    pub aliases: AliasService,
}

/// Shared application state. Cloning is cheap.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    /// Printed to the log on first start; protects the initial admin signup.
    /// `None` once an admin exists.
    pub setup_code: Option<Arc<str>>,
    pub login_limiter: Arc<LoginLimiter>,
    pub setup_limiter: Arc<LoginLimiter>,
    pub passwords: PasswordProcessor,
    /// Whether `X-Forwarded-For` comes from a reverse proxy we control.
    pub trust_proxy: bool,
    /// Asks Have I Been Pwned about new admin passwords. `None` switches the check off.
    pub breach_check: Option<Arc<BreachCheck>>,
    backend: Arc<RwLock<Option<Arc<Backend>>>>,
    alias_write_lock: Arc<tokio::sync::Mutex<()>>,
    connection_lock: Arc<tokio::sync::RwLock<()>>,
}

impl AppState {
    /// Builds the state and connects to Stalwart if it is already configured.
    pub async fn new(
        store: Store,
        setup_code: Option<String>,
        trust_proxy: bool,
    ) -> Result<Self, AppError> {
        let state = Self {
            store,
            setup_code: setup_code.map(Into::into),
            login_limiter: Arc::default(),
            setup_limiter: Arc::default(),
            passwords: PasswordProcessor::default(),
            trust_proxy,
            breach_check: None,
            backend: Arc::default(),
            alias_write_lock: Arc::default(),
            connection_lock: Arc::default(),
        };
        state.reload_backend().await?;
        Ok(state)
    }

    /// Enables the leaked-password check for new admin passwords.
    #[must_use]
    pub fn with_breach_check(mut self, check: BreachCheck) -> Self {
        self.breach_check = Some(Arc::new(check));
        self
    }

    /// The current Stalwart connection, or `None` until one is configured.
    pub fn backend(&self) -> Option<Arc<Backend>> {
        self.backend.read().unwrap().clone()
    }

    /// Call after the Stalwart URL or key changed.
    pub async fn reload_backend(&self) -> Result<(), AppError> {
        let _guard = self.connection_lock.write().await;
        let settings = self.store.settings().await?;
        let backend = match (settings.stalwart_url, settings.stalwart_key) {
            (Some(url), Some(key)) => Some(self.build_backend(&url, key)?),
            _ => None,
        };
        *self.backend.write().unwrap() = backend;
        Ok(())
    }

    fn build_backend(&self, url: &str, key: String) -> Result<Arc<Backend>, AppError> {
        let client = Arc::new(Client::new(url, key)?);
        Ok(Arc::new(Backend {
            aliases: AliasService::with_write_lock(client.clone(), self.alias_write_lock.clone()),
            client,
        }))
    }

    /// Keeps token authentication and creation on one connection until the request ends.
    pub(crate) async fn connection_read(&self) -> tokio::sync::OwnedRwLockReadGuard<()> {
        self.connection_lock.clone().read_owned().await
    }

    /// Waits for token requests before revoking old tokens and publishing the new backend.
    pub async fn set_connection(&self, url: String, key: String) -> Result<(), AppError> {
        let backend = self.build_backend(&url, key.clone())?;
        let state = self.clone();
        // Once saving starts, finish the DB/backend transition even if the browser disconnects.
        tokio::spawn(async move {
            let _guard = state.connection_lock.write().await;
            state.store.set_stalwart(url, key).await?;
            *state.backend.write().unwrap() = Some(backend);
            Ok(())
        })
        .await
        .expect("connection update task panicked")
    }
}

/// Errors from setting up or switching the Stalwart connection.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Stalwart(#[from] stalwart::Error),
}
