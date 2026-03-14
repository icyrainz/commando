use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "commando",
    version,
    about = "Commando CLI — transparent remote execution"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Execute a command on a remote target
    Exec {
        /// Target machine name
        target: String,
        /// Command to execute
        command: String,
        /// Timeout in seconds
        #[arg(long)]
        timeout: Option<u32>,
        /// Working directory on target
        #[arg(long)]
        workdir: Option<String>,
    },
    /// List available targets
    List,
    /// Ping a target
    Ping {
        /// Target machine name
        target: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let exit_code = rt.block_on(run(cli));
    std::process::exit(exit_code);
}

async fn run(cli: Cli) -> i32 {
    let url = match std::env::var("COMMANDO_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("error: COMMANDO_URL environment variable not set");
            return 1;
        }
    };
    let api_key = match std::env::var("COMMANDO_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            eprintln!("error: COMMANDO_API_KEY environment variable not set");
            return 1;
        }
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        // No read_timeout — the gateway's page_timeout_secs (default 5s) controls
        // how long each page poll blocks. A client-side read_timeout would race
        // against the gateway timeout and kill long-running commands.
        .build()
        .expect("failed to build HTTP client");
    match cli.command {
        Commands::Exec {
            target,
            command,
            timeout,
            workdir,
        } => {
            cmd_exec(
                &client,
                &url,
                &api_key,
                &target,
                &command,
                timeout,
                workdir.as_deref(),
            )
            .await
        }
        Commands::List => cmd_list(&client, &url, &api_key).await,
        Commands::Ping { target } => cmd_ping(&client, &url, &api_key, &target).await,
    }
}

async fn cmd_exec(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    target: &str,
    command: &str,
    timeout: Option<u32>,
    workdir: Option<&str>,
) -> i32 {
    use std::io::Write;
    let url = format!("{base_url}/api/exec");
    let mut body = serde_json::json!({"target": target, "command": command});
    if let Some(t) = timeout {
        body["timeout"] = serde_json::json!(t);
    }
    if let Some(w) = workdir {
        body["work_dir"] = serde_json::json!(w);
    }

    let resp = match client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = body["error"].as_str().unwrap_or("unknown error");
        eprintln!("error: {msg} (HTTP {status})");
        return 1;
    }
    let mut json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: failed to parse response: {e}");
            return 1;
        }
    };

    loop {
        if let Some(stdout) = json["stdout"].as_str()
            && !stdout.is_empty()
        {
            print!("{stdout}");
            let _ = std::io::stdout().flush();
        }
        if let Some(stderr) = json["stderr"].as_str()
            && !stderr.is_empty()
        {
            eprint!("{stderr}");
            let _ = std::io::stderr().flush();
        }
        if let Some(exit_code) = json["exit_code"].as_i64() {
            return exit_code as i32;
        }
        let next_page = match json["next_page"].as_str() {
            Some(p) => p.to_string(),
            None => {
                eprintln!("error: no exit_code and no next_page in response");
                return 1;
            }
        };
        let page_url = format!("{base_url}/api/exec?page={next_page}");
        let resp = match client.get(&page_url).bearer_auth(api_key).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let msg = body["error"].as_str().unwrap_or("unknown error");
            eprintln!("error: {msg} (HTTP {status})");
            return 1;
        }
        json = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                eprintln!("error: failed to parse response: {e}");
                return 1;
            }
        };
    }
}

async fn cmd_list(client: &reqwest::Client, base_url: &str, api_key: &str) -> i32 {
    let url = format!("{base_url}/api/targets");
    let resp = match client.get(&url).bearer_auth(api_key).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    if !resp.status().is_success() {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = body["error"].as_str().unwrap_or("unknown error");
        eprintln!("error: {msg}");
        return 1;
    }
    let targets: Vec<serde_json::Value> = match resp.json().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    for t in &targets {
        let name = t["name"].as_str().unwrap_or("?");
        let status = t["status"].as_str().unwrap_or("?");
        let host = t["host"].as_str().unwrap_or("");
        println!("{name}\t{status}\t{host}");
    }
    0
}

async fn cmd_ping(client: &reqwest::Client, base_url: &str, api_key: &str, target: &str) -> i32 {
    let url = format!("{base_url}/api/ping/{target}");
    let resp = match client.get(&url).bearer_auth(api_key).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    if !resp.status().is_success() {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = body["error"].as_str().unwrap_or("unknown error");
        eprintln!("error: {msg}");
        return 1;
    }
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let target = json["target"].as_str().unwrap_or("?");
    let latency = json["latency_ms"].as_u64().unwrap_or(0);
    let version = json["version"].as_str().unwrap_or("?");
    println!("pong from {target} in {latency}ms (v{version})");
    0
}
