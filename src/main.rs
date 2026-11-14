mod config;
mod image;
mod registry;
mod state;
mod web;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, bail};
use axum::serve;
use config::Config;
use registry::HttpClient;
use state::{CollectorState, poll_loop};
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use web::{AppContext, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "healthcheck" {
        return healthcheck().await;
    }

    let config = Arc::new(Config::from_env(args)?);
    let filter = EnvFilter::try_new(config.log_level.to_ascii_lowercase())
        .with_context(|| format!("invalid LOG_LEVEL {:?}", config.log_level))?;
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let http = Arc::new(HttpClient::new(&config.version)?);
    let state = Arc::new(CollectorState::default());
    tokio::spawn(poll_loop(
        Arc::clone(&http),
        Arc::clone(&config),
        Arc::clone(&state),
    ));

    let address = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind to {address}"))?;
    info!(
        images = config.images.len(),
        address = %address,
        "starting multiregistry-exporter"
    );

    if let Err(error) = serve(listener, router(AppContext { config, state })).await {
        warn!(%error, "HTTP server stopped");
        return Err(error.into());
    }

    Ok(())
}

async fn healthcheck() -> anyhow::Result<()> {
    let port = match std::env::var("PORT") {
        Ok(value) => value
            .parse::<u16>()
            .with_context(|| format!("invalid PORT value {value:?} for healthcheck"))?,
        Err(std::env::VarError::NotPresent) => 9101,
        Err(error) => return Err(error).context("failed to read PORT for healthcheck"),
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .context("failed to create healthcheck HTTP client")?;
    let response = client
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .send()
        .await
        .context("healthcheck request failed")?;
    if !response.status().is_success() {
        bail!("health endpoint returned {}", response.status());
    }
    Ok(())
}
