# Changelog

## 0.1.1

First public release.

- Builds: `topside build publish` copies builds into a synced folder (one folder
  per project and platform), keeps the newest per platform and announces them
  in the project's Telegram topic.
- Gates: `topside gate open` asks the operator in Telegram; the answer (text,
  photos, videos, a verdict) wakes the agent's T3 Code thread, or is read with
  `topside gate wait`.
- Messages are in English. Button labels are set in `[telegram.buttons]`, for
  example in the operator's language.
- macOS: `topside build open` and a launcher that starts builds from Telegram
  buttons.
- `topside guide`: the workflow for agents.
