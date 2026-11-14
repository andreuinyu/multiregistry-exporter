mod config;
mod image;
mod registry;
mod state;
mod web;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, bail};
use axum::{Router, body::Body};
use config::Config;
use hyper::{Request, body::Incoming};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto::Builder,
    service::TowerToHyperService,
};
use registry::HttpClient;
use state::{CollectorState, poll_loop};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};
use tower::ServiceExt;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;
use web::{AppContext, router};

const MAX_ACTIVE_CONNECTIONS: usize = 128;
const MAX_HTTP2_STREAMS_PER_CONNECTION: u32 = 32;
const REQUEST_HEADER_TIMEOUT: Duration = Duration::from_secs(10);

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

    serve_http(listener, router(AppContext { config, state })).await
}

async fn serve_http(listener: TcpListener, app: Router) -> anyhow::Result<()> {
    let active_connections = Arc::new(Semaphore::new(MAX_ACTIVE_CONNECTIONS));

    loop {
        let permit = Arc::clone(&active_connections)
            .acquire_owned()
            .await
            .context("HTTP connection limit semaphore closed")?;
        let (stream, remote_addr) = listener
            .accept()
            .await
            .context("failed to accept HTTP connection")?;
        let app = app.clone();

        tokio::spawn(async move {
            let _permit = permit;
            serve_connection(stream, app, remote_addr, REQUEST_HEADER_TIMEOUT).await;
        });
    }
}

async fn serve_connection(
    stream: TcpStream,
    app: Router,
    remote_addr: SocketAddr,
    header_timeout: Duration,
) {
    let io = TokioIo::new(stream);
    let app = app.map_request(|request: Request<Incoming>| request.map(Body::new));
    let service = TowerToHyperService::new(app);
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(header_timeout)
        .max_headers(64)
        .max_buf_size(16 * 1024)
        .keep_alive(false);
    builder
        .http2()
        .max_concurrent_streams(MAX_HTTP2_STREAMS_PER_CONNECTION);

    if let Err(error) = builder.serve_connection(io, service).await {
        debug!(%remote_addr, %error, "HTTP connection closed");
    }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{Router, routing::get};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        time::timeout,
    };

    use super::serve_connection;

    #[tokio::test]
    async fn serves_requests_and_closes_connections_with_incomplete_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/", get(|| async { "ok" }));
        let server = tokio::spawn(async move {
            let (stream, remote_addr) = listener.accept().await.unwrap();
            serve_connection(stream, app.clone(), remote_addr, Duration::from_millis(50)).await;
            let (stream, remote_addr) = listener.accept().await.unwrap();
            serve_connection(stream, app, remote_addr, Duration::from_millis(50)).await;
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(1), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert!(response.ends_with(b"ok"));

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"GET / HTTP/1.1\r\nHost:").await.unwrap();

        let mut response = Vec::new();
        timeout(Duration::from_secs(1), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.is_empty());
        server.await.unwrap();
    }
}
