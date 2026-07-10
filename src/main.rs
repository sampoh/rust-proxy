mod cli;
mod proxy;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{Router, routing};
use clap::Parser;
use tokio::sync::watch;

use crate::cli::Cli;
use crate::proxy::{AppState, proxy_handler, shutdown_handler};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Normalise target: strip trailing slash
    let target = cli.target.trim_end_matches('/').to_string();

    // Extract host[:port] for the Host header
    let target_host = extract_host(&target).unwrap_or_else(|| {
        eprintln!("[ERROR] Could not parse host from target URL: {target}");
        std::process::exit(1);
    });

    // Build a single shared reqwest::Client.
    // - HTTP/2 is negotiated via ALPN when available.
    // - Redirects are NOT followed; the original response is forwarded as-is.
    // - Auto-decompression is disabled so Content-Encoding is preserved.
    // - The connection pool is kept alive aggressively.
    let mut client_builder = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd();

    if cli.timeout > 0 {
        client_builder = client_builder.timeout(Duration::from_secs(cli.timeout));
    }

    let client = client_builder.build()
        .unwrap_or_else(|e| {
            eprintln!("[ERROR] Failed to build HTTP client: {e}");
            std::process::exit(1);
        });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);

    let state = Arc::new(AppState {
        client,
        target: target.clone(),
        target_host,
        shutdown_tx,
    });

    let app = Router::new()
        .route("/__shutdown", routing::post(shutdown_handler))
        .fallback(proxy_handler)
        .with_state(state);

    let addr: SocketAddr = cli.listen.parse().unwrap_or_else(|_| {
        eprintln!("[ERROR] Invalid listen address: {}", cli.listen);
        std::process::exit(1);
    });

    println!("Listening on {addr}");
    println!("Target: {target}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap_or_else(|e| {
        eprintln!("[ERROR] Failed to bind {addr}: {e}");
        std::process::exit(1);
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await
        .unwrap_or_else(|e| {
            eprintln!("[ERROR] Server error: {e}");
            std::process::exit(1);
        });
}

async fn shutdown_signal(mut rx: watch::Receiver<bool>) {
    #[cfg(unix)]
    let sigterm = async {
        use tokio::signal::unix::{signal, SignalKind};
        signal(SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let sigterm = std::future::pending::<()>();

    let stdin_closed = async {
        use tokio::io::AsyncReadExt;
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 1];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                _ => {}
            }
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = rx.changed() => {},
        _ = sigterm => {},
        _ = stdin_closed => {},
    }
}

/// Parse `host` or `host:port` from a URL string.
fn extract_host(url: &str) -> Option<String> {
    // Strip scheme
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;

    // Take only the authority part (before the first '/')
    let authority = without_scheme.split('/').next()?;
    Some(authority.to_string())
}
