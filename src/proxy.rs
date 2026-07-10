use std::collections::HashSet;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use futures_util::TryStreamExt;
use tokio::sync::watch;

pub struct AppState {
    pub client: reqwest::Client,
    /// Base URL without trailing slash, e.g. "https://example.com"
    pub target: String,
    /// Value to use for the Host request header, e.g. "example.com"
    pub target_host: String,
    pub shutdown_tx: Arc<watch::Sender<bool>>,
}

// RFC 7230 hop-by-hop headers — never forwarded
static HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "proxy-connection",
];

/// Collect header names listed inside `Connection: <name>, <name>` so they
/// can also be stripped (they are per-hop by definition).
fn connection_listed_headers(headers: &HeaderMap) -> HashSet<String> {
    let mut set = HashSet::new();
    for value in headers.get_all("connection") {
        if let Ok(s) = value.to_str() {
            for part in s.split(',') {
                set.insert(part.trim().to_lowercase());
            }
        }
    }
    set
}

fn is_hop_by_hop(name: &str, extra: &HashSet<String>) -> bool {
    let lower = name.to_lowercase();
    HOP_BY_HOP.contains(&lower.as_str()) || extra.contains(&lower)
}

pub async fn shutdown_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    let _ = state.shutdown_tx.send(true);
    StatusCode::OK
}

pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Result<Response, StatusCode> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let req_headers = req.headers().clone();
    let body_stream = req.into_body().into_data_stream();

    // ── Build target URL ──────────────────────────────────────────────────
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let target_url = format!("{}{}", state.target, path_and_query);

    // ── Build reqwest request ─────────────────────────────────────────────
    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .map_err(|_| StatusCode::METHOD_NOT_ALLOWED)?;

    let mut builder = state.client.request(reqwest_method, &target_url);

    // Forward request headers, stripping hop-by-hop and Host
    let conn_headers = connection_listed_headers(&req_headers);
    for (name, value) in &req_headers {
        let lower = name.as_str().to_lowercase();
        if lower == "host" || is_hop_by_hop(&lower, &conn_headers) {
            continue;
        }
        builder = builder.header(name, value);
    }

    // Host must match the target origin
    builder = builder.header("host", &state.target_host);

    // Stream the request body
    let reqwest_body = reqwest::Body::wrap_stream(
        body_stream.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)),
    );
    builder = builder.body(reqwest_body);

    // ── Send ──────────────────────────────────────────────────────────────
    let upstream = builder.send().await.map_err(|e| {
        eprintln!("[ERROR] Upstream request failed: {e}");
        StatusCode::BAD_GATEWAY
    })?;

    // ── Build response ────────────────────────────────────────────────────
    let status = upstream.status().as_u16();
    let resp_headers = upstream.headers().clone();

    let mut response_builder = Response::builder().status(status);

    // Forward response headers, stripping hop-by-hop
    let resp_conn_headers = connection_listed_headers(&resp_headers);
    for (name, value) in &resp_headers {
        if is_hop_by_hop(name.as_str(), &resp_conn_headers) {
            continue;
        }
        response_builder = response_builder.header(name, value);
    }

    // Stream response body back to the client
    let resp_body = Body::from_stream(upstream.bytes_stream());

    response_builder.body(resp_body).map_err(|e| {
        eprintln!("[ERROR] Failed to build response: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}
