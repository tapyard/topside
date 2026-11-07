# topside

Builds and operator gates for agent-driven game development.

AI coding agents can build a game on their own until they hit a question only a
human can answer: does the jump feel right, does the menu read well on a phone,
may this be released? topside gives such agents a way to reach a human operator
who is not at the machine, and gives the operator the builds to look at.

- **Builds.** An agent publishes a build. It lands in a synced folder (for
  example Syncthing) on the operator's devices, and a Telegram message says
  what is new.
- **Gates.** An agent asks the operator a question. The question appears in
  the project's Telegram topic. The operator replies with text, photos or
  videos and presses a button. The answer arrives in the agent's
  [T3 Code](https://github.com/pingdotgg/t3code) thread as a new message and
  wakes the agent up. Other agents can block on `topside gate wait` instead.

The name comes from diving: the agents work below, and the crew topside keeps
the line open, answers their questions and takes in what they bring up.

```
agent ── topside gate open ──▶ SQLite ──▶ topside serve ──▶ Telegram topic ──▶ operator
  ▲                                            │                                 │
  └──────── new message in the agent's thread ◀┴──────── reply + button ◀────────┘

agent ── topside build publish ──▶ <synced folder>/<project>/<platform>/… ──▶ operator's devices
```

The service makes only outbound connections (Telegram long polling), so the
build machine needs no public address and can move between networks.

Agents learn the workflow from `topside guide` ([src/guide.md](src/guide.md)).
The design and the reasons behind it are in [docs/design.md](docs/design.md).

## What you need

- A build machine where the agents work, with git and the `topside` binary.
- A Telegram bot, and a Telegram supergroup with topics: one topic per project.
- A folder that reaches the operator's devices. Syncthing works well; any
  synced folder will do.
- To wake agents up: [T3 Code](https://github.com/pingdotgg/t3code) and its `t3`
  CLI. Without it, agents use `topside gate wait`.
- Optionally, on the operator's Mac: `topside` to unpack and start the newest
  macOS build, including from a button in Telegram.

## Install

```sh
cargo install topside          # Rust 1.89 or newer
```

The binary is self-contained (SQLite bundled, rustls).

## Set up

### 1. Telegram

1. Create a bot with [@BotFather](https://t.me/BotFather). Put its token into a
   file, one line, readable only by you (`chmod 600`).
2. Create a group, turn on topics, and create one topic per project.
3. Add the bot to the group as an **administrator**. An administrator bot sees
   every message, not only replies to it, so a plain message in a topic can
   count as an answer.

### 2. Configuration

`~/.config/topside/config.toml` on the build machine:

```toml
builds_root = "~/builds"             # the synced folder
keep = 5                             # builds kept per project and platform
# t3_cli = "~/bin/t3"                # the T3 Code CLI; default: t3 in PATH

[telegram]
token_file = "~/.config/topside/telegram-token"
chat_id = -1001234567890             # the group
allowed_users = [123456789]          # Telegram user ids allowed to answer gates
# proxy = "socks5h://127.0.0.1:1080" # for Telegram only, where it is blocked

[projects.my-game]
repo = "~/dev/my-game"               # its worktrees belong to the project too
topic = 12                           # the project's topic in the group
title = "My Game"                    # shown in messages; the key by default
```

To find the ids, start the service (below) and send `/gates@<your_bot>` in a
project's topic. `topside status` and the service log then show the chat id,
the topic id and the topic's **name**. Match topics by their name.
`topside status` keeps checking the chat and the topics against the config.
`topside telegram test --project my-game` sends a test message.

Paths: the data lives in `~/.local/share/topside/` and the cache in
`~/.cache/topside/` (the XDG variables are respected). `$TOPSIDE_CONFIG` points
to another config file. `$TOPSIDE_HOME` puts the config, the data and the cache
into one folder.

### 3. Button labels

The operator presses buttons under each gate. Their labels can be set, for
example in the operator's language:

```toml
[telegram.buttons]
send = "📨 Отправить агенту"         # send the replies to the agent as the answer
accept = "✅ Принято"                # accept; anything written goes along as remarks
launch = "▶️ Запустить на Mac"       # start the build on the Mac (see below)
```

The defaults are `📨 Send to agent`, `✅ Accepted` and `▶️ Run on Mac`. The
gate message refers to the buttons by these same labels.

### 4. The service

```sh
topside serve      # foreground; one instance per data folder
topside status     # config, heartbeat, queued messages, undelivered answers
```

As a systemd user unit (`~/.config/systemd/user/topside.service`):

```ini
[Unit]
Description=topside: builds and operator gates
After=network-online.target

[Service]
ExecStart=%h/.local/bin/topside serve
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

`systemctl --user enable --now topside`. On a machine without a login session,
also run `loginctl enable-linger`.

### 5. The project

A project usually has a script that exports the game and publishes it:

```sh
godot --headless --export-debug "macOS" build/macos/game.zip
godot --headless --export-debug "Linux" build/linux/game.x86_64
topside build publish macos=build/macos/game.zip linux=build/linux
```

Point the project's agent instructions (`AGENTS.md`, `CLAUDE.md`) to
`topside guide`, and add what is specific to the project: the publish script,
which questions need the operator, the operator's language.

An in-game "questions about this build" button can open the project's topic:
`topside url --project my-game` prints the link.

## For the operator

- A gate is one message: what to do, numbered questions, the build to check.
  Reply to it with text, photos or videos (several messages are fine), then
  press the send button. Or press the accept button, alone or after writing
  remarks.
- A plain message in the topic, not a reply, counts as an answer when exactly
  one gate of the project is open. With several open gates, the bot asks which
  one the message is for.
- `/gates` in a topic lists its open gates.
- Answering from a shell works too: `topside gate answer <id> --text "…"`, or
  `--accept`.

### Starting builds on a Mac

On the Mac, a config with only `builds_root` is enough:
`topside build open my-game` unpacks the newest macOS build from the synced
folder and starts it.

To also start builds from Telegram, add the launcher on both machines:

```toml
[launcher]
url = "http://127.0.0.1:47711"                    # 127.0.0.1: Telegram rejects localhost
secret_file = "~/.config/topside/launcher-secret" # the same 32+ characters on both machines
```

On the Mac, run `topside launcher install`. It installs a LaunchAgent; the log
is in `~/Library/Logs/topside-launcher.log`. Build announcements, gates and
"build is ready" messages then carry a launch button when there is a macOS
build. Telegram allows only http(s) links in buttons, so the button opens
`http://127.0.0.1:47711/open?…` in the browser. The launcher on the Mac checks
the link's signature (HMAC with the shared secret, so that no web page can make
the Mac start anything), waits for the synced folder to finish copying, and
starts exactly that build.

## Commands

```sh
topside build publish <platform>=<path>… [--tag T]
topside build list [--project P] [--json]
topside build latest P [--platform macos]
topside build open P [--platform macos] [--print]
topside build reindex P                        # adopt files already in the folder

topside gate open --title T [--body B | --body-file F] -q Q… [--attach F…] [--thread ID | --no-thread] [--commit SHA | --no-commit]
topside gate list [--project P] [--status open|answered|withdrawn|all] [--json]
topside gate show ID [--json]
topside gate ask ID "question" | note ID "text" | attach ID FILE… | withdraw ID [--reason R]
topside gate wait ID [--timeout 30m]           # exit 0 answered, 3 withdrawn, 75 timeout
topside gate answer ID [--text T] [--file F] [--accept]   # for the operator
topside gate set-thread ID THREAD              # deliver the answer to another thread

topside status [--json]
topside url [--project P]                      # link to the project's topic
topside telegram test --project P
topside launcher serve | install | link P FILE
topside guide
```

## Safety

- Only `allowed_users`, in the configured group, can answer gates. Everything
  else is ignored.
- A publish copies files to every device of the synced folder, so it refuses
  key and credential files, hidden folders, and folders git tracks (see
  `topside guide`).
- The bot token and the launcher secret live in their own files, not in the
  config.

## Limits

- The standard Bot API downloads files up to 20 MB and uploads up to 50 MB.
  The operator is asked to shorten larger videos or put them into the synced
  folder. Builds are announced in Telegram, never sent through it.
- A gate, like every message topside sends, fits one Telegram message. The CLI
  checks the length when the gate is opened.

## License

MIT
