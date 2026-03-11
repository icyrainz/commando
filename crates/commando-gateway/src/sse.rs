use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tracing::info;

use crate::config::GatewayConfig;
use crate::handler;
use crate::registry::Registry;

type SseEvent = std::result::Result<Event, Infallible>;
type SessionMap = Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<String>>>>;

/// Work item sent from axum handlers (outside LocalSet) to the RPC worker (inside LocalSet).
struct WorkItem {
    request: Value,
    response_tx: tokio::sync::oneshot::Sender<Option<Value>>,
}

type WorkSender = tokio::sync::mpsc::Sender<WorkItem>;

#[derive(Clone)]
struct AppState {
    sessions: SessionMap,
    work_tx: WorkSender,
}

#[derive(Deserialize)]
struct MessageQuery {
    session_id: String,
}

pub async fn run_sse_server(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
) -> Result<()> {
    let (work_tx, mut work_rx) = tokio::sync::mpsc::channel::<WorkItem>(64);

    // RPC worker: runs inside LocalSet, processes JSON-RPC requests concurrently.
    // axum::serve dispatches handlers via tokio::spawn (outside LocalSet),
    // so handlers send work here via the channel to bridge the !Send gap.
    let worker_config = config.clone();
    let worker_registry = registry.clone();
    let worker_limiter = limiter.clone();
    tokio::task::spawn_local(async move {
        while let Some(item) = work_rx.recv().await {
            let cfg = worker_config.clone();
            let reg = worker_registry.clone();
            let lim = worker_limiter.clone();
            tokio::task::spawn_local(async move {
                let result =
                    handler::dispatch_request(&item.request, &cfg, &reg, &lim).await;
                let _ = item.response_tx.send(result);
            });
        }
    });

    let state = AppState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        work_tx,
    };

    let app = Router::new()
        .route("/sse", get(handle_sse))
        .route("/messages", post(handle_message))
        .route("/health", get(handle_health))
        .with_state(state);

    let addr = format!("{}:{}", config.server.bind, config.server.port);
    let listener = TcpListener::bind(&addr).await?;
    info!(addr = %addr, "SSE server listening");

    let shutdown = async {
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
        info!("shutting down SSE server");
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    Ok(())
}

async fn handle_sse(State(state): State<AppState>) -> impl IntoResponse {
    let session_id = uuid::Uuid::new_v4().to_string().replace("-", "");

    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(32);
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<String>(32);

    state
        .sessions
        .lock()
        .unwrap()
        .insert(session_id.clone(), msg_tx);

    let sessions = state.sessions.clone();
    let sid = session_id.clone();

    // tokio::spawn (not spawn_local) — only shuffles strings, no !Send types
    tokio::spawn(async move {
        let endpoint_event = Event::default()
            .event("endpoint")
            .data(format!("/messages?session_id={sid}"));
        if tx.send(Ok(endpoint_event)).await.is_err() {
            sessions.lock().unwrap().remove(&sid);
            return;
        }

        while let Some(data) = msg_rx.recv().await {
            let event = Event::default().event("message").data(data);
            if tx.send(Ok(event)).await.is_err() {
                break;
            }
        }

        sessions.lock().unwrap().remove(&sid);
        info!(session_id = %sid, "SSE session closed");
    });

    info!(session_id = %session_id, "SSE session opened");

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

async fn handle_message(
    State(state): State<AppState>,
    Query(query): Query<MessageQuery>,
    body: axum::body::Bytes,
) -> Response {
    let body = String::from_utf8_lossy(&body).into_owned();

    let sender = {
        let sessions = state.sessions.lock().unwrap();
        match sessions.get(&query.session_id) {
            Some(tx) => tx.clone(),
            None => {
                return (
                    axum::http::StatusCode::NOT_FOUND,
                    Json(json!({"error": "session not found"})),
                )
                    .into_response();
            }
        }
    };

    let request: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            let error_response = handler::make_error_response(
                Value::Null,
                -32700,
                &format!("Parse error: {e}"),
            );
            let json_str = serde_json::to_string(&error_response).unwrap_or_default();
            if sender.send(json_str).await.is_err() {
                state.sessions.lock().unwrap().remove(&query.session_id);
            }
            return axum::http::StatusCode::ACCEPTED.into_response();
        }
    };

    // Notifications (no id) — acknowledge without dispatching
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
            "Worker unavailable",
        )
            .into_response();
    }

    if let Ok(Some(response)) = response_rx.await {
        let json_str = serde_json::to_string(&response).unwrap_or_default();
        if sender.send(json_str).await.is_err() {
            state.sessions.lock().unwrap().remove(&query.session_id);
        }
    }

    axum::http::StatusCode::ACCEPTED.into_response()
}

async fn handle_health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}
