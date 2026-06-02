use serde::Deserialize;
use serenity::all::Message;

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct VisibilityConfig {
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

impl VisibilityConfig {
    pub(crate) fn visible_prompt(&self, msg: &Message, bot_user_id: u64) -> Option<String> {
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

fn default_channel_mode() -> ChannelMode {
    ChannelMode::Mention
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
