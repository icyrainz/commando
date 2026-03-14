use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::audit::AuditLogger;
use crate::config::GatewayConfig;
use crate::handler;
use crate::registry::Registry;
use crate::session::SessionMap;

/// MCP server loop over stdio. Reads JSON-RPC from stdin, writes responses to stdout.
pub async fn run_stdio_loop(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
    audit: Arc<AuditLogger>,
) -> Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut lines = stdin.lines();

    let session_map = Rc::new(RefCell::new(SessionMap::new()));

    // Spawn idle cleanup timer
    let cleanup_map = session_map.clone();
    let idle_timeout = Duration::from_secs(config.streaming.session_idle_timeout_secs);
    tokio::task::spawn_local(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            let expired = cleanup_map.borrow_mut().cleanup_expired(idle_timeout);
            if !expired.is_empty() {
                tracing::info!(
                    count = expired.len(),
                    "cleaned up expired streaming sessions"
                );
            }
        }
    });

    while let Ok(Some(line)) = lines.next_line().await as tokio::io::Result<Option<String>> {
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err =
                    handler::make_error_response(Value::Null, -32700, &format!("Parse error: {e}"));
                write_response(&mut stdout, &err).await?;
                continue;
            }
        };

        if let Some(response) =
            handler::dispatch_request(&request, &config, &registry, &limiter, &session_map, &audit)
                .await
        {
            write_response(&mut stdout, &response).await?;
        }
    }

    Ok(())
}

async fn write_response(stdout: &mut tokio::io::Stdout, response: &Value) -> Result<()> {
    let json = serde_json::to_string(response)?;
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}
