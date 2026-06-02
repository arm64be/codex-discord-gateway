use std::env;

use anyhow::Context as _;
use serde::Deserialize;

use crate::visibility::VisibilityConfig;

const CONFIG_FILE: &str = "config.discord-gateway.toml";

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct AppConfig {
    pub(crate) discord_token: Option<String>,
    pub(crate) codex_bin: Option<String>,
    pub(crate) default_model: Option<String>,
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) inherit_stderr: bool,
    #[serde(default)]
    pub(crate) visibility: VisibilityConfig,
}

impl AppConfig {
    pub(crate) fn load() -> anyhow::Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
    }
}

fn config_path() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(codex_home) = env::var("CODEX_HOME") {
        return Ok(std::path::PathBuf::from(codex_home).join(CONFIG_FILE));
    }

    let home = env::var("HOME").context("CODEX_HOME or HOME is required to locate config")?;
    Ok(std::path::PathBuf::from(home)
        .join(".codex")
        .join(CONFIG_FILE))
}
