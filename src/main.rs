use std::io::IsTerminal;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use rand::distr::{Alphanumeric, SampleString};
use stalias::config::ServerConfig;
use stalias::password::{BreachCheck, PasswordProcessor};
use stalias::state::AppState;
use stalias::store::Store;
use stalias::web;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = match ServerConfig::from_env() {
        Ok(config) => config,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [command] if command == "healthcheck" => return healthcheck(config.listen.port()).await,
        [command] if command == "reset-admin-password" => {
            return match reset_admin_password(config).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            };
        }
        _ => {
            eprintln!("Usage: stalias [healthcheck | reset-admin-password]");
            return ExitCode::FAILURE;
        }
    }

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(config: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&config.data_dir)?;
    let store = Store::open(&config.data_dir.join("stalias.db"))?;

    let setup_code = store.settings().await?.admin_hash.is_none().then(|| {
        let code = Alphanumeric.sample_string(&mut rand::rng(), 12);
        // Printed directly, so a restrictive RUST_LOG cannot hide it.
        println!("First start. Open the web UI and enter the setup code: {code}");
        code
    });

    tokio::spawn(purge_sessions(store.clone()));
    let mut state = AppState::new(store, setup_code, config.trust_proxy).await?;
    if let Some(check) = breach_check(&config)? {
        state = state.with_breach_check(check);
    }
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!("listening on {}", config.listen);
    axum::serve(
        listener,
        web::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

/// Expired sessions are never accepted, but they would otherwise pile up in the database.
async fn purge_sessions(store: Store) {
    let mut every_hour = tokio::time::interval(Duration::from_secs(3600));
    loop {
        every_hour.tick().await;
        if let Err(e) = store.purge_expired_sessions().await {
            tracing::warn!("could not purge expired sessions: {e}");
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = terminate => {} }
}

async fn healthcheck(port: u16) -> ExitCode {
    let url = format!("http://127.0.0.1:{port}/healthz");
    // Loopback must never go through an HTTP_PROXY that the container happens to inherit.
    let Ok(client) = reqwest::Client::builder().no_proxy().build() else {
        return ExitCode::FAILURE;
    };
    match client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
    {
        Ok(_) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// The leaked-password check, unless `HIBP_CHECK=false` switched it off.
fn breach_check(config: &ServerConfig) -> Result<Option<BreachCheck>, reqwest::Error> {
    config
        .breach_check
        .then(|| BreachCheck::new(BreachCheck::DEFAULT_BASE))
        .transpose()
}

/// Only local operators with a terminal and access to the existing database can recover access.
async fn reset_admin_password(config: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    if !std::io::stdin().is_terminal() {
        return Err("Password reset requires an interactive terminal.".into());
    }
    let store = Store::open_existing(&config.data_dir.join("stalias.db"))?;
    let expected_hash = store
        .settings()
        .await?
        .admin_hash
        .ok_or("No admin exists in this database.")?;
    let password = rpassword::prompt_password("New admin password: ")?;
    let confirm = rpassword::prompt_password("Repeat new password: ")?;
    let check = breach_check(&config)?;
    let hash = PasswordProcessor::default()
        .reserve()?
        .new_hash(password, confirm, check.as_ref())
        .await?;
    if !store
        .change_admin_password(expected_hash, hash, None)
        .await?
    {
        return Err("The admin password changed concurrently. Run the reset again.".into());
    }
    println!("Admin password reset. All devices have been logged out.");
    Ok(())
}
