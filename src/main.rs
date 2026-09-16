mod app;
mod auth;
mod commands;
mod composer;
mod config;
mod discovery;
mod model;
mod question;
mod rpc;
mod session;
mod state;
mod term;
mod timeline;
mod ui;
mod vim;
mod wire;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tria", about = "A terminal chat client for T3 Code")]
struct Cli {
    /// Server origin, e.g. http://127.0.0.1:3773. Defaults to the local server runtime file.
    #[arg(long, global = true)]
    url: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Exchange a pairing credential (or /pair#token= URL) for a stored bearer token.
    Pair { credential: String },
    /// Connect and print the server config plus the project and thread list, then exit.
    Probe,
    /// Open a thread through the supervisor and print reduced state for a few seconds.
    Dump {
        thread_id: String,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = config::Config::load()?;
    let origin = match cli.url.or_else(|| cfg.url.clone()) {
        Some(url) => url,
        None => discovery::local_origin()?,
    };
    match cli.command {
        Some(Command::Pair { credential }) => {
            let token = auth::exchange_pairing_credential(&origin, &credential).await?;
            cfg.url = Some(origin.clone());
            cfg.token = Some(token.access_token);
            cfg.save()?;
            println!("Paired with {origin}; scopes: {}", token.scope);
            Ok(())
        }
        Some(Command::Probe) => probe(&origin, &cfg).await,
        None => {
            let token = cfg.token.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "no token stored; run `tria pair <credential>` first (mint one with `t3 pair`)"
                )
            })?;
            init_logging()?;
            let launch = app::Launch {
                git_command: cfg.git_command(),
                editor: cfg.editor(),
            };
            app::run(origin, token, launch).await
        }
        Some(Command::Dump { thread_id, seconds }) => {
            dump(&origin, &cfg, &thread_id, seconds).await
        }
    }
}

async fn probe(origin: &str, cfg: &config::Config) -> Result<()> {
    let token = cfg
        .token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("no token stored; run `tria pair <credential>` first"))?;
    let ticket = auth::websocket_ticket(origin, token).await?;
    let client = rpc::RpcClient::connect(origin, &ticket).await?;
    let config: serde_json::Value = client
        .call("server.getConfig", serde_json::json!({}))
        .await?;
    println!(
        "server config keys: {:?}",
        config.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );
    if let Some(path) = std::env::var_os("TRIA_DUMP_CONFIG") {
        std::fs::write(path, serde_json::to_string_pretty(&config)?)?;
    }
    let mut shell = client
        .subscribe(
            "orchestration.subscribeShell",
            serde_json::json!({ "requestCompletionMarker": true }),
        )
        .await?;
    while let Some(item) = shell.next().await {
        let item = item?;
        let kind = item.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
        match kind {
            "snapshot" => {
                let snap = &item["snapshot"];
                println!(
                    "projects: {}",
                    snap["projects"].as_array().map_or(0, |a| a.len())
                );
                println!(
                    "threads:  {}",
                    snap["threads"].as_array().map_or(0, |a| a.len())
                );
                for t in snap["threads"].as_array().into_iter().flatten().take(10) {
                    println!(
                        "  - {}  [{}]",
                        t["title"].as_str().unwrap_or(""),
                        t["session"]["status"].as_str().unwrap_or("")
                    );
                }
            }
            "synchronized" => {
                println!("synchronized");
                break;
            }
            other => println!("shell event: {other}"),
        }
    }
    Ok(())
}

async fn dump(origin: &str, cfg: &config::Config, thread_id: &str, seconds: u64) -> Result<()> {
    let token = cfg
        .token
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no token stored; run `tria pair <credential>` first"))?;
    let (handle, mut updates) = session::spawn(origin.to_string(), token);
    let mut shell = state::Shell::default();
    let mut thread: Option<state::ThreadState> = None;
    handle.open_thread(thread_id);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        let update = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            update = updates.recv() => match update { Some(u) => u, None => break },
        };
        match update {
            session::Update::Status(status) => println!("status: {status:?}"),
            session::Update::Config(config) => println!(
                "config: {} providers, pagination={}",
                config.providers.len(),
                config.thread_snapshot_pagination
            ),
            session::Update::Shell(item) => {
                shell.apply(item);
                if shell.synchronized {
                    println!(
                        "shell: {} projects, {} threads, seq {:?}",
                        shell.projects.len(),
                        shell.threads.len(),
                        shell.last_sequence
                    );
                }
            }
            session::Update::Thread { item, .. } => match (&mut thread, item) {
                (None, model::ThreadItem::Snapshot { snapshot }) => {
                    let t = state::ThreadState::from_snapshot(snapshot);
                    println!(
                        "thread snapshot: {} messages, {} activities, has_more={}, seq {}",
                        t.detail.messages.len(),
                        t.detail.activities.len(),
                        t.has_more,
                        t.last_sequence
                    );
                    println!(
                        "  running={} pending approvals={} plan steps={}",
                        t.is_running(),
                        t.pending_approvals().len(),
                        t.active_plan().len()
                    );
                    thread = Some(t);
                }
                (Some(t), item) => {
                    if let model::ThreadItem::Event { event } = &item {
                        println!("event {} seq {}", event.kind, event.sequence);
                    }
                    t.apply(item);
                }
                (None, _) => {}
            },
            session::Update::OlderPage { snapshot, .. } => {
                println!("older page: {} messages", snapshot.thread.messages.len())
            }
            session::Update::ThreadStreamError { error } => {
                println!("thread stream error: {error}")
            }
            session::Update::Terminals(_)
            | session::Update::TerminalStream(_)
            | session::Update::Vcs(_) => {}
            session::Update::Error(error) => println!("error: {error}"),
        }
    }
    if let Some(last) = thread.as_ref().and_then(|t| t.detail.messages.last()) {
        println!(
            "last message ({}): {:?}",
            last.role,
            last.text.chars().take(120).collect::<String>()
        );
    }
    Ok(())
}

/// Log to a file under the state directory; the terminal is owned by the UI.
fn init_logging() -> Result<()> {
    let dir = config::Config::path()?
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("tria.log"))?;
    let filter =
        tracing_subscriber::EnvFilter::try_from_env("TRIA_LOG").unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .init();
    Ok(())
}
