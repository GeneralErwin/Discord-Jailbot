# Discord-Jailbot

A Discord moderation bot written in **Rust**, using Twilight and SQLite. Designed to provide persistent member jailing, role restoration, and configurable jail permissions.

> **Status: Beta**
>
> Everything listed below exists in the code. Features with an explicit setup switch, including quarantine, stay off until configured. Large servers should still be watched during the first jail/release cycle, because the bot repairs state in bounded, rate-limited passes rather than all at once.

## Features

### Commands

| Command | Permission | Purpose |
|---|---|---|
| `/ping` | — | Check whether the bot is responding. |
| `/jailsetup` | Administrator or owner | Create or find the `Prisoner` role. |
| `/setjail` | Administrator or owner | Choose the channel prisoners may use, and apply overwrites to every channel. |
| `/jail` | Configured Moderator role, or owner | Jail a lower-ranked member, with an optional duration (`30m`, `2h`, `3d`, `1w`) and reason. |
| `/unjail` | Configured Moderator role, or owner | Release a lower-ranked member and restore their previous state. |
| `/set moderator role` | Administrator or owner | Choose who may jail members below their highest role. |
| `/setlogs` | Administrator or owner | Set the moderation log channel. |
| `/prisoners` | Manage Roles | List the server's current prisoners. |
| `/jailinfo` | Manage Roles | Show a member's jail history. |
| `/jailconfig` | Administrator or owner | Show the server's jail configuration. |
| `/sanitize` | Administrator or owner | Scan channels and repair incorrect `Prisoner` role permissions. |
| `/quarantine` | Administrator or owner | Toggle quarantine and display module states. All modules start enabled. |
| `/enable module` and `/disable module` | Administrator or owner | Toggle one quarantine module. |
| `/help` | Everyone | Show the command list. |
| `/changelog` | Everyone | Show recent changes. |
| `/ping` | Everyone | Check whether the bot is responding. |

### Jail state

- One active sentence per member per server, enforced by a partial unique index.
- Original roles (with positions and restore flags) and nickname are saved before anything changes.
- Restrictions are applied through the **Prisoner role** alone, so jailing costs a handful of API calls instead of one per channel. Outside the jail channel the role denies viewing, sending, thread replies, thread creation and reactions; inside it the role may only read and send plain text. Legacy records that still hold member overwrites are restored on release.
- The database record is written **before** Discord is modified, and a release is only marked complete **after** roles, nickname and overwrites are restored. A crash in the middle leaves a record that the next repair pass finishes.

### Automatic systems

- **Sentence expiry** — a maintenance loop releases due sentences every 30 seconds. Prisoners who left the server are expired through their saved overwrites alone.
- **Rejoin detection** — a prisoner who rejoins is re-jailed by the next repair pass, and the event is logged.
- **Permission reconciliation** — new and updated channels, changed roles, and edited nicknames all queue a repair pass that re-applies the Prisoner role and member overwrites.
- **Recovery** — a startup sweep plus a five-minute sweep repair any state left behind by an API failure or a restart.
- **Deleted channels** — their saved overwrite snapshots are discarded instead of accumulating.

### Quarantine service

Run `/quarantine` as the server owner or an administrator to turn the service on. On first activation all seven modules start enabled; `/quarantine` also displays their states with a green check or red X. Use `/enable module` and `/disable module` with one of the module names in that embed to change individual protections. Run `/quarantine` again to pause the full service.

- Dangerous role grants and removals are reversed when the affected role has moderation permissions, then the audit-log actor is jailed.
- Dangerous role permission changes are reset to the old permission set. Dangerous channel overwrites are restored from the audit log.
- Changes to a jailed member's roles queue the existing jail repair and quarantine the actor.
- A channel deletion quarantines and logs the actor. Discord does not let the bot restore the deleted channel's original ID or messages.
- Three webhook creations within 30 seconds, or a webhook mass mention, trigger containment: delete all guild webhooks and remove `Manage Webhooks` from roles and channel overwrites.
- Automatic quarantine jailing requires `/jailsetup` and `/setjail`. If the bot lacks hierarchy or API permissions to reverse an action, the failure is logged and the bot reports the failed step.

### Safety limits

The bot protects itself and your server instead of relying on Discord to throttle it:

- **Outbound request governor** (`src/services/ratelimit.rs`). Every bulk request reserves a send slot first. Five lanes have their own spacing (reads, interaction responses, member edits, channel overwrites, messages), and a shared 25 ms floor keeps the bot well under Discord's global limit.
- **429 backoff.** A real rate-limit response pauses *all* outbound requests with an exponential cooldown (2 s doubling to 30 s), instead of hammering and getting the bot flagged.
- **Coalesced repairs.** Gateway events no longer each trigger a full guild sweep: they mark the server as dirty, and one worker repairs it at most once every 20 seconds.
- **Bounded passes.** A single reconciliation pass stops after 120 writes or 10 seconds and resumes on the next pass, so the event loop keeps answering commands.
- **No wasted writes.** Overwrites are compared before being sent, and a snapshot is written only once per record and channel.

### Moderator protection

- Self-moderation and jailing the server owner are refused.
- `/jail` and `/unjail` require the configured Moderator role and a strictly higher highest-role position than the target. The owner can always moderate.
- The bot refuses to run if the `Prisoner` role has Administrator, if `@everyone` has Administrator, or if its own highest role is not above the roles it must remove.

## Technology

| Component | Technology |
|---|---|
| Language | Rust (edition 2024) |
| Discord API | Twilight 0.18 (gateway, HTTP, model, interactions) |
| Database | SQLite via SQLx 0.8 |
| Async runtime | Tokio |
| Logging | `tracing` + `tracing-subscriber` |
| TLS | rustls (ring) |

## Requirements

- Rust toolchain (stable, 1.85+)
- Cargo
- A Discord application and bot

## Installation

```bash
git clone https://github.com/GeneralErwin/Discord-Jailbot.git
cd Discord-Jailbot
cargo build --release
```

## Configuration

Create a `.env` file in the project root:

```env
DISCORD_TOKEN=YOUR_BOT_TOKEN
```

**Never publish your bot token or commit your `.env` file.** The `.gitignore` in this repository already excludes `.env`, `data/` and `target/`.

Optional: set `RUST_LOG` to change log verbosity, for example `RUST_LOG=discord_bot=debug`.

The bot creates its database automatically at `data/jail.sqlite3` and applies migrations on startup.

## Discord setup

1. Create an application in the [Discord Developer Portal](https://discord.com/developers/applications).
2. Create a bot and obtain its token.
3. Enable the **Server Members Intent** under Privileged Gateway Intents. The bot also requests the Guild Messages intent to detect webhook mass mentions.
4. Invite the bot with the `bot` and `applications.commands` scopes.
5. Grant Manage Roles, Manage Channels, Manage Nicknames, View Audit Log, and Manage Webhooks.
6. Make sure the bot's highest role is above every role it has to remove or restore.

Then, in the server:

```text
/ping
/jailsetup
/setjail
/set moderator role
/setlogs   (optional but recommended)
/quarantine   (optional)
```

View Audit Log is required for quarantine to identify who made a change. Quarantine can remove Manage Webhooks from role and channel permissions when it contains a webhook raid.

## Running

Development:

```bash
cargo run
```

Release:

```bash
cargo run --release
```

The startup log reports how long boot took:

```text
info: elapsed_ms=42 Database initialized successfully
info: elapsed_ms=613 Global slash commands registered
info: elapsed_ms=1104 Connected to Discord successfully
info: boot_ms=1108 Boot complete
```

### With PM2

PM2 can supervise the release binary directly:

```bash
cargo build --release
pm2 start ./target/release/discord-bot --name jailbot
pm2 logs jailbot
pm2 save
```

`boot_ms` is printed on every start and restart, so `pm2 logs jailbot` doubles as a startup-speed log.

## Deploying to a server

`sqlx::migrate!` embeds `migrations/` into the binary at compile time, so the migrations folder is **not** needed at runtime — but `src/` alone is **not** enough to build either.

**Build on the server (recommended).** Copy the whole project except build output and runtime data:

```text
Cargo.toml
Cargo.lock
build.rs
src/           (entire folder, including services/ratelimit.rs)
migrations/    (needed to compile)
.env           (your token, create it on the server)
```

Then run `cargo build --release` on the server. Do **not** copy `target/` or `data/`.

**Or use a prebuilt binary.** A binary built on the same OS/architecture is self-contained:

```text
discord-bot    (target/release/discord-bot, or discord-bot.exe on Windows)
.env
data/          (optional, if you want to keep existing records)
```

A Windows `.exe` will not run on a Linux VPS: build on Linux, or cross-compile for it. Use `pm2` or a systemd unit to keep the binary alive; it recreates `data/` on first start.

### Fast updates (no file dragging)

Do not re-upload the tree with WinSCP: SFTP copies file by file, and `target/` alone is hundreds of megabytes. Because the Rust toolchain is on the server, only the source ever needs to move.

One command does the upload, build and restart:

```bash
bash scripts/deploy.sh user@yourserver /opt/jailbot
```

It sends a single compressed stream (about a hundred kilobytes) and finishes with `cargo build --release` plus `pm2 restart jailbot`.

If the server can reach GitHub, skip the upload entirely:

```bash
ssh user@yourserver 'cd /opt/jailbot && git pull && cargo build --release && pm2 restart jailbot --update-env'
```

If you prefer WinSCP, use **Commands → Synchronize** (or `winscp.com /command "open ..." "synchronize remote ..."`) instead of dragging: it transfers only changed files. Either way, never upload `target/` or `data/`.

## Project structure

```text
discord-bot/
├── src/
│   ├── commands/
│   │   ├── moderation.rs     Slash command definitions
│   │   └── mod.rs
│   ├── database/
│   │   ├── config_repository.rs
│   │   ├── connection.rs
│   │   ├── models.rs
│   │   ├── repository.rs
│   │   └── mod.rs
│   ├── services/
│   │   ├── discord_moderation.rs   Jail/release/reconcile via the Discord API
│   │   ├── duration.rs             Duration parsing
│   │   ├── jail.rs                 Jail lifecycle + audit logging
│   │   ├── ratelimit.rs            Outbound request governor
│   │   └── mod.rs
│   ├── discord.rs            Gateway loop, interactions, repair queue
│   └── main.rs
├── migrations/
│   └── 0001_initial.sql
├── build.rs
├── Cargo.toml
└── README.md
```

## Database

SQLite persists:

- Guild configuration (Prisoner role, jail channel, log channel, moderator role, quarantine state)
- Per-server quarantine module states
- Active and historical jail records
- Saved member roles and original nicknames
- Original member-specific channel permissions
- Moderation audit records

Tables are created by `migrations/0001_initial.sql`. `guild_configs`, `jail_records`, `saved_roles`, `member_overwrite_snapshots` and `audit_logs` are covered by indexes on the lookup paths the bot actually uses (active sentence per guild/member, due releases, audit lookups by guild and target).

## Contributing notes

- Keep every new Discord write behind `ratelimit::guard` so pacing stays global.
- Never add a Discord call inside a hot loop without a lane.
- Prefer queueing repair work over sweeping inline in the gateway loop.

## License

[Apache License 2.0](https://github.com/GeneralErwin/Discord-Jailbot/blob/main/LICENSE)
