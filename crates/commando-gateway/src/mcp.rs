use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::GatewayConfig;
use crate::handler;
use crate::registry::Registry;

/// MCP server loop over stdio. Reads JSON-RPC from stdin, writes responses to stdout.
pub async fn run_stdio_loop(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
) -> Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut lines = stdin.lines();

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
            handler::dispatch_request(&request, &config, &registry, &limiter).await
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
