mod commands;
mod config;
mod handler;
mod output;
mod visibility;

use std::env;
use std::sync::Arc;

use anyhow::Context as _;
use codex_gateway_core::{CodexGateway, GatewayConfig};
use serenity::all::GatewayIntents;

use crate::config::AppConfig;
use crate::handler::Handler;

const DEFAULT_MODEL: &str = "gpt-5.4-mini";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = AppConfig::load()?;
    let token = env::var("DISCORD_TOKEN")
        .ok()
        .or(config.discord_token.clone())
        .context("discord token is required in DISCORD_TOKEN or config.discord-gateway.toml")?;

    let gateway = CodexGateway::spawn(gateway_config(&config)).await?;
    let handler = Handler {
        gateway: Arc::new(gateway),
        visibility: Arc::new(config.visibility),
    };

    let intents = GatewayIntents::non_privileged()
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;
    let mut client = serenity::Client::builder(token, intents)
        .event_handler(handler)
        .await?;

    client.start().await?;
    Ok(())
}

fn gateway_config(config: &AppConfig) -> GatewayConfig {
    let codex_bin = env::var("CODEX_BIN")
        .ok()
        .or(config.codex_bin.clone())
        .unwrap_or_else(|| "codex".to_string());
    let default_model = env::var("CODEX_DEFAULT_MODEL")
        .ok()
        .or(config.default_model.clone())
        .unwrap_or_else(|| DEFAULT_MODEL.into());
    let default_cwd = env::var("CODEX_CWD").ok().or(config.cwd.clone());
    let inherit_stderr = env::var("CODEX_INHERIT_STDERR").is_ok() || config.inherit_stderr;

    let mut gateway_config = GatewayConfig::new(default_model);
    gateway_config.codex_bin = codex_bin;
    gateway_config.default_cwd = default_cwd;
    gateway_config.inherit_stderr = inherit_stderr;
    gateway_config.client_name = "codex_discord_gateway".into();
    gateway_config.client_title = Some("Codex Discord Gateway".into());
    gateway_config.client_version = env!("CARGO_PKG_VERSION").into();
    gateway_config.service_name = Some("discord".into());
    gateway_config
}
