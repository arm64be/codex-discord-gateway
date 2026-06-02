use std::sync::Arc;

use codex_gateway_core::CodexGateway;
use serenity::all::{ChannelId, Command, Context, EventHandler, Interaction, Message, Ready};
use serenity::async_trait;
use tracing::{error, info};

use crate::commands::{commands, handle_codex_command};
use crate::output::{output, respond_ephemeral};
use crate::visibility::VisibilityConfig;

pub(crate) struct Handler {
    pub(crate) gateway: Arc<CodexGateway<ChannelId>>,
    pub(crate) visibility: Arc<VisibilityConfig>,
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
