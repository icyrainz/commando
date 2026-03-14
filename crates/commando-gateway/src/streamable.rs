use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::GatewayConfig;
use crate::handler;
use crate::registry::Registry;
use crate::session::SessionMap;

/// Work item sent from axum handlers (outside LocalSet) to the RPC worker (inside LocalSet).
pub struct WorkItem {
    pub request: Value,
    pub response_tx: tokio::sync::oneshot::Sender<Option<Value>>,
}

pub type WorkSender = tokio::sync::mpsc::Sender<WorkItem>;

#[derive(Clone)]
pub struct AppState {
    pub work_tx: WorkSender,
}

/// Build the Axum router and spawn the RPC worker that bridges axum handlers
/// to the LocalSet where Cap'n Proto RPC lives.
pub fn build_app(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
) -> Router {
    let (work_tx, mut work_rx) = tokio::sync::mpsc::channel::<WorkItem>(64);
    let api_key = Arc::new(config.server.api_key.clone().unwrap_or_default());

    // RPC worker: runs inside LocalSet, processes JSON-RPC requests.
    // axum::serve dispatches handlers via tokio::spawn (outside LocalSet),
    // so handlers send work here via the channel to bridge the !Send gap.
    let idle_timeout_secs = config.streaming.session_idle_timeout_secs;
    tokio::task::spawn_local(async move {
        let session_map = Rc::new(RefCell::new(SessionMap::new()));

        // Spawn idle cleanup timer
        let cleanup_map = session_map.clone();
        let idle_timeout = Duration::from_secs(idle_timeout_secs);
        tokio::task::spawn_local(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                let expired = cleanup_map.borrow_mut().cleanup_expired(idle_timeout);
                if !expired.is_empty() {
                    info!(
                        count = expired.len(),
                        "cleaned up expired streaming sessions"
                    );
                }
            }
        });

        while let Some(item) = work_rx.recv().await {
            let cfg = config.clone();
            let reg = registry.clone();
            let lim = limiter.clone();
            let smap = session_map.clone();
            tokio::task::spawn_local(async move {
                let result =
                    handler::dispatch_request(&item.request, &cfg, &reg, &lim, &smap).await;
                let _ = item.response_tx.send(result);
            });
        }
    });
    let state = AppState { work_tx };

    let authed_routes = Router::new()
        .route(
            "/mcp",
            post(handle_post).get(handle_get).delete(handle_delete),
        )
        .route(
            "/api/exec",
            post(crate::rest::handle_exec_post).get(crate::rest::handle_exec_get),
        )
        .route("/api/targets", get(crate::rest::handle_targets))
        .route("/api/ping/{*target}", get(crate::rest::handle_ping))
        .layer(middleware::from_fn_with_state(
            api_key,
            bearer_auth_middleware,
        ))
        .with_state(state);

    Router::new()
        .merge(authed_routes)
        .route("/health", get(handle_health))
}

pub async fn run_streamable_server(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
) -> Result<()> {
    let bind = config.server.bind.clone();
    let port = config.server.port;
    let app = build_app(config, registry, limiter);

    let addr = format!("{bind}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    info!(addr = %addr, "Streamable HTTP server listening");

    let shutdown = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
        info!("shutting down Streamable HTTP server");
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    Ok(())
}

async fn bearer_auth_middleware(
    axum::extract::State(api_key): axum::extract::State<Arc<String>>,
    request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token = match auth_header {
        Some(h) if h.starts_with("Bearer ") => &h[7..],
        _ => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            )
                .into_response();
        }
    };

    if token.as_bytes().ct_eq(api_key.as_bytes()).into() {
        next.run(request).await
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response()
    }
}

async fn handle_post(
    axum::extract::State(state): axum::extract::State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    let body_str = String::from_utf8_lossy(&body).into_owned();

    let request: Value = match serde_json::from_str(&body_str) {
        Ok(v) => v,
        Err(e) => {
            let error =
                handler::make_error_response(Value::Null, -32700, &format!("Parse error: {e}"));
            return Json(error).into_response();
        }
    };

    // Reject batch requests (JSON arrays)
    if request.is_array() {
        let error =
            handler::make_error_response(Value::Null, -32600, "batch requests not supported");
        return Json(error).into_response();
    }

    // Notifications (no id or null id) — accept without dispatching
    if request.get("id").is_none() || request["id"].is_null() {
        return axum::http::StatusCode::ACCEPTED.into_response();
    }

    // Send to the LocalSet worker via channel and await response
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    if state
        .work_tx
        .send(WorkItem {
            request,
            response_tx,
        })
        .await
        .is_err()
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(handler::make_error_response(
                Value::Null,
                -32603,
                "worker unavailable",
            )),
        )
            .into_response();
    }

    match response_rx.await {
        Ok(Some(response)) => Json(response).into_response(),
        Ok(None) => axum::http::StatusCode::ACCEPTED.into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(handler::make_error_response(
                Value::Null,
                -32603,
                "worker dropped response",
            )),
        )
            .into_response(),
    }
}

async fn handle_get() -> Response {
    axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response()
}

async fn handle_delete() -> Response {
    axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response()
}

async fn handle_health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}
