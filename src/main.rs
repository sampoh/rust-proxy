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

    // Build one reqwest::Client per target so each can have its own timeout.
    // The connection pool is per-client but that's fine — different origins
    // wouldn't share connections anyway.
    let client = build_client(cli.timeout);
    let client2 = cli.target2.as_ref().map(|_| build_client(cli.timeout2.unwrap_or(cli.timeout)));

    let listen_addr: SocketAddr = cli.listen.parse().unwrap_or_else(|_| {
        eprintln!("[ERROR] Invalid listen address: {}", cli.listen);
        std::process::exit(1);
    });

    // Resolve --listen2 only when --target2 is present. If target2 is unset,
    // listen2 is silently ignored per spec.
    let second = cli.target2.as_ref().map(|target2| {
        let addr = match cli.listen2.as_ref() {
            Some(s) => s.parse().unwrap_or_else(|_| {
                eprintln!("[ERROR] Invalid listen2 address: {s}");
                std::process::exit(1);
            }),
            None => {
                // Guard against u16 overflow when --listen uses port 65535.
                let next_port = listen_addr.port().checked_add(1).unwrap_or_else(|| {
                    eprintln!(
                        "[ERROR] Cannot derive listen2 from listen port {}: overflow. Specify --listen2 explicitly.",
                        listen_addr.port()
                    );
                    std::process::exit(1);
                });
                SocketAddr::new(listen_addr.ip(), next_port)
            }
        };
        (target2.clone(), addr)
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

    // Shared shutdown channel — either /__shutdown endpoint stops both servers.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);

    let primary = spawn_proxy(
        cli.target.clone(),
        listen_addr,
        client.clone(),
        shutdown_tx.clone(),
        shutdown_rx.clone(),
        stdin_rx.clone(),
    );

    let secondary = match second {
        Some((target2, addr2)) => Some(spawn_proxy(
            target2,
            addr2,
            client2.expect("client2 built when target2 is set"),
            shutdown_tx.clone(),
            shutdown_rx.clone(),
            stdin_rx.clone(),
        )),
        None => None,
    };

    let _ = primary.await;
    if let Some(handle) = secondary {
        let _ = handle.await;
    }
}

/// Build a reqwest::Client with the shared connection-pool / decompression /
/// redirect policies. `timeout_secs` of 0 disables the request timeout.
///
/// - HTTP/2 is negotiated via ALPN when available.
/// - Redirects are NOT followed; the original response is forwarded as-is.
/// - Auto-decompression is disabled so Content-Encoding is preserved.
/// - The connection pool is kept alive aggressively.
fn build_client(timeout_secs: u64) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd();

    if timeout_secs > 0 {
        builder = builder.timeout(Duration::from_secs(timeout_secs));
    }

    builder.build().unwrap_or_else(|e| {
        eprintln!("[ERROR] Failed to build HTTP client: {e}");
        std::process::exit(1);
    })
}

/// Spawn a proxy server as a background task. Each instance owns its own
/// AppState and TcpListener but shares the reqwest client, shutdown signal
/// and stdin-close signal.
fn spawn_proxy(
    target: String,
    addr: SocketAddr,
    client: reqwest::Client,
    shutdown_tx: Arc<watch::Sender<bool>>,
    shutdown_rx: watch::Receiver<bool>,
    stdin_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let target = target.trim_end_matches('/').to_string();

        let target_host = match extract_host(&target) {
            Some(h) => h,
            None => {
                eprintln!("[ERROR] Could not parse host from target URL: {target}");
                std::process::exit(1);
            }
        };

        let listener = match wait_for_port(addr, stdin_rx.clone()).await {
            Some(l) => l,
            None => return,
        };

        println!("Listening on {addr}");
        println!("Target: {target}");

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
                eprintln!("[ERROR] Server error on {addr}: {e}");
                std::process::exit(1);
            });
    })
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
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => { sig.recv().await; }
            Err(e) => {
                eprintln!("[ERROR] Failed to register SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
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
