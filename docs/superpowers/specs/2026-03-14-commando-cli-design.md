# Commando CLI — Transparent Remote Execution

## Problem

When Claude Code calls commands via the commando MCP server, the output is returned as MCP `content[0].text` — a single text blob that Claude Code collapses after a few lines. Users can't see command output without manually expanding it. Additionally, multi-page streaming output wastes LLM round-trips: each `commando_output` pagination call costs a full LLM cycle just to follow a `next_page` token.

## Solution

A CLI binary (`commando`) that acts as a transparent pipe to remote machines through the commando gateway. Claude Code calls it via Bash, getting native terminal rendering and zero LLM overhead for streaming pagination.

The CLI talks to a dedicated REST API on the gateway — separate from the MCP endpoint. This avoids MCP protocol constraints and gives full control over the response format.

```
Claude Code (Mac)                    Gateway (akio-commando:9877)
┌──────────────┐                     ┌──────────────────┐    Cap'n Proto
│ MCP client   │──── /mcp ──────────►│ MCP endpoint     │───────────────► agents
│ (list, ping) │                     │ (Claude Code)    │
├──────────────┤                     ├──────────────────┤
│ Bash tool    │──── /api/exec ─────►│ REST endpoint    │───────────────► agents
│ `commando    │◄────────────────────│ (CLI)            │
│  exec ...`   │  stdout/stderr/exit │                  │
└──────────────┘                     └──────────────────┘
```

## CLI Interface

### `commando exec <target> <command> [--timeout <secs>] [--workdir <path>]`

Transparent pipe to a remote machine. Stdout to stdout, stderr to stderr, exit code as process exit code. No metadata, no wrappers.

```bash
$ commando exec akio-lab "ls /tmp"
file1.txt
file2.txt
$ echo $?
0

$ commando exec akio-lab "ls /nonexistent"
ls: cannot access '/nonexistent': No such file or directory
$ echo $?
2
```

### `commando list`

List available targets with status.

```bash
$ commando list
akio-lab    running  10.0.0.5
akio-garage running  10.0.0.6
```

### `commando ping <target>`

Health check a target.

```bash
$ commando ping akio-lab
pong from akio-lab in 5ms (v0.4.1)
```

## Configuration

Environment variables only — shared with Claude Code's MCP config:

```bash
export COMMANDO_URL="http://akio-commando:9877"
export COMMANDO_API_KEY="your-api-key"
```

Both the CLI and Claude Code's MCP server reference the same env vars. No config file duplication.

`COMMANDO_URL` is the gateway base URL (no path). The CLI appends `/api/...`, Claude Code's MCP config appends `/mcp`.

CLI exits with an error message if either variable is missing.

## Gateway Changes

### Architecture: handler refactoring and the LocalSet bridge

The gateway's session map (`Rc<RefCell<SessionMap>>`) and all Cap'n Proto RPC work live inside a `tokio::task::spawn_local` block on a `LocalSet`. Axum handlers run outside that `LocalSet` (dispatched via `tokio::spawn`), so they cannot directly call `build_page()` or touch the `SessionMap`.

The MCP endpoint already solves this with a `WorkItem` channel: handlers send requests through the channel, the LocalSet worker processes them and sends back responses via a oneshot.

**All REST requests — both `POST /api/exec` and `GET /api/exec?page=` — must go through the WorkItem channel.** The `build_page()` function borrows `Rc<RefCell<SessionMap>>` and awaits on `Notify`, so it is inherently LocalSet-bound. Every pagination poll, not just the initial exec, must be dispatched through the channel.

**Handler refactoring required:** The current `handle_exec`, `handle_list`, and `handle_ping` functions return MCP-formatted JSON-RPC responses (with `content[0].text` containing human-readable strings). The REST handlers need structured data (separate `stdout`, `stderr`, `exit_code` fields). This requires extracting core logic into shared internal functions that return structured data, then having MCP and REST formatters wrap it:

```
handle_exec_core()  → ExecPage { stdout, stderr, exit_code, duration_ms, next_page, timed_out }
  ├── format_page_response()  → MCP JSON-RPC (existing, wraps into content[0].text)
  └── format_rest_response()  → REST JSON (new, returns fields directly)

handle_list_core()  → Vec<TargetInfo>
  ├── MCP formatter  → content[0].text with pretty-printed JSON
  └── REST formatter → JSON array

handle_ping_core()  → PingResult { target, latency_ms, version }
  ├── MCP formatter  → content[0].text with "pong from ..."
  └── REST formatter → JSON object
```

This is the main refactoring in the gateway — separating data from presentation.

### New REST API endpoints

Add routes to the existing axum router alongside `/mcp`. Same bearer auth middleware.

#### `POST /api/exec`

Start command execution. Returns first page of output.

**Request:**
```json
{
  "target": "akio-lab",
  "command": "ls /tmp",
  "timeout": 60,
  "work_dir": "/tmp"
}
```

`timeout` and `work_dir` are optional.

**Response (streaming — command still running):**
```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "stdout": "partial output...",
  "stderr": "",
  "next_page": "rotated-token-hex"
}
```

**Response (completed):**
```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "stdout": "file1.txt\nfile2.txt",
  "stderr": "",
  "exit_code": 0,
  "duration_ms": 150
}
```

**Response (non-zero exit):**
```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "stdout": "",
  "stderr": "ls: cannot access '/nonexistent': No such file or directory",
  "exit_code": 2,
  "duration_ms": 50
}
```

**Response (timeout):**
```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "stdout": "partial output before timeout",
  "stderr": "",
  "exit_code": 124,
  "duration_ms": 60000,
  "timed_out": true
}
```

Exit code `124` on timeout (matches GNU `timeout` convention).

**Response (error — unknown target, concurrency limit, missing params):**
```http
HTTP/1.1 400 Bad Request
Content-Type: application/json

{
  "error": "unknown target: foo"
}
```

#### `GET /api/exec?page=<token>`

Fetch next page of streaming output. Same response format as `POST /api/exec`.

**Invalid or expired token:**
```http
HTTP/1.1 400 Bad Request
Content-Type: application/json

{
  "error": "invalid or expired page token"
}
```

#### `GET /api/targets`

List all targets with status.

**Response:**
```http
HTTP/1.1 200 OK
Content-Type: application/json

[
  { "name": "akio-lab", "status": "running", "host": "10.0.0.5" },
  { "name": "akio-garage", "status": "running", "host": "10.0.0.6" }
]
```

#### `GET /api/ping/:target`

Health check a target.

**Response (success):**
```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "target": "akio-lab",
  "latency_ms": 5,
  "version": "0.4.1"
}
```

**Response (failure):**
```http
HTTP/1.1 502 Bad Gateway
Content-Type: application/json

{
  "error": "ping failed: connection refused"
}
```

### Response format rules

- `stdout` and `stderr` are always separate string fields (never merged)
- `next_page` present = command still running, CLI should poll
- `exit_code` present = command completed, this is the final page
- `timed_out` only present when true (omitted when false)
- Errors use appropriate HTTP status codes (400 for client errors, 502 for agent failures)
- All success responses are 200 regardless of the command's exit code (the command ran successfully, even if it exited non-zero)

### MCP tool list changes

Update `commando_list` tool description to guide Claude toward the CLI:

```
"List all available commando targets with their status and IP. To execute commands on a target, use the Bash tool: commando exec <target> '<command>'"
```

Remove `commando_exec` and `commando_output` from `tools/list`. The MCP handlers can remain for backward compatibility or be removed — the CLI does not use them.

`commando_ping` stays unchanged.

### No changes to MCP response format

The `/mcp` endpoint continues to return the existing text format with the footer (`---\nexit_code: ...`). No MCP protocol changes needed.

## CLI Implementation

### Crate: `crates/commando-cli/`

Binary name: `commando` (via `[[bin]] name = "commando"` in Cargo.toml).

**Dependencies:** `reqwest` (HTTP client), `clap` (CLI parsing), `serde_json` (JSON), `serde` (derive), `tokio` (async runtime)

### HTTP client details

All requests to the gateway:
- Base URL: `COMMANDO_URL` env var
- Headers:
  - `Content-Type: application/json` (for POST)
  - `Authorization: Bearer <COMMANDO_API_KEY>`
- Connect timeout: 10 seconds (hardcoded)
- Read timeout: 30 seconds (hardcoded). Must exceed the gateway's `page_timeout_secs` (default 5s) to avoid timing out during normal long-polls. The 30s margin accounts for slow gateway responses under load.
- No client-side polling delay needed: the gateway's `build_page` blocks server-side for up to `page_timeout_secs` (default 5s) before returning when no data is available, so the CLI's tight polling loop is naturally throttled.

### Exec flow

```
1. Read COMMANDO_URL and COMMANDO_API_KEY from env
2. POST /api/exec:
   { "target": T, "command": C, "timeout": N, "work_dir": W }
3. Parse response:
   a. If HTTP error status (4xx/5xx):
      → print error field to stderr, exit 1
   b. If 200:
      → write stdout field to process stdout
      → write stderr field to process stderr
      → if next_page exists:
        GET /api/exec?page=TOKEN, goto 3
      → if exit_code exists:
        exit with that code
4. On network error → print error to stderr, exit 1
```

### Full pagination example

```
CLI: POST /api/exec {"target":"box","command":"long-cmd"}
← 200 {"stdout":"first chunk...","stderr":"","next_page":"abc"}
   CLI prints "first chunk..." to stdout

CLI: GET /api/exec?page=abc
← 200 {"stdout":"more output...","stderr":"","next_page":"def"}
   CLI prints "more output..." to stdout

CLI: GET /api/exec?page=def
← 200 {"stdout":"final output","stderr":"","exit_code":0,"duration_ms":3200}
   CLI prints "final output" to stdout, exits 0
```

### List flow

```
1. GET /api/targets
2. Format and print each target to stdout
3. Exit 0
```

### Ping flow

```
1. GET /api/ping/<target>
2. Print formatted result to stdout
3. Exit 0 on success, 1 on error
```

## What does NOT change

- **Agents** — no changes, completely transparent
- **MCP endpoint** (`/mcp`) — unchanged, same format, same behavior
- **Gateway streaming internals** — page timeout, page max bytes, session management all unchanged. REST endpoints reuse the same `build_page` / session machinery via the existing LocalSet work channel.
- **`commando_ping` and `commando_list` MCP tools** — still available for Claude Code discovery
- **Authentication** — same bearer token, same constant-time comparison on both `/mcp` and `/api/*`
- **`--env` flag** — intentionally omitted from v1. Can be added later if needed.

## Constraints

- The CLI assumes a single gateway instance (no load balancer). Streaming sessions are in-memory on the gateway, and `next_page` tokens are tied to that instance.
- If the CLI is killed mid-stream, the gateway session lingers until the idle timeout (default 60s) fires and cleans it up.
- Interleaved stdout/stderr: the gateway drains stdout first per page, then stderr. Chronological ordering between stdout and stderr within a page is not guaranteed.

## Testing

- Unit tests for CLI arg parsing and response parsing
- Unit tests for REST endpoint routing and response format
- Integration test: CLI → gateway → agent (requires running agent, can reuse existing test infra)
- Verify stdout/stderr separation with commands that produce both
- Verify exit code passthrough for 0, non-zero, and timeout cases
- Verify exit code 124 on timeout
- Verify streaming: long-running command output arrives incrementally
- Verify error handling: HTTP errors, invalid page tokens, network failures
