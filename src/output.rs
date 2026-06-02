use std::sync::Arc;

use codex_gateway_core::{BoxFuture, TurnOutput};
use serenity::all::{
    ChannelId, CommandInteraction, Context, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage,
};
use serenity::http::Http;

const DISCORD_LIMIT: usize = 1900;

#[derive(Debug)]
pub(crate) struct DiscordOutput {
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

pub(crate) fn output(ctx: &Context) -> Arc<DiscordOutput> {
    Arc::new(DiscordOutput {
        http: Arc::clone(&ctx.http),
    })
}

pub(crate) async fn respond_ephemeral(
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
