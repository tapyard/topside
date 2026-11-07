# topside: guide for agents

topside connects you with a human operator who is not at the machine. It does
two things:

- **builds**: you publish a build; it lands in a synced folder on the
  operator's devices, and they get a Telegram message saying what is new;
- **gates**: you ask a question that only the operator can answer; they answer
  in Telegram, and the answer comes back to you as a new message in your thread.

You use the `topside` CLI. The operator uses Telegram. Commands that reach the
operator return as soon as their part is recorded; a background service
delivers it, so a slow network or Telegram outage never blocks you.

## Which project

Run commands from the project's checkout: its main checkout or any worktree of
it. topside finds the project by the repository. Elsewhere, pass
`--project <name>`; the error lists the configured names.

## Builds

Publish every build the operator should run: the build for a gate, or a build
after a change they asked for.

```sh
topside build publish android=build/game.apk macos=build/macos/game.zip linux=build/linux
```

- Arguments are `<platform>=<path>`. Platforms are free names: `android`,
  `macos`, `linux`, `windows`, `web`. A folder is packed into `.tar.gz`.
- The build records the commit: `HEAD` of the checkout. Only a committed tree
  is published, so commit first. (`--allow-dirty` exists, but a build from
  local edits cannot be reproduced; never use it for a build a gate refers to.)
- A build that is not from the main line gets `--tag <name>`, for example
  `--tag prototype`.
- The newest builds per platform are kept (the limit is in the config); older
  ones are deleted from every device.
- If the project has its own script that exports and publishes, use that
  script.

The synced folder reaches every device of the operator, so a publish refuses
anything that does not look like an export. That means a key or credentials
file (`.env`, `*.pem`, `*.keystore`, `id_rsa`, `secrets/`, …), a hidden folder,
or a folder that git tracks (export into an ignored folder such as `build/`).
The error names the path. Fix the export; do not work around the check.

`topside build list` shows what is published. `topside build latest <project>`
prints the path of the newest build.

## Gates

Open a gate when the next step needs the operator's eyes, taste or permission.
For example: how the controls feel, whether a screen reads well on the device,
whether a model looks right, whether something may be released or pushed where
others see it.

Do not open a gate for something you can find out yourself: a test, a
measurement, a screenshot you can compare, the project's documentation.

```sh
topside gate open --title "Wall drawing" \
  --body "Day phase: Build → Walls. Drag a line with one finger, release, then start the night." \
  -q "Is drawing a wall comfortable? Any gaps on diagonals?" \
  -q "Do enemies break through and use the gap believably?" \
  --attach shots/wall.png
```

It prints the gate id.

### Writing a good gate

- **One gate, one decision.** Unrelated questions go into separate gates, so
  each can be answered and closed on its own.
- **Write in the operator's language.** The project's agent docs usually say
  which one it is.
- **The title** says what is being checked.
- **The body** says exactly what to do before answering: where to go and which
  steps to take. The operator reads it on a phone, next to the game.
- **Each `-q`** is one question that can be answered in a sentence. Up to 12.
- **Long material** goes into a file with `--attach` (screenshots, a video, a
  log, a longer description). `--body-file <file>` (or `-` for stdin) reads
  the body from a file.
- **The whole gate is one Telegram message.** The title, the body, each
  question and all of them together have limits. The error names what is too
  long and the limit. Shorten the text or attach the details.

### The gate and its build

A gate records the commit under review: `HEAD` of your checkout, or
`--commit <sha>`. The operator is shown the newest published build that contains
that commit. So **publish a build that contains the commit**, before or after
opening the gate. If the build comes later, the gate gets a "build is ready"
message when it appears.

A gate that is not about a build (for example "may I push this?") takes
`--no-commit`.

### After opening a gate

The answer wakes **your T3 thread**: the one in `$T3CODE_THREAD_ID`, or
another one passed with `--thread <id>`. **End your turn after opening the
gate**, and say that you are waiting for the operator. Do not wait or poll.

Without a T3 thread (`--no-thread`, or another harness), block instead:
`topside gate wait <id> [--timeout 30m]`. Exit code 0 means answered (the
answer is on stdout), 3 means withdrawn, 75 means the wait timed out.

While a gate is open:

- `topside gate ask <id> "<question>"` adds a question;
- `topside gate attach <id> <files>` adds screenshots or files;
- `topside gate note <id> "<text>"` adds a remark that needs no answer;
- `topside gate withdraw <id> --reason "<why>"` takes the gate back when it is
  no longer needed. If the work under review has changed a lot, withdraw the
  gate and open a new one for the new work instead of adding to the old one.

`topside gate list` shows the gates; `topside gate show <id>` shows everything
about one, including the answer.

## The answer

The answer arrives in your thread as a new message that starts with
`[topside]`. It contains:

- the verdict;
- the operator's text;
- the paths of the photos, videos and files they sent (on this machine);
- the gate's questions, so you can match answers to them.

The verdict is one of three:

| Verdict line | `--json` `verdict` | What to do |
|---|---|---|
| `Verdict: accepted.` | `accepted` | The check passed; move on. |
| `Verdict: accepted with remarks.` | `accepted_with_notes` | It passed. Take the remarks into account (small fixes, notes for later); no new gate is needed for them. |
| `Verdict: answered.` | `answered` | Read the answer and act on it. |

The operator often answers with photos or screen recordings. Look at the
photos. A video is easier to study as frames, for example
`ffmpeg -i clip.mp4 -vf fps=1 /tmp/frame-%03d.png`.

Fixes go into new commits. If the operator should look again, publish a new
build and open a new gate. Never reopen an answered gate. Never answer a gate
yourself: `topside gate answer` is for the operator.

## When something goes wrong

- **"not inside a git checkout" / "belongs to no configured project"**: run
  from the project's checkout or pass `--project`.
- **"has uncommitted changes: only committed builds are published"**: commit,
  then publish.
- **A publish refuses a path**: the export picked up something that must not
  be shared (see Builds). Change the export.
- **"… is N characters, at most M"** or **"the gate is N characters …"**:
  shorten it, or move the details into `--attach`.
- **The answer was not delivered to your thread** (for example the thread was
  replaced): the operator sees this in Telegram. The answer is kept:
  `topside gate show <id>` prints it, and `topside gate set-thread <id> <thread>`
  delivers it to another thread.
- **Telegram is down or the service is stopped**: nothing to do. Gates and
  builds are queued and sent later; `topside status` shows the queue.

## Exit codes

0 success · 1 error (message on stderr) · 3 gate withdrawn · 75 wait timed out.
Every read command accepts `--json`.
