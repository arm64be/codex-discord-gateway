# Codex Discord Gateway

Rust Discord bot for Codex. Gateway state, turn queues, sessions, goals, and
Codex app-server interaction are handled by
[`arm64be/codex-gateway-core`](https://github.com/arm64be/codex-gateway-core).

## Requirements

- Rust toolchain
- `codex` CLI installed and authenticated
- A Discord application bot token with slash commands enabled

## Install

```bash
cargo install --git https://github.com/arm64be/codex-discord-gateway.git
```

Then run:

```bash
codex-discord-gateway
```

## Run

Create `$CODEX_HOME/config.discord-gateway.toml`, or `~/.codex/config.discord-gateway.toml`
when `CODEX_HOME` is unset:

```toml
discord_token = "your-discord-bot-token"
default_model = "gpt-5.4-mini"

# Optional:
# codex_bin = "codex"
# cwd = "/path/to/workspace"
# inherit_stderr = false

[visibility]
# Alice and David may DM the bot; DMs are read without requiring a mention.
dm_allow_users = [111111111111111111, 444444444444444444]

# In this channel, Alice and Bob are visible only when they mention the bot.
[[visibility.channels]]
id = 555555555555555555
mode = "mention"
users = [111111111111111111, 222222222222222222]

# In this channel, Bob and David are visible even without mentioning the bot.
[[visibility.channels]]
id = 666666666666666666
mode = "always"
users = [222222222222222222, 444444444444444444]
```

```bash
cargo run
```

Environment variables still work and override the config for `DISCORD_TOKEN`,
`CODEX_DEFAULT_MODEL`, `CODEX_BIN`, `CODEX_CWD`, and `CODEX_INHERIT_STDERR`.

The bot registers one global slash command, `/codex`, on startup.

## Commands

- `/codex ask prompt:<text>` queues a Codex turn for the current Discord channel.
- `/codex steer message:<text>` steers the active turn with `turn/steer`; if idle, it queues a turn.
- `/codex queue` shows active and queued turns.
- `/codex model [name]` shows or changes the model for future turns.
- `/codex effort [level]` shows or changes reasoning effort. Use `default`, `minimal`, `low`, `medium`, or `high`.
- `/codex models` lists available Codex models.
- `/codex status` shows thread, model, effort, and rate-limit reset data.
- `/codex goal [objective] [token_budget]` shows or sets the thread goal.
- `/codex pause` pauses the current goal.
- `/codex resume` resumes the current goal.
- `/codex goal-clear` clears the current goal.
- `/codex session action:<show|new|list|switch> [thread_id]` manages Codex threads.
- `/codex interrupt` interrupts the active turn.

## Behavior

Sessions are scoped per Discord channel. Each channel keeps its own Codex thread,
model, effort, active turn, and queue. Codex outputs are sent as completed
assistant messages; turns with no assistant output stay silent.

Automatic message visibility is opt-in. The bot only reads normal message
content from users and channels listed in `[visibility]`. In `mention` mode the
message must mention the bot; in `always` mode any message from an allowed user
in that channel is sent to Codex. Allowed DMs never require a mention.

## License

Licensed under either of Apache License, Version 2.0 or MIT license, at your option.
