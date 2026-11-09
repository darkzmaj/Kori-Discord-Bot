// serenity::Error is large by design; every Discord call returns it.
#![allow(clippy::result_large_err)]

mod bot;
mod config;
mod db;
mod discord;
mod health;
mod logger;
mod services;
#[cfg(test)]
mod test_support;

use bot::{KoriBot, Handler};
use db::repositories::KoriRepository;
use services::audit_log_service::AuditEvent;
use std::sync::{Arc, OnceLock};

#[tokio::main]
async fn main() {
    let config = config::Config::from_env();
    if let Err(error) = run(config).await {
        logger::error("Kori failed to start", ctx! {"error" => error.to_string()});
        std::process::exit(1);
    }
}

async fn sigterm() {
    #[cfg(unix)]
    {
        if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            signal.recv().await;
            return;
        }
    }
    std::future::pending::<()>().await
}

async fn run(config: config::Config) -> anyhow::Result<()> {
    config.require()?;
    let repo = Arc::new(KoriRepository::new(db::create_database(&config.database_path)?));
    let slot = Arc::new(OnceLock::new());
    let mut client = serenity::Client::builder(&config.token, bot::intents())
        .event_handler(Handler { bot: slot.clone() })
        .await?;
    let discord = discord::Discord { http: client.http.clone(), cache: client.cache.clone() };
    let bot = Arc::new(KoriBot::new(repo, config.application_id.clone(), &config, discord));
    let _ = slot.set(bot.clone());
    if let Some(addr) = std::env::var("KORI_HEALTH_ADDR").ok().filter(|v| !v.trim().is_empty()) {
        let addr: std::net::SocketAddr = addr.trim().parse()?;
        tokio::spawn(async move {
            if let Err(error) = health::serve(addr).await {
                logger::error("Health check could not start", ctx! {"error" => error.to_string()});
            }
        });
    }
    let shards = client.shard_manager.clone();
    let mut gateway = tokio::spawn(async move { client.start().await });

    let (signal, exit_code) = tokio::select! {
        result = &mut gateway => {
            // Login/gateway failure before any shutdown request.
            result??;
            anyhow::bail!("Discord gateway stopped unexpectedly.");
        }
        _ = tokio::signal::ctrl_c() => ("SIGINT", 0),
        _ = sigterm() => ("SIGTERM", 0),
        _ = bot.startup_failed.notified() => ("startupFailed", 1),
    };

    if exit_code == 0 {
        logger::info("Kori shutting down", ctx! {"signal" => signal});
        bot.audit.record(AuditEvent {
            title: "Kori Shutting Down".into(),
            category: "Bot".into(),
            color: "warning".into(),
            priority: "critical".into(),
            details: vec![("Signal".into(), signal.into())],
            ..Default::default()
        });
    }
    let flush = std::time::Duration::from_millis(2_000);
    bot.audit.shutdown(flush).await;
    shards.shutdown_all().await;
    // The SQLite connection closes with the process; WAL keeps it crash-safe.
    std::process::exit(exit_code);
}
