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
    headers: axum::http::HeaderMap,
    Json(req): Json<ExecRequest>,
) -> Response {
    let profile = headers
        .get("x-commando-profile")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "true" || v == "1");
    let internal_req = json!({
        "__rest": "exec",
        "target": req.target,
        "command": req.command,
        "timeout": req.timeout,
        "work_dir": req.work_dir.unwrap_or_default(),
        "__profile": profile,
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
    let target = target.strip_prefix('/').unwrap_or(&target);
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

async fn send_work(state: &crate::streamable::AppState, request: Value) -> Result<Value, String> {
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

#[cfg(test)]
mod tests {
    use crate::types::ExecPage;

    #[test]
    fn exec_page_completed_serialization() {
        let page = ExecPage {
            stdout: "hello".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            duration_ms: Some(150),
            timed_out: None,
            next_page: None,
            _profile: None,
        };
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["stdout"], "hello");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["duration_ms"], 150);
        assert!(json.get("timed_out").is_none());
        assert!(json.get("next_page").is_none());
    }

    #[test]
    fn exec_page_streaming_serialization() {
        let page = ExecPage {
            stdout: "partial".to_string(),
            stderr: String::new(),
            exit_code: None,
            duration_ms: None,
            timed_out: None,
            next_page: Some("abc123".to_string()),
            _profile: None,
        };
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["next_page"], "abc123");
        assert!(json.get("exit_code").is_none());
    }

    #[test]
    fn exec_page_timeout_serialization() {
        let page = ExecPage {
            stdout: "partial".to_string(),
            stderr: String::new(),
            exit_code: Some(124),
            duration_ms: Some(60000),
            timed_out: Some(true),
            next_page: None,
            _profile: None,
        };
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["exit_code"], 124);
        assert_eq!(json["timed_out"], true);
    }
}
