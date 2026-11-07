# topside: design

## 1. Why

AI agents can develop a game on a machine of their own (here: Godot projects,
agents in T3 Code) while the operator directs them from a laptop and a phone.
Two things need the operator, and both used to be ad-hoc per project:

- getting a build onto the operator's devices, with a word on what changed;
- asking the operator something only a human can judge, and getting the answer
  back to the agent that asked, even when that agent's session has ended.

topside does both for every project on the build machine, through tools the
operator already uses: a synced folder for files and a Telegram group for
conversation.

## 2. Decisions

### 2.1 Gates live in Telegram, not on a web page

The operator already gets notifications in a Telegram group with one topic per
project. A gate there is one message with numbered questions and two buttons;
the operator replies to it (text, photos, videos) and presses the send button,
or the accept button. Button labels are configurable, so the operator can have
them in their own language. The texts topside itself writes are in English.

- No inbound port, certificate, DNS name or token in a URL: the service long-polls
  `getUpdates`. The build machine works the same at home and away.
- Media from a phone is natural in Telegram and awkward in a browser form.
- Limits accepted for version 1: the standard Bot API downloads at most 20 MB
  (the operator is told to shorten a video or drop it into Syncthing) and
  uploads at most 50 MB (builds are not sent through Telegram — only announced).
  A local Bot API server would lift both, but topside is written for the standard
  one (fixed limits, files fetched over HTTP); `api_base` only points elsewhere.
- Only `allowed_users` in the configured `chat_id` can answer; everything else is
  ignored silently.

### 2.2 The answer wakes the agent's T3 thread

`t3 thread send <thread> -` (T3 Code's CLI) queues a message as the
thread's next turn and never interrupts a running one. The thread id comes from
`$T3CODE_THREAD_ID`, which T3 sets for Claude and Codex sessions; other harnesses
pass `--thread` or block on `topside gate wait`. Delivery is retried (8 attempts,
backoff up to 15 min); a final failure is reported in the gate's Telegram
thread, and `topside gate set-thread` re-delivers.

So an agent opens a gate and ends its turn; it does not hold a turn open waiting.

Every message topside sends is a single Telegram message: the CLI limits what an
agent writes, and the service shortens what still does not fit (on the raw text,
before escaping). So an event is either delivered or not — a retry never leaves
half of a gate without its buttons. Delivery is strictly in order within a
project (its topic reads top to bottom); a project whose next event keeps
failing holds up only itself. An error Telegram will repeat forever (a
malformed request) is dropped after three attempts so it cannot hold up the
project, while setup errors (the bot is not in the group yet) and network errors
keep being retried; an upload that keeps timing out is replaced, on the third
try, by a note with the file's path. An operator's message that cannot be
handled (a download that fails three times) is answered with a request to send
it again; an edited answer message updates the answer while the gate is open.

### 2.3 One store, the CLI never waits for the network

SQLite (bundled) in `~/.local/share/topside/`. Agent commands write rows and an
outbox event and return; `topside serve` sends events and wake-ups with retries.
A broken network or a stopped service delays a message, never loses it, and never
blocks an agent. Files the operator sends and the agent attaches are copied into
`gates/<id>/` — the service sends and reports only files it owns.

The schema migrates itself. A topside refuses a database newer than it knows,
unless the topside that migrated it recorded that older ones can still use it
(`min_reader_schema` in `meta`: a migration that only adds keeps it), so a
rollback to the previous release keeps working. A deploy script should still
copy the database before a restart and restore it if the release it rolls back
to cannot open it.

### 2.4 Builds: one Syncthing folder, a folder per project

`<root>/<project>/<platform>/<project>-<YYYYmmdd-HHMM>-<sha7>[-<tag>]-<platform><ext>`.
The name carries date and commit (readable without topside); `builds.json` lets
topside on another machine find the latest build without the database
(`topside build open` on the Mac); `LATEST.md` is for people. Only a clean
checkout is published. A file appears in the folder by one rename, so Syncthing
never sends half a build. `keep` newest per platform stay; older ones are
deleted (and disappear on the receivers).

A Syncthing folder that is send-only on the build machine fits well: every
device of the operator receives builds, and none can change them.

### 2.5 A gate knows its commit, so it knows its build

A gate records the `HEAD` of the checkout it was opened from. The build shown is
the newest per platform whose commit contains it (`git merge-base
--is-ancestor`), untagged builds preferred over tagged ones (a prototype branch
also contains main). When a build that contains an open gate's commit is
published, the gate's Telegram thread gets "build is ready".

## 3. Not in version 1

- A web page (read-only list, uploads over 20 MB). Telegram covers the flow;
  revisit if the limits bite.
- Several build machines in one Telegram group: works if each has its own bot
  and topics; a shared view across machines is not designed.
- Signing, notarisation, store uploads — project scripts' business.
- Telegram-side creation of gates or free-form chat with agents: the operator
  talks to agents in T3.
