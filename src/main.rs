use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Context as _, anyhow};
use chrono::{DateTime, Utc};
use codex_app_server::{AppClientInfo, AppServer, AppServerConfig, ThreadHandle};
use codex_app_server_protocol::{
    ClientRequest, GetAccountRateLimitsResponse, ModelListParams, ModelListResponse,
    ReasoningEffort, RequestId, ServerNotification, ThreadGoalClearParams, ThreadGoalClearResponse,
    ThreadGoalGetParams, ThreadGoalGetResponse, ThreadGoalSetParams, ThreadGoalSetResponse,
    ThreadGoalStatus, ThreadListParams, ThreadListResponse, ThreadResumeParams,
    ThreadResumeResponse, ThreadStartParams, TurnInterruptParams, TurnInterruptResponse,
    TurnStartParams, TurnStartResponse, TurnSteerParams, TurnSteerResponse, UserInput,
};
use serde::Deserialize;
use serenity::all::{
    ChannelId, Command, CommandInteraction, CommandOptionType, Context, CreateCommand,
    CreateCommandOption, CreateInteractionResponse, CreateInteractionResponseMessage,
    CreateMessage, EventHandler, GatewayIntents, Interaction, Message, Ready,
};
use serenity::async_trait;
use tokio::sync::{Mutex, mpsc};
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

    let gateway =
        CodexGateway::spawn(codex_bin, default_model, default_cwd, inherit_stderr).await?;
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
    gateway: Arc<CodexGateway>,
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

        if let Err(err) = handle_codex_command(&ctx, &command, Arc::clone(&self.gateway)).await {
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
            .enqueue_turn(ctx, msg.channel_id, prompt, true)
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
    gateway: Arc<CodexGateway>,
) -> anyhow::Result<()> {
    let Some(sub) = command.data.options.first() else {
        return respond_ephemeral(ctx, command, "Missing subcommand").await;
    };
    let channel_id = command.channel_id;

    match sub.name.as_str() {
        "ask" => {
            let prompt = sub_string(command, "prompt")?;
            let status = gateway
                .enqueue_turn(ctx.clone(), channel_id, prompt, false)
                .await?;
            respond_ephemeral(ctx, command, status).await?;
        }
        "steer" => {
            let message = sub_string(command, "message")?;
            let status = gateway
                .enqueue_turn(ctx.clone(), channel_id, message, true)
                .await?;
            respond_ephemeral(ctx, command, status).await?;
        }
        "queue" => {
            respond_ephemeral(ctx, command, gateway.queue_status(channel_id).await).await?;
        }
        "model" => {
            let content = if let Some(model) = sub_string_opt(command, "name") {
                gateway.set_model(channel_id, model).await
            } else {
                gateway.model_status(channel_id).await
            };
            respond_ephemeral(ctx, command, content).await?;
        }
        "effort" => {
            let content = if let Some(level) = sub_string_opt(command, "level") {
                gateway.set_effort(channel_id, &level).await?
            } else {
                gateway.effort_status(channel_id).await
            };
            respond_ephemeral(ctx, command, content).await?;
        }
        "models" => {
            let models = gateway.list_models().await?;
            respond_ephemeral(ctx, command, models).await?;
        }
        "status" => {
            let status = gateway.status(channel_id).await?;
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
                    .set_goal_status(channel_id, ThreadGoalStatus::Paused)
                    .await?,
            )
            .await?;
        }
        "resume" => {
            respond_ephemeral(
                ctx,
                command,
                gateway
                    .set_goal_status(channel_id, ThreadGoalStatus::Active)
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
            let content = gateway.session(channel_id, &action, thread_id).await?;
            respond_ephemeral(ctx, command, content).await?;
        }
        "interrupt" => {
            respond_ephemeral(ctx, command, gateway.interrupt(channel_id).await?).await?;
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

struct CodexGateway {
    server: Arc<AppServer>,
    sessions: Arc<Mutex<HashMap<ChannelId, SessionState>>>,
    default_model: String,
    default_cwd: Option<String>,
    next_request_id: AtomicI64,
    tx: mpsc::Sender<WorkItem>,
}

#[derive(Clone)]
struct SessionState {
    thread_id: Option<String>,
    model: String,
    effort: Option<ReasoningEffort>,
    active_turn_id: Option<String>,
    queued: VecDeque<QueuedTurn>,
}

#[derive(Clone)]
struct QueuedTurn {
    prompt: String,
}

struct WorkItem {
    ctx: Context,
    channel_id: ChannelId,
}

impl CodexGateway {
    async fn spawn(
        codex_bin: String,
        default_model: String,
        default_cwd: Option<String>,
        inherit_stderr: bool,
    ) -> anyhow::Result<Self> {
        let mut server = AppServer::spawn(AppServerConfig {
            codex_bin,
            server_args: vec!["app-server".into()],
            inherit_stderr,
        })
        .await?;
        server
            .initialize(AppClientInfo {
                name: "codex_discord_gateway".into(),
                title: Some("Codex Discord Gateway".into()),
                version: env!("CARGO_PKG_VERSION").into(),
            })
            .await?;

        let (tx, rx) = mpsc::channel(128);
        let gateway = Self {
            server: Arc::new(server),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            default_model,
            default_cwd,
            next_request_id: AtomicI64::new(10_000),
            tx,
        };
        gateway.start_worker(rx);
        Ok(gateway)
    }

    fn start_worker(&self, mut rx: mpsc::Receiver<WorkItem>) {
        let server = Arc::clone(&self.server);
        let sessions = self.sessions.clone();
        let default_model = self.default_model.clone();
        let default_cwd = self.default_cwd.clone();

        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                loop {
                    let next = {
                        let mut guard = sessions.lock().await;
                        let session = guard
                            .entry(item.channel_id)
                            .or_insert_with(|| SessionState::new(default_model.clone()));
                        if session.active_turn_id.is_some() {
                            None
                        } else {
                            session.queued.pop_front()
                        }
                    };

                    let Some(turn) = next else {
                        break;
                    };

                    if let Err(err) = run_turn(
                        &server,
                        &sessions,
                        &default_model,
                        default_cwd.as_deref(),
                        item.ctx.clone(),
                        item.channel_id,
                        turn,
                    )
                    .await
                    {
                        error!(?err, "turn failed");
                        let _ = item
                            .channel_id
                            .say(&item.ctx.http, format!("Codex turn failed: {err:#}"))
                            .await;
                    }
                }
            }
        });
    }

    async fn enqueue_turn(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        prompt: String,
        steer: bool,
    ) -> anyhow::Result<String> {
        if steer {
            let snapshot = self.session_snapshot(channel_id).await;
            if let (Some(thread_id), Some(turn_id)) =
                (snapshot.thread_id.clone(), snapshot.active_turn_id.clone())
            {
                let _: TurnSteerResponse = self
                    .call(|id| ClientRequest::TurnSteer {
                        id,
                        params: TurnSteerParams {
                            client_user_message_id: None,
                            expected_turn_id: turn_id,
                            input: vec![UserInput::Text {
                                text: prompt,
                                text_elements: vec![],
                            }],
                            thread_id,
                        },
                    })
                    .await?;
                return Ok("Done.".into());
            }
        }

        {
            let mut guard = self.sessions.lock().await;
            let session = guard
                .entry(channel_id)
                .or_insert_with(|| SessionState::new(self.default_model.clone()));
            session.queued.push_back(QueuedTurn { prompt });
        }

        self.tx.send(WorkItem { ctx, channel_id }).await?;
        Ok("Done.".into())
    }

    async fn queue_status(&self, channel_id: ChannelId) -> String {
        let guard = self.sessions.lock().await;
        let Some(session) = guard.get(&channel_id) else {
            return "No session for this channel yet.".into();
        };
        let active = session
            .active_turn_id
            .as_deref()
            .unwrap_or("no active turn");
        format!("Active: {active}\nQueued turns: {}", session.queued.len())
    }

    async fn model_status(&self, channel_id: ChannelId) -> String {
        let session = self.session_snapshot(channel_id).await;
        format!("Current model: `{}`", session.model)
    }

    async fn set_model(&self, channel_id: ChannelId, model: String) -> String {
        let mut guard = self.sessions.lock().await;
        let session = guard
            .entry(channel_id)
            .or_insert_with(|| SessionState::new(self.default_model.clone()));
        session.model = model.clone();
        format!("Model set to `{model}`. It will apply to the next turn.")
    }

    async fn effort_status(&self, channel_id: ChannelId) -> String {
        let session = self.session_snapshot(channel_id).await;
        match session.effort {
            Some(effort) => format!("Current reasoning effort: `{effort}`"),
            None => "Current reasoning effort: Codex default".into(),
        }
    }

    async fn set_effort(&self, channel_id: ChannelId, level: &str) -> anyhow::Result<String> {
        let effort = parse_effort(level)?;
        let mut guard = self.sessions.lock().await;
        let session = guard
            .entry(channel_id)
            .or_insert_with(|| SessionState::new(self.default_model.clone()));
        session.effort = effort;
        Ok(match effort {
            Some(value) => format!("Reasoning effort set to `{value}`."),
            None => "Reasoning effort cleared; Codex default will be used.".into(),
        })
    }

    async fn list_models(&self) -> anyhow::Result<String> {
        let response: ModelListResponse = self
            .call(|id| ClientRequest::ModelList {
                id,
                params: ModelListParams {
                    include_hidden: Some(false),
                    limit: Some(25),
                    ..Default::default()
                },
            })
            .await?;
        let mut out = String::from("Available models:\n");
        for model in response.data.iter().take(25) {
            let default = if model.is_default { " default" } else { "" };
            out.push_str(&format!(
                "- `{}` ({}){}\n",
                model.model, model.display_name, default
            ));
        }
        if response.next_cursor.is_some() {
            out.push_str("\nMore models exist; increase this command if needed.");
        }
        Ok(out)
    }

    async fn status(&self, channel_id: ChannelId) -> anyhow::Result<String> {
        let session = self.session_snapshot(channel_id).await;
        let limits: GetAccountRateLimitsResponse = self
            .call(|id| ClientRequest::AccountRateLimitsRead { id, params: () })
            .await?;

        let mut out = String::new();
        out.push_str(&format!(
            "Thread: `{}`\nModel: `{}`\nEffort: `{}`\n",
            session.thread_id.as_deref().unwrap_or("none"),
            session.model,
            session
                .effort
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Codex default".into())
        ));
        out.push_str(&format_rate_limits(&limits));
        Ok(out)
    }

    async fn goal(
        &self,
        channel_id: ChannelId,
        objective: Option<String>,
        budget: Option<i64>,
    ) -> anyhow::Result<String> {
        let thread_id = self.ensure_thread(channel_id).await?;
        if let Some(objective) = objective {
            let response: ThreadGoalSetResponse = self
                .call(|id| ClientRequest::ThreadGoalSet {
                    id,
                    params: ThreadGoalSetParams {
                        thread_id: thread_id.clone(),
                        objective: Some(objective),
                        token_budget: budget,
                        status: Some(ThreadGoalStatus::Active),
                    },
                })
                .await?;
            return Ok(format_goal(Some(&response.goal)));
        }

        let response: ThreadGoalGetResponse = self
            .call(|id| ClientRequest::ThreadGoalGet {
                id,
                params: ThreadGoalGetParams {
                    thread_id: thread_id.clone(),
                },
            })
            .await?;
        Ok(format_goal(response.goal.as_ref()))
    }

    async fn set_goal_status(
        &self,
        channel_id: ChannelId,
        status: ThreadGoalStatus,
    ) -> anyhow::Result<String> {
        let thread_id = self.ensure_thread(channel_id).await?;
        let response: ThreadGoalSetResponse = self
            .call(|id| ClientRequest::ThreadGoalSet {
                id,
                params: ThreadGoalSetParams {
                    thread_id: thread_id.clone(),
                    objective: None,
                    token_budget: None,
                    status: Some(status),
                },
            })
            .await?;
        Ok(format_goal(Some(&response.goal)))
    }

    async fn clear_goal(&self, channel_id: ChannelId) -> anyhow::Result<String> {
        let thread_id = self.ensure_thread(channel_id).await?;
        let response: ThreadGoalClearResponse = self
            .call(|id| ClientRequest::ThreadGoalClear {
                id,
                params: ThreadGoalClearParams {
                    thread_id: thread_id.clone(),
                },
            })
            .await?;
        Ok(if response.cleared {
            "Goal cleared.".into()
        } else {
            "No goal was set.".into()
        })
    }

    async fn session(
        &self,
        channel_id: ChannelId,
        action: &str,
        thread_id: Option<String>,
    ) -> anyhow::Result<String> {
        match action {
            "show" => {
                let session = self.session_snapshot(channel_id).await;
                Ok(format!(
                    "Thread: `{}`\nModel: `{}`\nEffort: `{}`",
                    session.thread_id.as_deref().unwrap_or("none"),
                    session.model,
                    session
                        .effort
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "Codex default".into())
                ))
            }
            "new" => {
                let thread = self.start_thread(channel_id).await?;
                Ok(format!("Started new thread `{}`.", thread.thread_id))
            }
            "list" => {
                let response: ThreadListResponse = self
                    .call(|id| ClientRequest::ThreadList {
                        id,
                        params: ThreadListParams {
                            limit: Some(10),
                            ..Default::default()
                        },
                    })
                    .await?;
                let mut out = String::from("Recent threads:\n");
                for thread in response.data {
                    out.push_str(&format!(
                        "- `{}` {} ({})\n",
                        thread.id,
                        thread.name.unwrap_or(thread.preview),
                        fmt_ts(thread.updated_at)
                    ));
                }
                Ok(out)
            }
            "switch" => {
                let thread_id = thread_id.ok_or_else(|| anyhow!("thread_id is required"))?;
                let snapshot = self.session_snapshot(channel_id).await;
                let response: ThreadResumeResponse = self
                    .call(|id| ClientRequest::ThreadResume {
                        id,
                        params: ThreadResumeParams {
                            thread_id: thread_id.clone(),
                            model: Some(snapshot.model.clone()),
                            cwd: self.default_cwd.clone(),
                            ..empty_thread_resume_params()
                        },
                    })
                    .await?;
                let mut guard = self.sessions.lock().await;
                let session = guard
                    .entry(channel_id)
                    .or_insert_with(|| SessionState::new(self.default_model.clone()));
                session.thread_id = Some(response.thread.id.clone());
                Ok(format!("Switched to thread `{}`.", response.thread.id))
            }
            other => Err(anyhow!("unknown session action `{other}`")),
        }
    }

    async fn interrupt(&self, channel_id: ChannelId) -> anyhow::Result<String> {
        let session = self.session_snapshot(channel_id).await;
        let thread_id = session
            .thread_id
            .ok_or_else(|| anyhow!("no active thread for this channel"))?;
        let turn_id = session
            .active_turn_id
            .ok_or_else(|| anyhow!("no active turn for this channel"))?;
        let _: TurnInterruptResponse = self
            .call(|id| ClientRequest::TurnInterrupt {
                id,
                params: TurnInterruptParams { thread_id, turn_id },
            })
            .await?;
        Ok("Interrupt requested.".into())
    }

    async fn ensure_thread(&self, channel_id: ChannelId) -> anyhow::Result<String> {
        if let Some(thread_id) = self.session_snapshot(channel_id).await.thread_id {
            return Ok(thread_id);
        }
        Ok(self.start_thread(channel_id).await?.thread_id)
    }

    async fn start_thread(&self, channel_id: ChannelId) -> anyhow::Result<ThreadHandle> {
        let snapshot = self.session_snapshot(channel_id).await;
        let thread = {
            self.server
                .thread_start(ThreadStartParams {
                    model: Some(snapshot.model),
                    cwd: self.default_cwd.clone(),
                    service_name: Some("discord".into()),
                    ..Default::default()
                })
                .await?
        };
        let mut guard = self.sessions.lock().await;
        let session = guard
            .entry(channel_id)
            .or_insert_with(|| SessionState::new(self.default_model.clone()));
        session.thread_id = Some(thread.thread_id.clone());
        Ok(thread)
    }

    async fn call<R, F>(&self, build: F) -> anyhow::Result<R>
    where
        R: for<'de> serde::Deserialize<'de>,
        F: FnOnce(RequestId) -> ClientRequest,
    {
        let id = RequestId::Int64(self.next_request_id.fetch_add(1, Ordering::Relaxed));
        self.server.call(build(id)).await.map_err(Into::into)
    }

    async fn session_snapshot(&self, channel_id: ChannelId) -> SessionState {
        let mut guard = self.sessions.lock().await;
        guard
            .entry(channel_id)
            .or_insert_with(|| SessionState::new(self.default_model.clone()))
            .clone()
    }
}

async fn run_turn(
    server: &Arc<AppServer>,
    sessions: &Mutex<HashMap<ChannelId, SessionState>>,
    default_model: &str,
    default_cwd: Option<&str>,
    ctx: Context,
    channel_id: ChannelId,
    queued: QueuedTurn,
) -> anyhow::Result<()> {
    let snapshot = {
        let mut guard = sessions.lock().await;
        guard
            .entry(channel_id)
            .or_insert_with(|| SessionState::new(default_model.to_string()))
            .clone()
    };

    let thread_id = match snapshot.thread_id {
        Some(thread_id) => thread_id,
        None => {
            let thread = {
                server
                    .thread_start(ThreadStartParams {
                        model: Some(snapshot.model.clone()),
                        cwd: default_cwd.map(str::to_string),
                        service_name: Some("discord".into()),
                        ..Default::default()
                    })
                    .await?
            };
            let mut guard = sessions.lock().await;
            let session = guard
                .entry(channel_id)
                .or_insert_with(|| SessionState::new(default_model.to_string()));
            session.thread_id = Some(thread.thread_id.clone());
            thread.thread_id
        }
    };

    let start: TurnStartResponse = {
        server
            .call(ClientRequest::TurnStart {
                id: RequestId::String(format!("turn-start-{channel_id}-{thread_id}")),
                params: TurnStartParams {
                    thread_id: thread_id.clone(),
                    input: vec![UserInput::Text {
                        text: queued.prompt,
                        text_elements: vec![],
                    }],
                    model: Some(snapshot.model),
                    effort: snapshot.effort,
                    cwd: default_cwd.map(str::to_string),
                    ..empty_turn_start_params()
                },
            })
            .await?
    };
    let turn_id = start.turn.id.clone();

    {
        let mut guard = sessions.lock().await;
        if let Some(session) = guard.get_mut(&channel_id) {
            session.active_turn_id = Some(turn_id.clone());
        }
    }

    let mut sent_agent_items = HashSet::new();

    loop {
        let incoming = { server.recv().await? };

        let codex_app_server::IncomingMessage::Notification(notification) = incoming else {
            continue;
        };

        match *notification {
            ServerNotification::ItemCompleted(item)
                if item.thread_id == thread_id && item.turn_id == turn_id =>
            {
                if let Some((id, text)) = agent_item_text(&item.item)
                    && !text.trim().is_empty()
                    && sent_agent_items.insert(id.to_string())
                {
                    send_discord_text(&ctx, channel_id, text).await?;
                }
            }
            ServerNotification::TurnCompleted(note)
                if note.thread_id == thread_id && note.turn.id == turn_id =>
            {
                for item in &note.turn.items {
                    if let Some((id, text)) = agent_item_text(item)
                        && !text.trim().is_empty()
                        && sent_agent_items.insert(id.to_string())
                    {
                        send_discord_text(&ctx, channel_id, text).await?;
                    }
                }
                {
                    let mut guard = sessions.lock().await;
                    if let Some(session) = guard.get_mut(&channel_id) {
                        session.active_turn_id = None;
                    }
                }
                break;
            }
            ServerNotification::Error(err)
                if err.thread_id == thread_id && err.turn_id == turn_id =>
            {
                let mut guard = sessions.lock().await;
                if let Some(session) = guard.get_mut(&channel_id) {
                    session.active_turn_id = None;
                }
                return Err(anyhow!("{:?}", err.error));
            }
            _ => {}
        }
    }

    Ok(())
}

async fn send_discord_text(ctx: &Context, channel_id: ChannelId, text: &str) -> anyhow::Result<()> {
    for chunk in discord_chunks(text, DISCORD_LIMIT) {
        channel_id
            .send_message(&ctx.http, CreateMessage::new().content(chunk))
            .await?;
    }
    Ok(())
}

impl SessionState {
    fn new(model: String) -> Self {
        Self {
            thread_id: None,
            model,
            effort: None,
            active_turn_id: None,
            queued: VecDeque::new(),
        }
    }
}

fn empty_turn_start_params() -> TurnStartParams {
    TurnStartParams {
        approval_policy: None,
        approvals_reviewer: None,
        client_user_message_id: None,
        cwd: None,
        effort: None,
        input: vec![],
        model: None,
        output_schema: None,
        personality: None,
        sandbox_policy: None,
        service_tier: None,
        summary: None,
        thread_id: String::new(),
    }
}

fn empty_thread_resume_params() -> ThreadResumeParams {
    ThreadResumeParams {
        approval_policy: None,
        approvals_reviewer: None,
        base_instructions: None,
        config: None,
        cwd: None,
        developer_instructions: None,
        model: None,
        model_provider: None,
        personality: None,
        sandbox: None,
        service_tier: None,
        thread_id: String::new(),
    }
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

fn format_goal(goal: Option<&codex_app_server_protocol::ThreadGoal>) -> String {
    let Some(goal) = goal else {
        return "No goal set.".into();
    };
    format!(
        "Goal: {}\nStatus: `{}`\nTokens: {}{}\nTime used: {}s",
        goal.objective,
        goal.status,
        goal.tokens_used,
        goal.token_budget
            .map(|budget| format!(" / {budget}"))
            .unwrap_or_default(),
        goal.time_used_seconds
    )
}

fn format_rate_limits(response: &GetAccountRateLimitsResponse) -> String {
    let mut out = String::from("Rate limits\n");
    let snapshots: Vec<_> = response
        .rate_limits_by_limit_id
        .as_ref()
        .map(|map| map.values().collect())
        .unwrap_or_else(|| vec![&response.rate_limits]);

    for snapshot in snapshots {
        let name = snapshot
            .limit_name
            .as_deref()
            .or(snapshot.limit_id.as_deref())
            .unwrap_or("default");
        out.push_str(&format!("- **{name}**"));
        if let Some(primary) = &snapshot.primary {
            out.push_str(&format!(
                ": primary {} remaining, resets {}",
                remaining_percent(primary.used_percent),
                discord_relative_ts(primary.resets_at)
            ));
        }
        if let Some(secondary) = &snapshot.secondary {
            out.push_str(&format!(
                "; secondary {} remaining, resets {}",
                remaining_percent(secondary.used_percent),
                discord_relative_ts(secondary.resets_at)
            ));
        }
        if snapshot.primary.is_none() && snapshot.secondary.is_none() {
            out.push_str(" no window data");
        }
        out.push('\n');
    }
    out
}

fn remaining_percent(used_percent: i32) -> String {
    format!("{}%", (100 - used_percent).clamp(0, 100))
}

fn discord_relative_ts(ts: Option<i64>) -> String {
    ts.map(|ts| format!("<t:{ts}:R>"))
        .unwrap_or_else(|| "unknown".into())
}

fn agent_item_text(item: &codex_app_server_protocol::ThreadItem) -> Option<(&str, &str)> {
    match item {
        codex_app_server_protocol::ThreadItem::AgentMessage { id, text, .. } => {
            Some((id.as_str(), text.as_str()))
        }
        _ => None,
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

fn fmt_ts(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "unknown".into())
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
