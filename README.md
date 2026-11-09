# Discord-Jailbot

A Discord moderation bot written in **Rust**, using Twilight and SQLite. Designed to provide persistent member jailing, role restoration, and configurable jail permissions.

> **Status: Beta / Under Development**
>
> This project is actively being developed. Some planned features are not implemented yet, and the current commands have not all been verified in a live Discord server.

## Features

### Implemented
- Slash command registration.
- `/ping` — Check whether the bot is responding.
- `/jailsetup` — Set up the Prisoner role.
- `/setjail` — Configure the jail channel and channel permissions.
- `/jail` — Jail a member, optionally specifying a duration and reason.
- `/unjail` — Release a member.
- SQLite database for persistent jail records.
- Save member roles and nickname before jailing.
- Save member-specific channel permission overwrites for restoration.
- Basic moderator permission and role-hierarchy checks.

### Planned
- `/setlogs` — Configure a moderation log channel.
- `/prisoners` — List currently jailed members.
- `/jailinfo` — View a member's jail history and information.
- `/jailconfig` — Display server configuration.
- Automatic sentence expiration and release.
- Rejoin detection and automatic re-jailing.
- Permission reconciliation for newly created and updated channels.
- Detailed moderation logs and recovery handling.

## Technology

| Component | Technology |
|---|---|
| Language | Rust |
| Discord API | Twilight |
| Database | SQLite |
| Async runtime | Tokio |
| Database library | SQLx |
| Command system | Discord Slash Commands |

## Requirements

- Rust toolchain (stable)
- Cargo
- A Discord application and bot
- SQLite support (handled through SQLx)

## Installation

Clone the repository:

```bash
git clone https://github.com/GeneralErwin/Discord-Jailbot.git
cd Discord-Jailbot
```

Build the project:

```bash
cargo build --release
```

## Configuration

Create a `.env` file in the project root:

```env
DISCORD_TOKEN=YOUR_BOT_TOKEN
```

**Never publish your bot token or commit your `.env` file.**

The bot creates its SQLite database automatically at:

```text
data/jail.sqlite3
```

Database migrations are applied automatically when the bot starts.

## Discord Setup

1. Create an application in the [Discord Developer Portal](https://discord.com/developers/applications).
2. Create a bot and obtain its token.
3. Enable the **Server Members Intent** under Privileged Gateway Intents.
4. Invite the bot with the `bot` and `applications.commands` scopes.
5. Grant the permissions it needs, including Manage Roles, Manage Channels, and Manage Nicknames.
6. Make sure the bot's highest role is above the roles it needs to manage.

After starting the bot, use:

```text
/ping
/jailsetup
/setjail
```

Then configure the jail channel before using `/jail`.

## Running

Development:

```bash
cargo run
```

Production:

```bash
cargo run --release
```

## Project Structure

```text
discord-bot/
├── src/
│   ├── commands/
│   ├── database/
│   ├── events/
│   ├── services/
│   ├── discord.rs
│   └── main.rs
├── migrations/
│   └── 0001_initial.sql
├── build.rs
├── Cargo.toml
└── README.md
```

## Database

The bot uses SQLite to persist:

- Guild configuration
- Active and historical jail records
- Saved member roles
- Original member-specific channel permissions
- Moderation audit records

Data is stored locally in the `data/` directory.

## License

[License](https://github.com/GeneralErwin/Discord-Jailbot/blob/main/LICENSE)
