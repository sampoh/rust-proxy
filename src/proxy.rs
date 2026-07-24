use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::{
        ws::{CloseFrame as AxumCloseFrame, Message as AxumMsg, WebSocket, WebSocketUpgrade},
        FromRequestParts, Request, State,
    },
    http::{HeaderMap, StatusCode},
    response::Response,
};
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use tokio::sync::watch;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        self,
        handshake::client::generate_key,
        protocol::{frame::coding::CloseCode, CloseFrame as TungCloseFrame, Message as TungMsg},
    },
    MaybeTlsStream, WebSocketStream,
};

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

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

pub async fn shutdown_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    let _ = state.shutdown_tx.send(true);
    StatusCode::OK
}

pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Result<Response, StatusCode> {
    if is_websocket_upgrade(req.headers()) {
        return ws_proxy(state, req).await;
    }

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

// ── WebSocket proxy ───────────────────────────────────────────────────────────

async fn ws_proxy(state: Arc<AppState>, req: Request) -> Result<Response, StatusCode> {
    let uri = req.uri().clone();
    let req_headers = req.headers().clone();

    let (mut parts, _body) = req.into_parts();

    let ws_upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .map_err(|_| {
            eprintln!("[WS] WebSocket upgrade extraction failed");
            StatusCode::BAD_REQUEST
        })?;

    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let upstream_url = build_ws_url(&state.target, path_and_query);
    let target_host = state.target_host.clone();

    Ok(ws_upgrade.on_upgrade(move |client_ws| async move {
        match connect_upstream_ws(&upstream_url, &req_headers, &target_host).await {
            Ok(upstream_ws) => tunnel_ws(client_ws, upstream_ws).await,
            Err(e) => eprintln!("[WS] Upstream connect failed for {upstream_url}: {e}"),
        }
    }))
}

fn build_ws_url(target: &str, path_and_query: &str) -> String {
    let base = if let Some(rest) = target.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = target.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        target.to_string()
    };
    format!("{base}{path_and_query}")
}

async fn connect_upstream_ws(
    url: &str,
    req_headers: &HeaderMap,
    target_host: &str,
) -> Result<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    Box<dyn std::error::Error + Send + Sync>,
> {
    // tungstenite 0.24 の connect_async(Request<()>) は Sec-WebSocket-* / Upgrade /
    // Connection / Host を自動生成しないため、こちら側で必ず付与する必要がある。
    // 欠けていると generate_request が "Missing, duplicated or incorrect header
    // sec-websocket-key" を返して接続前にエラーになる。
    let mut builder = tungstenite::http::Request::builder()
        .uri(url)
        .header("host", target_host)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key());

    let conn_headers = connection_listed_headers(req_headers);
    for (name, value) in req_headers {
        let lower = name.as_str().to_lowercase();
        // 自前で組み立てたヘッダは重複させない。Sec-WebSocket-Key/Version はクライアント値を
        // そのまま転送すると鍵検証が壊れるので破棄。Sec-WebSocket-Protocol/Extensions は
        // サブプロトコル・拡張のネゴシエーションに必要なのでクライアント値を転送する。
        if lower == "host"
            || is_hop_by_hop(&lower, &conn_headers)
            || lower == "sec-websocket-key"
            || lower == "sec-websocket-version"
            || lower == "sec-websocket-accept"
        {
            continue;
        }
        if let Ok(v) = value.to_str() {
            builder = builder.header(name.as_str(), v);
        }
    }

    let request = builder.body(())?;
    let (ws_stream, _response) = connect_async(request).await?;
    Ok(ws_stream)
}

async fn tunnel_ws(
    mut client: WebSocket,
    mut upstream: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
) {
    // 両脚に定期 Ping を送る。axum の自動 Pong はクライアント脚しか維持できず、
    // tungstenite の自動 Pong は上流脚しか維持できないため、そのままだと
    // 反対側の中継(nginx など)がアイドルで接続を切って WebSocket 1006 になる。
    // nginx の proxy_read_timeout デフォルト 60s を下回るように 25s に設定。
    let mut keepalive = tokio::time::interval(Duration::from_secs(25));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await; // 即時発火する 1 発目を消費

    // Some(reason) なら「上流側の切断を我々が検知した」ケースなので
    // クライアントに Close フレームを送って 1006 ではなく 1011 で終わらせる。
    let upstream_failure: Option<&'static str> = loop {
        tokio::select! {
            msg = client.recv() => {
                match msg {
                    Some(Ok(m)) => {
                        let is_close = matches!(&m, AxumMsg::Close(_));
                        if let Some(tm) = axum_to_tung(m) {
                            if upstream.send(tm).await.is_err() {
                                break Some("upstream send failed");
                            }
                        }
                        // クライアントからの Close を転送済み。二重 Close を避けるため即終了。
                        if is_close {
                            break None;
                        }
                    }
                    // クライアント発の切断。Close をこちらから送り返す必要はない。
                    _ => break None,
                }
            }
            msg = upstream.next() => {
                match msg {
                    Some(Ok(m)) => {
                        let is_close = matches!(&m, TungMsg::Close(_));
                        if let Some(am) = tung_to_axum(m) {
                            if client.send(am).await.is_err() {
                                break None;
                            }
                        }
                        // 上流からの Close を転送済み。1011 の追加送信は不要。
                        if is_close {
                            break None;
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("[WS] Upstream error: {e}");
                        break Some("upstream error");
                    }
                    None => {
                        eprintln!("[WS] Upstream closed without Close frame");
                        break Some("upstream closed");
                    }
                }
            }
            _ = keepalive.tick() => {
                if upstream.send(TungMsg::Ping(Vec::new())).await.is_err() {
                    break Some("upstream ping failed");
                }
                if client.send(AxumMsg::Ping(Vec::new())).await.is_err() {
                    break None;
                }
            }
        }
    };

    if let Some(reason) = upstream_failure {
        eprintln!("[WS] Sending Close(1011) to client: {reason}");
        let _ = client
            .send(AxumMsg::Close(Some(AxumCloseFrame {
                code: 1011,
                reason: reason.into(),
            })))
            .await;
    }
    let _ = upstream.close(None).await;
}

// Ping は両方向で転送する。
// upstream→client: tungstenite が upstream に auto-Pong を送る。client が受信して Pong を返すが
//   その Pong はプロキシで破棄し upstream には届かない。よって upstream への Pong は 1 回だけ。
// client→upstream: axum が client に auto-Pong を送る。upstream が受信して Pong を返すが
//   その Pong はプロキシで破棄し client には届かない。よって client への Pong は 1 回だけ。
// Pong は転送しない（auto-Pong が既に相手に送られているため重複になる）。
fn axum_to_tung(msg: AxumMsg) -> Option<TungMsg> {
    match msg {
        AxumMsg::Text(t) => Some(TungMsg::Text(t.to_string())),
        AxumMsg::Binary(b) => Some(TungMsg::Binary(b.to_vec())),
        AxumMsg::Ping(data) => Some(TungMsg::Ping(data)),
        AxumMsg::Pong(_) => None,
        AxumMsg::Close(c) => Some(TungMsg::Close(c.map(|f| TungCloseFrame {
            code: CloseCode::from(f.code),
            reason: f.reason,
        }))),
    }
}

fn tung_to_axum(msg: TungMsg) -> Option<AxumMsg> {
    match msg {
        TungMsg::Text(t) => Some(AxumMsg::Text(t.to_string())),
        TungMsg::Binary(b) => Some(AxumMsg::Binary(b.into())),
        TungMsg::Ping(data) => Some(AxumMsg::Ping(data)),
        TungMsg::Pong(_) => None,
        TungMsg::Close(c) => Some(AxumMsg::Close(c.map(|f| AxumCloseFrame {
            code: f.code.into(),
            reason: f.reason,
        }))),
        TungMsg::Frame(_) => None,
    }
}
