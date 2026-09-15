mod auth;
mod config;
mod discovery;
mod rpc;
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
        Some(Command::Probe) | None => probe(&origin, &cfg).await,
    }
}

async fn probe(origin: &str, cfg: &config::Config) -> Result<()> {
    let token = cfg
        .token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("no token stored; run `tria pair <credential>` first"))?;
    let ticket = auth::websocket_ticket(origin, token).await?;
    let client = rpc::RpcClient::connect(origin, &ticket).await?;
    let config: serde_json::Value = client.call("server.getConfig", serde_json::json!({})).await?;
    println!("server config keys: {:?}", config.as_object().map(|o| o.keys().collect::<Vec<_>>()));
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
                println!("projects: {}", snap["projects"].as_array().map_or(0, |a| a.len()));
                println!("threads:  {}", snap["threads"].as_array().map_or(0, |a| a.len()));
                for t in snap["threads"].as_array().into_iter().flatten().take(10) {
                    println!("  - {}  [{}]", t["title"].as_str().unwrap_or(""), t["session"]["status"].as_str().unwrap_or(""));
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
