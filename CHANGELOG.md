# Changelog

## 0.1.4

- The crate package includes this changelog.

## 0.1.3

- The repository moved to [github.com/tapyard/topside](https://github.com/tapyard/topside).
- The macOS LaunchAgent of the launcher is `com.tapyard.topside-launcher`. If an
  older one is installed, remove it first:
  `launchctl bootout gui/$(id -u)/com.dzamataev.topside-launcher` and delete
  `~/Library/LaunchAgents/com.dzamataev.topside-launcher.plist`.

## 0.1.2

- Refreshing the build folder's manifests no longer gives up when the publish
  lock only looks taken for a moment after its release (a process being started
  by another thread still holds a copy of it).

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
