//! `MODEL_API_KEY=... cargo run -p agora-server` then open <http://127.0.0.1:8080>
//!
//! Settings (all optional): `AGORA_ADDR` (default `127.0.0.1:8080`), `AGORA_TOKEN`
//! (required to listen on anything but loopback), `AGORA_MODEL`, `AGORA_CAP_USD`
//! (a spending cap per run; none by default, but set one before opening the hub to
//! others), `AGORA_PARALLEL` (default 4), `AGORA_MAX_ACTIVE` (default
//! 2), `AGORA_JOURNAL` (default `agora-journal.db`), and `AGORA_REVIEWER=baseten`
//! with `BASETEN_API_KEY` to review on Muse Spark Open. `AGORA_CHAT_CAP_USD` (default
//! 0.50) caps what the office chat spends while the server runs.

use std::net::SocketAddr;
use std::path::PathBuf;

use agora_engine::Remote;
use agora_muse::MuseClient;
use agora_server::{HubConfig, app};

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(v) => v.trim().parse().map_err(|_| format!("{name}={v:?} is not a valid value")),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = env_or("AGORA_ADDR", SocketAddr::from(([127, 0, 0, 1], 8080)))?;
    let token = std::env::var("AGORA_TOKEN").ok().filter(|t| !t.trim().is_empty());
    if !addr.ip().is_loopback() && token.is_none() {
        return Err("set AGORA_TOKEN before listening beyond localhost: starting a run spends money".into());
    }
    let reviewer = match env_or("AGORA_REVIEWER", String::new())?.as_str() {
        "" | "meta" => None,
        "baseten" => Some(Remote {
            client: MuseClient::baseten_from_env()?,
            model: // Baseten's docs say `meta-models/muse-spark-open`; with a hackathon key that
                // id answers 403 "check the api-key", and only the dated id works (checked
                // live 2026-10-30).
                env_or("AGORA_REVIEWER_MODEL", "meta-models/muse-spark-open-1026".to_string())?,
        }),
        other => return Err(format!("AGORA_REVIEWER={other:?}: use \"baseten\" or leave it unset").into()),
    };
    let config = HubConfig {
        muse: MuseClient::from_env()?,
        model: env_or("AGORA_MODEL", "muse-spark-1.3".to_string())?,
        cap_usd: env_or("AGORA_CAP_USD", f64::INFINITY)?,
        max_parallel: env_or("AGORA_PARALLEL", 4)?,
        reviewer,
        journal: Some(env_or("AGORA_JOURNAL", PathBuf::from("agora-journal.db"))?),
        token,
        max_active: env_or("AGORA_MAX_ACTIVE", 2)?,
        chat_cap_usd: env_or("AGORA_CHAT_CAP_USD", 0.50)?,
    };
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("Agora on http://{addr}");
    axum::serve(listener, app(config))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
