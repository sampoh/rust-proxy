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

    let client = client_builder.build().unwrap_or_else(|e| {
        eprintln!("[ERROR] Failed to build HTTP client: {e}");
        std::process::exit(1);
    });

    let addr: SocketAddr = cli.listen.parse().unwrap_or_else(|_| {
        eprintln!("[ERROR] Invalid listen address: {}", cli.listen);
        std::process::exit(1);
    });

    // A single task owns tokio stdin and broadcasts EOF via a watch channel.
    // This avoids multiple concurrent readers on the same stdin handle.
    let (stdin_tx, stdin_rx) = watch::channel(false);
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 1];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                _ => {}
            }
        }
        let _ = stdin_tx.send(true);
    });

    // Try to bind the port. If already in use, wait dormant (stdin still monitored)
    // until the port is released or stdin closes.
    let listener = match wait_for_port(addr, stdin_rx.clone()).await {
        Some(l) => l,
        None => return,
    };

    println!("Listening on {addr}");
    println!("Target: {target}");

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

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx, stdin_rx))
        .await
        .unwrap_or_else(|e| {
            eprintln!("[ERROR] Server error: {e}");
            std::process::exit(1);
        });
}

/// Try to bind `addr`. If the port is already in use, retry every second
/// in dormant mode (stdin is still monitored). Returns `None` if stdin
/// closed before the port became available.
async fn wait_for_port(
    addr: SocketAddr,
    mut stdin_rx: watch::Receiver<bool>,
) -> Option<tokio::net::TcpListener> {
    let mut dormant = false;
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                if dormant {
                    eprintln!("[INFO] Port {addr} is now available. Starting HTTPD.");
                }
                return Some(listener);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if !dormant {
                    eprintln!("[INFO] Port {addr} is already in use. Waiting in dormant mode...");
                    dormant = true;
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    _ = stdin_rx.wait_for(|v| *v) => {
                        eprintln!("[INFO] stdin closed while dormant. Exiting.");
                        return None;
                    }
                }
            }
            Err(e) => {
                eprintln!("[ERROR] Failed to bind {addr}: {e}");
                std::process::exit(1);
            }
        }
    }
}

async fn shutdown_signal(mut rx: watch::Receiver<bool>, mut stdin_rx: watch::Receiver<bool>) {
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

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = rx.changed() => {},
        _ = sigterm => {},
        _ = stdin_rx.wait_for(|v| *v) => {},
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
