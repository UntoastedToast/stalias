use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use rand::distr::{Alphanumeric, SampleString};
use stalias::config::ServerConfig;
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

    // Used by the container HEALTHCHECK, since the image has no curl.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck(config.listen.port()).await;
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
    let state = AppState::new(store, setup_code, config.trust_proxy).await?;
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
