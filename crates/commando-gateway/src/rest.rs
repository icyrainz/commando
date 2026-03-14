use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
pub struct ExecRequest {
    pub target: String,
    pub command: String,
    pub timeout: Option<u32>,
    pub work_dir: Option<String>,
}

#[derive(Deserialize)]
pub struct PageQuery {
    pub page: String,
}

fn error_response(status: axum::http::StatusCode, msg: &str) -> Response {
    (status, Json(json!({"error": msg}))).into_response()
}

pub async fn handle_exec_post(
    State(state): State<crate::streamable::AppState>,
    Json(req): Json<ExecRequest>,
) -> Response {
    let internal_req = json!({
        "__rest": "exec",
        "target": req.target,
        "command": req.command,
        "timeout": req.timeout,
        "work_dir": req.work_dir.unwrap_or_default(),
    });
    match send_work(&state, internal_req).await {
        Ok(resp) => {
            if resp.get("error").is_some() {
                let status = if resp["_gateway"].as_bool().unwrap_or(false) {
                    axum::http::StatusCode::BAD_GATEWAY
                } else {
                    axum::http::StatusCode::BAD_REQUEST
                };
                let clean = json!({"error": resp["error"]});
                return (status, Json(clean)).into_response();
            }
            Json(resp).into_response()
        }
        Err(e) => error_response(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

pub async fn handle_exec_get(
    State(state): State<crate::streamable::AppState>,
    Query(query): Query<PageQuery>,
) -> Response {
    let internal_req = json!({
        "__rest": "output",
        "page": query.page,
    });
    match send_work(&state, internal_req).await {
        Ok(resp) => {
            if resp.get("error").is_some() {
                return (axum::http::StatusCode::BAD_REQUEST, Json(resp)).into_response();
            }
            Json(resp).into_response()
        }
        Err(e) => error_response(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

pub async fn handle_targets(State(state): State<crate::streamable::AppState>) -> Response {
    let internal_req = json!({"__rest": "list"});
    match send_work(&state, internal_req).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => error_response(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

pub async fn handle_ping(
    State(state): State<crate::streamable::AppState>,
    axum::extract::Path(target): axum::extract::Path<String>,
) -> Response {
    let internal_req = json!({"__rest": "ping", "target": target});
    match send_work(&state, internal_req).await {
        Ok(resp) => {
            if resp.get("error").is_some() {
                let status = if resp["_gateway"].as_bool().unwrap_or(false) {
                    axum::http::StatusCode::BAD_GATEWAY
                } else {
                    axum::http::StatusCode::BAD_REQUEST
                };
                let clean = json!({"error": resp["error"]});
                return (status, Json(clean)).into_response();
            }
            Json(resp).into_response()
        }
        Err(e) => error_response(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

async fn send_work(
    state: &crate::streamable::AppState,
    request: Value,
) -> Result<Value, String> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    state
        .work_tx
        .send(crate::streamable::WorkItem {
            request,
            response_tx,
        })
        .await
        .map_err(|_| "worker unavailable".to_string())?;
    response_rx
        .await
        .map_err(|_| "worker dropped response".to_string())?
        .ok_or_else(|| "no response from worker".to_string())
}
