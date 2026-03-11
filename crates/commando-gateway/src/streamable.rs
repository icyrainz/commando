use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing::info;

use crate::config::GatewayConfig;
use crate::handler;
use crate::registry::Registry;

/// Work item sent from axum handlers (outside LocalSet) to the RPC worker (inside LocalSet).
struct WorkItem {
    request: Value,
    response_tx: tokio::sync::oneshot::Sender<Option<Value>>,
}

type WorkSender = tokio::sync::mpsc::Sender<WorkItem>;

#[derive(Clone)]
struct AppState {
    work_tx: WorkSender,
}

/// Build the Axum router and spawn the RPC worker that bridges axum handlers
/// to the LocalSet where Cap'n Proto RPC lives.
pub fn build_app(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
) -> Router {
    let (work_tx, mut work_rx) = tokio::sync::mpsc::channel::<WorkItem>(64);

    // RPC worker: runs inside LocalSet, processes JSON-RPC requests.
    // axum::serve dispatches handlers via tokio::spawn (outside LocalSet),
    // so handlers send work here via the channel to bridge the !Send gap.
    tokio::task::spawn_local(async move {
        while let Some(item) = work_rx.recv().await {
            let cfg = config.clone();
            let reg = registry.clone();
            let lim = limiter.clone();
            tokio::task::spawn_local(async move {
                let result =
                    handler::dispatch_request(&item.request, &cfg, &reg, &lim).await;
                let _ = item.response_tx.send(result);
            });
        }
    });

    let state = AppState { work_tx };

    Router::new()
        .route("/mcp", post(handle_post).get(handle_get).delete(handle_delete))
        .route("/health", get(handle_health))
        .with_state(state)
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
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
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

async fn handle_post(
    axum::extract::State(state): axum::extract::State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    let body_str = String::from_utf8_lossy(&body).into_owned();

    let request: Value = match serde_json::from_str(&body_str) {
        Ok(v) => v,
        Err(e) => {
            let error = handler::make_error_response(
                Value::Null,
                -32700,
                &format!("Parse error: {e}"),
            );
            return Json(error).into_response();
        }
    };

    // Reject batch requests (JSON arrays)
    if request.is_array() {
        let error = handler::make_error_response(
            Value::Null,
            -32600,
            "batch requests not supported",
        );
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
