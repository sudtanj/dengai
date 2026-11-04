//! dengai: a tiny single-account Steam idler (card farming, key redeem, free licenses).

mod bot;
mod web;

use dengai::api::{self, Shared, Status};
use dengai::config::Config;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use tracing::{error, info, Level};

const HELP: &str = "dengai: single-account Steam card farmer, key redeemer and free-license claimer

Usage: dengai [--help | --version]

Configured through DENGAI_* environment variables and an optional TOML file
(DENGAI_CONFIG, default ./dengai.toml). See README.md for every option.";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("-h" | "--help") => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Some("-V" | "--version") => {
            println!("dengai {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let cfg = match Config::load(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dengai: {e}");
            return ExitCode::FAILURE;
        }
    };
    let level = cfg.log.parse::<Level>().unwrap_or(Level::INFO);
    tracing_subscriber::fmt().with_max_level(level).with_target(false).init();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(run(cfg))
}

async fn run(cfg: Config) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(&cfg.data_dir) {
        error!("creating {}: {e}", cfg.data_dir.display());
        return ExitCode::FAILURE;
    }
    let cfg = Arc::new(cfg);
    let shared = Shared::new(Mutex::new(Status::default()));
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let (pause_tx, pause_rx) = tokio::sync::watch::channel(false);
    let bot = match bot::Bot::new(cfg.clone(), shared.clone(), rx, pause_rx) {
        Ok(b) => b,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if cfg.api_token.is_empty() {
        info!("DENGAI_API_TOKEN not set, HTTP API disabled");
    } else {
        let (bind, token) = (cfg.api_bind.clone(), cfg.api_token.clone());
        tokio::spawn(async move {
            if let Err(e) = api::serve(bind, token, shared, tx, pause_tx).await {
                error!("API: {e}");
            }
        });
    }
    tokio::select! {
        _ = bot.run() => {}
        _ = shutdown() => info!("shutting down"),
    }
    ExitCode::SUCCESS
}

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
