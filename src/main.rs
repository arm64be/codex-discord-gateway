use std::env;
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use codex_gateway_core::{
    BoxFuture, CodexGateway, GatewayConfig, GoalStatus, ReasoningEffort, SessionAction, TurnOutput,
};
use serde::Deserialize;
use serenity::all::{
    ChannelId, Command, CommandInteraction, CommandOptionType, Context, CreateCommand,
    CreateCommandOption, CreateInteractionResponse, CreateInteractionResponseMessage,
    CreateMessage, EventHandler, GatewayIntents, Interaction, Message, Ready,
};
use serenity::async_trait;
use serenity::http::Http;
use tracing::{error, info};

const DISCORD_LIMIT: usize = 1900;
const DEFAULT_MODEL: &str = "gpt-5.4-mini";
const CONFIG_FILE: &str = "config.discord-gateway.toml";

#[derive(Debug, Clone, Default, Deserialize)]
struct AppConfig {
    discord_token: Option<String>,
    codex_bin: Option<String>,
    default_model: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    inherit_stderr: bool,
    #[serde(default)]
    visibility: VisibilityConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct VisibilityConfig {
    #[serde(default)]
    dm_allow_users: Vec<u64>,
    #[serde(default)]
    channels: Vec<ChannelVisibilityRule>,
}

#[derive(Debug, Clone, Deserialize)]
struct ChannelVisibilityRule {
    id: u64,
    #[serde(default = "default_channel_mode")]
    mode: ChannelMode,
    #[serde(default)]
    users: Vec<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ChannelMode {
    Mention,
    Always,
}

impl Default for ChannelMode {
    fn default() -> Self {
        Self::Mention
    }
}

fn default_channel_mode() -> ChannelMode {
    ChannelMode::Mention
}

impl AppConfig {
    fn load() -> anyhow::Result<Self> {
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

    let gateway = CodexGateway::spawn(gateway_config).await?;
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

struct Handler {
    gateway: Arc<CodexGateway<ChannelId>>,
    visibility: Arc<VisibilityConfig>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "discord gateway connected");
        if let Err(err) = Command::set_global_commands(&ctx.http, commands()).await {
            error!(?err, "failed to register slash commands");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Command(command) = interaction else {
            return;
        };

        if command.data.name != "codex" {
            return;
        }

        if let Err(err) =
            handle_codex_command(&ctx, &command, Arc::clone(&self.gateway), output(&ctx)).await
        {
            error!(?err, "command failed");
            let _ = respond_ephemeral(&ctx, &command, format!("Codex error: {err:#}")).await;
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let bot_user = ctx.cache.current_user().id;
        let Some(prompt) = self.visibility.visible_prompt(&msg, bot_user.get()) else {
            return;
        };

        if prompt.trim().is_empty() {
            return;
        }

        if let Err(err) = self
            .gateway
            .enqueue_turn(msg.channel_id, prompt, true, output(&ctx))
            .await
        {
            error!(?err, "automatic message handling failed");
        }
    }
}

impl VisibilityConfig {
    fn visible_prompt(&self, msg: &Message, bot_user_id: u64) -> Option<String> {
        let user_id = msg.author.id.get();
        if msg.guild_id.is_none() {
            return self
                .dm_allow_users
                .contains(&user_id)
                .then(|| msg.content.trim().to_string());
        }

        let rule = self
            .channels
            .iter()
            .find(|rule| rule.id == msg.channel_id.get())?;

        if !rule.users.contains(&user_id) {
            return None;
        }

        match rule.mode {
            ChannelMode::Always => Some(strip_bot_mention(&msg.content, bot_user_id)),
            ChannelMode::Mention => {
                mentions_bot(msg, bot_user_id).then(|| strip_bot_mention(&msg.content, bot_user_id))
            }
        }
    }
}

fn mentions_bot(msg: &Message, bot_user_id: u64) -> bool {
    msg.mentions.iter().any(|user| user.id.get() == bot_user_id)
        || msg.content.contains(&format!("<@{bot_user_id}>"))
        || msg.content.contains(&format!("<@!{bot_user_id}>"))
}

fn strip_bot_mention(content: &str, bot_user_id: u64) -> String {
    content
        .replace(&format!("<@{bot_user_id}>"), "")
        .replace(&format!("<@!{bot_user_id}>"), "")
        .trim()
        .to_string()
}

fn commands() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("codex")
            .description("Use Codex through Discord")
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "ask", "Queue a user turn")
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "prompt",
                            "Prompt text",
                        )
                        .required(true),
                    ),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "steer",
                    "Steer the active turn, or queue if idle",
                )
                .add_sub_option(
                    CreateCommandOption::new(CommandOptionType::String, "message", "Steering text")
                        .required(true),
                ),
            )
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "queue",
                "Show queued turns",
            ))
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "model",
                    "Show or switch model",
                )
                .add_sub_option(CreateCommandOption::new(
                    CommandOptionType::String,
                    "name",
                    "Model id, omit to show current",
                )),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "effort",
                    "Show or switch reasoning effort",
                )
                .add_sub_option(CreateCommandOption::new(
                    CommandOptionType::String,
                    "level",
                    "minimal, low, medium, high",
                )),
            )
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "models",
                "List available models",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "status",
                "Show account, rate limits, and current session",
            ))
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "goal",
                    "Set or inspect a goal",
                )
                .add_sub_option(CreateCommandOption::new(
                    CommandOptionType::String,
                    "objective",
                    "New objective, omit to show current goal",
                ))
                .add_sub_option(CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "token_budget",
                    "Optional token budget",
                )),
            )
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "pause",
                "Pause the current goal",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "resume",
                "Resume the current goal",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "goal-clear",
                "Clear the current goal",
            ))
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "session",
                    "Show, new, list, or switch sessions",
                )
                .add_sub_option(CreateCommandOption::new(
                    CommandOptionType::String,
                    "action",
                    "show, new, list, switch",
                ))
                .add_sub_option(CreateCommandOption::new(
                    CommandOptionType::String,
                    "thread_id",
                    "Thread id for switch",
                )),
            )
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "interrupt",
                "Interrupt the active turn",
            )),
    ]
}

async fn handle_codex_command(
    ctx: &Context,
    command: &CommandInteraction,
    gateway: Arc<CodexGateway<ChannelId>>,
    output: Arc<DiscordOutput>,
) -> anyhow::Result<()> {
    let Some(sub) = command.data.options.first() else {
        return respond_ephemeral(ctx, command, "Missing subcommand").await;
    };
    let channel_id = command.channel_id;

    match sub.name.as_str() {
        "ask" => {
            let prompt = sub_string(command, "prompt")?;
            let status = gateway
                .enqueue_turn(channel_id, prompt, false, output)
                .await?;
            respond_ephemeral(ctx, command, status).await?;
        }
        "steer" => {
            let message = sub_string(command, "message")?;
            let status = gateway
                .enqueue_turn(channel_id, message, true, output)
                .await?;
            respond_ephemeral(ctx, command, status).await?;
        }
        "queue" => {
            respond_ephemeral(ctx, command, gateway.queue_status(&channel_id).await).await?;
        }
        "model" => {
            let content = if let Some(model) = sub_string_opt(command, "name") {
                gateway.set_model(channel_id, model).await
            } else {
                gateway.model_status(&channel_id).await
            };
            respond_ephemeral(ctx, command, content).await?;
        }
        "effort" => {
            let content = if let Some(level) = sub_string_opt(command, "level") {
                gateway.set_effort(channel_id, parse_effort(&level)?).await
            } else {
                gateway.effort_status(&channel_id).await
            };
            respond_ephemeral(ctx, command, content).await?;
        }
        "models" => {
            let models = gateway.list_models().await?;
            respond_ephemeral(ctx, command, models).await?;
        }
        "status" => {
            let status = gateway.status(&channel_id).await?;
            respond_ephemeral(ctx, command, status).await?;
        }
        "goal" => {
            let objective = sub_string_opt(command, "objective");
            let budget = sub_i64_opt(command, "token_budget");
            let content = gateway.goal(channel_id, objective, budget).await?;
            respond_ephemeral(ctx, command, content).await?;
        }
        "pause" => {
            respond_ephemeral(
                ctx,
                command,
                gateway
                    .set_goal_status(channel_id, GoalStatus::Paused)
                    .await?,
            )
            .await?;
        }
        "resume" => {
            respond_ephemeral(
                ctx,
                command,
                gateway
                    .set_goal_status(channel_id, GoalStatus::Active)
                    .await?,
            )
            .await?;
        }
        "goal-clear" => {
            respond_ephemeral(ctx, command, gateway.clear_goal(channel_id).await?).await?;
        }
        "session" => {
            let action = sub_string_opt(command, "action").unwrap_or_else(|| "show".into());
            let thread_id = sub_string_opt(command, "thread_id");
            let content = gateway
                .session(channel_id, parse_session_action(&action)?, thread_id)
                .await?;
            respond_ephemeral(ctx, command, content).await?;
        }
        "interrupt" => {
            respond_ephemeral(ctx, command, gateway.interrupt(&channel_id).await?).await?;
        }
        _ => respond_ephemeral(ctx, command, "Unknown subcommand").await?,
    }

    Ok(())
}

async fn respond_ephemeral(
    ctx: &Context,
    command: &CommandInteraction,
    content: impl Into<String>,
) -> anyhow::Result<()> {
    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(truncate(content.into(), DISCORD_LIMIT))
                    .ephemeral(true),
            ),
        )
        .await?;
    Ok(())
}

fn sub_string(command: &CommandInteraction, name: &str) -> anyhow::Result<String> {
    sub_string_opt(command, name).ok_or_else(|| anyhow!("missing required option `{name}`"))
}

fn sub_string_opt(command: &CommandInteraction, name: &str) -> Option<String> {
    let sub = command.data.options.first()?;
    let serenity::all::CommandDataOptionValue::SubCommand(options) = &sub.value else {
        return None;
    };
    options
        .iter()
        .find(|option| option.name == name)
        .and_then(|option| option.value.as_str())
        .map(str::to_string)
}

fn sub_i64_opt(command: &CommandInteraction, name: &str) -> Option<i64> {
    let sub = command.data.options.first()?;
    let serenity::all::CommandDataOptionValue::SubCommand(options) = &sub.value else {
        return None;
    };
    options
        .iter()
        .find(|option| option.name == name)
        .and_then(|option| option.value.as_i64())
}

#[derive(Debug)]
struct DiscordOutput {
    http: Arc<Http>,
}

impl TurnOutput<ChannelId> for DiscordOutput {
    fn send<'a>(
        &'a self,
        channel_id: &'a ChannelId,
        text: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            for chunk in discord_chunks(text, DISCORD_LIMIT) {
                channel_id
                    .send_message(&self.http, CreateMessage::new().content(chunk))
                    .await?;
            }
            Ok(())
        })
    }
}

fn output(ctx: &Context) -> Arc<DiscordOutput> {
    Arc::new(DiscordOutput {
        http: Arc::clone(&ctx.http),
    })
}

fn parse_effort(value: &str) -> anyhow::Result<Option<ReasoningEffort>> {
    match value.to_ascii_lowercase().as_str() {
        "default" | "clear" | "none" => Ok(None),
        "minimal" => Ok(Some(ReasoningEffort::Minimal)),
        "low" => Ok(Some(ReasoningEffort::Low)),
        "medium" => Ok(Some(ReasoningEffort::Medium)),
        "high" => Ok(Some(ReasoningEffort::High)),
        other => Err(anyhow!(
            "unknown effort `{other}`; use default, minimal, low, medium, or high"
        )),
    }
}

fn parse_session_action(value: &str) -> anyhow::Result<SessionAction> {
    match value.to_ascii_lowercase().as_str() {
        "show" => Ok(SessionAction::Show),
        "new" => Ok(SessionAction::New),
        "list" => Ok(SessionAction::List),
        "switch" => Ok(SessionAction::Switch),
        other => Err(anyhow!("unknown session action `{other}`")),
    }
}

fn discord_chunks(text: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut remaining = text.trim();

    while remaining.len() > max {
        let boundary = char_boundary_at_or_before(remaining, max);
        let split = remaining[..boundary]
            .rfind('\n')
            .or_else(|| remaining[..boundary].rfind(' '))
            .unwrap_or(boundary);
        let (chunk, rest) = remaining.split_at(split.max(1));
        chunks.push(chunk.trim().to_string());
        remaining = rest.trim();
    }

    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }

    chunks
}

fn char_boundary_at_or_before(text: &str, max: usize) -> usize {
    let mut boundary = max.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary.max(1)
}

fn truncate(mut value: String, max: usize) -> String {
    if value.len() <= max {
        return value;
    }
    while !value.is_char_boundary(max) {
        value.pop();
    }
    value.truncate(max);
    value.push_str("\n...[truncated]");
    value
}
