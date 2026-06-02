use std::sync::Arc;

use anyhow::anyhow;
use codex_gateway_core::{CodexGateway, GoalStatus, ReasoningEffort, SessionAction};
use serenity::all::{
    ChannelId, CommandInteraction, CommandOptionType, Context, CreateCommand, CreateCommandOption,
};

use crate::output::{DiscordOutput, respond_ephemeral};

pub(crate) fn commands() -> Vec<CreateCommand> {
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
                    "minimal, low, medium, high, xhigh",
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
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "resume",
                    "Resume a session by thread id, or resume the current goal",
                )
                .add_sub_option(CreateCommandOption::new(
                    CommandOptionType::String,
                    "thread_id",
                    "Thread id to resume",
                )),
            )
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "goal-clear",
                "Clear the current goal",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "session",
                "Show current session",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "new",
                "Start a new Codex session",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "clear",
                "Clear the current Codex session",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "sessions",
                "List Codex sessions",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "interrupt",
                "Interrupt the active turn",
            )),
    ]
}

pub(crate) async fn handle_codex_command(
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
        "models" => respond_ephemeral(ctx, command, gateway.list_models().await?).await?,
        "status" => respond_ephemeral(ctx, command, gateway.status(&channel_id).await?).await?,
        "goal" => {
            let objective = sub_string_opt(command, "objective");
            let budget = sub_i64_opt(command, "token_budget");
            respond_ephemeral(
                ctx,
                command,
                gateway.goal(channel_id, objective, budget).await?,
            )
            .await?;
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
            let content = if let Some(thread_id) = sub_string_opt(command, "thread_id") {
                gateway
                    .session(channel_id, SessionAction::Switch, Some(thread_id))
                    .await?
            } else {
                gateway
                    .set_goal_status(channel_id, GoalStatus::Active)
                    .await?
            };
            respond_ephemeral(ctx, command, content).await?;
        }
        "goal-clear" => {
            respond_ephemeral(ctx, command, gateway.clear_goal(channel_id).await?).await?;
        }
        "session" => {
            let content = gateway
                .session(channel_id, SessionAction::Show, None)
                .await?;
            respond_ephemeral(ctx, command, content).await?;
        }
        "new" | "clear" => {
            let content = gateway
                .session(channel_id, SessionAction::New, None)
                .await?;
            respond_ephemeral(ctx, command, content).await?;
        }
        "sessions" => {
            let content = gateway
                .session(channel_id, SessionAction::List, None)
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

fn parse_effort(value: &str) -> anyhow::Result<Option<ReasoningEffort>> {
    match value.to_ascii_lowercase().as_str() {
        "default" | "clear" | "none" => Ok(None),
        "minimal" => Ok(Some(ReasoningEffort::Minimal)),
        "low" => Ok(Some(ReasoningEffort::Low)),
        "medium" => Ok(Some(ReasoningEffort::Medium)),
        "high" => Ok(Some(ReasoningEffort::High)),
        "xhigh" => Ok(Some(ReasoningEffort::XHigh)),
        other => Err(anyhow!(
            "unknown effort `{other}`; use default, minimal, low, medium, high, or xhigh"
        )),
    }
}
