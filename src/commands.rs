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
