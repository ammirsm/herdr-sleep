# herdr-sleep

Free RAM from idle Claude Code chats in [herdr](https://herdr.dev) without losing them.

Sleep a chat, keep its tab, wake it later with one keypress.

```
   zz  sleeping: birthday-workers
   press Enter to wake this chat
```

## Why

Every open Claude Code chat is a live Node process, plus its MCP server children.
Together they hold **300 to 1000 MB** each, all the time, even when nobody has typed
into the chat for weeks.

If you use herdr the way it is meant to be used, you end up with dozens of chats
across many sessions. On one machine, 68 idle chats held **31 GB**. macOS was
squeezing 44 GB into the memory compressor to cope.

The chat itself is not in that process. It is on disk in
`~/.claude/projects/<cwd-slug>/<session-id>.jsonl`. So exiting the process loses
nothing, and `claude --resume <id>` brings the same conversation back.

herdr-sleep does that for you, and makes the wake-up a single Enter in the tab.

## How it works

**Sleep**

1. Send `/exit` to the chat. The Claude process ends. The pane, tab, and shell stay.
2. Park the pane: print a note and wait in a tiny `head -n1` process (about 1 MB).
3. Record the chat's session id, working directory, and launch flags in
   `~/.config/herdr-sleep/sleeping.json`.

**Wake**

- Press Enter in the tab. The wait ends, and the shell runs
  `claude --resume <id>` with the same flags the chat had before. About one second.
- Or run `herdr-sleep wake <session> <pane>`, which presses Enter for you.

**Auto**

- `herdr-sleep auto --hours 24` sleeps every chat that herdr marks idle or done and
  that has had no message for 24 hours.
- `herdr-sleep install` adds a launchd timer (macOS) that runs `auto` every 15 minutes.

## Install

Homebrew (macOS):

```sh
brew install ammirsm/tap/herdr-sleep
```

Binary from a release:

```sh
curl -fsSL https://github.com/ammirsm/herdr-sleep/releases/latest/download/herdr-sleep-$(uname -m)-apple-darwin.tar.gz | tar xz
mv herdr-sleep /usr/local/bin/   # or anywhere on your PATH
```

With cargo:

```sh
cargo install --git https://github.com/ammirsm/herdr-sleep
```

From source:

```sh
git clone https://github.com/ammirsm/herdr-sleep && cd herdr-sleep
cargo build --release
cp target/release/herdr-sleep ~/.local/bin/
```

Needs the `herdr` and `claude` commands on your PATH. The binary is under 1 MB.

## Use

```sh
herdr-sleep list                              # every chat: idle time, RAM, who would sleep
herdr-sleep sleep niloo w1:p3 w1:p4           # sleep chosen panes
herdr-sleep sleep --all-idle --hours 24       # sleep every idle chat older than 24h
herdr-sleep sleep --all-idle --dry-run        # preview only
herdr-sleep wake niloo w1:p3                  # resume one
herdr-sleep wake --all                        # resume all
herdr-sleep auto --hours 24                   # one auto-sleep pass
herdr-sleep install --hours 24 --every 15m    # launchd timer that runs auto
herdr-sleep uninstall
```

`list` output, one row per chat. `*` marks chats that `auto` would sleep:

```
* eva          w3:p1   idle   idle=21.3d   424MB  wizard-run3-done  2eb7c60a  run-3.md execution
* niloo        w1:p2   idle   idle=19.8d   498MB  birthday-spa-manager  635948e3  Spa restaurant flowers
  slr-runner   w6:p1   working idle=0.0h  1170MB  extraction-explorer  176bee66  Extraction explorer brief

70 awake, 36.6 GB. * = idle >= 24h and would sleep
```

## Safety rules

- Only chats that herdr reports as `idle` or `done` can sleep. `working` and `blocked` never do.
- Idle time is the age of the last user or assistant message in the transcript.
  File modification time is not used: Claude Code's Remote Control appends rows to
  old transcripts on reconnect, so mtime looks fresh on chats nobody has touched in weeks.
- RAM per chat is the whole pane process group, so MCP servers are counted.
- The tool never types into a pane unless the pane is at a plain shell prompt.
  Typing into a parked pane would wake it. Typing into a chat that is still shutting
  down would lose the text.
- Wake reuses the flags the chat was launched with, for example
  `--permission-mode` or `--add-dir`. Flags that pick a conversation
  (`--resume`, `--continue`) and any initial prompt are dropped.

## Limits

- Sleep drops in-memory state that is not in the transcript: running subagents,
  background tasks, artifact watches. Idle chats have none of these.
- Any key in a parked pane wakes it, not only Enter. Harmless, but surprising.
- If a chat has unsent text in its prompt box, `/exit` is appended to that text and
  the chat does not exit. The tool reports this as a failed sleep and leaves the chat alone.
- Waking reloads the transcript. Long chats take a few seconds.
- `install` is macOS only. On Linux, run `herdr-sleep auto` from cron or a systemd timer.

## Files

- State: `~/.config/herdr-sleep/sleeping.json`
- Log: `~/.config/herdr-sleep/auto.log`
- Timer: `~/Library/LaunchAgents/dev.herdr-sleep.plist`

## Development

Rust, no runtime dependencies beyond `clap`, `serde`, and `serde_json`.

```sh
cargo test
cargo build --release
```

`prototype.ts` is the original Bun prototype where the park trick was worked out.
It is kept for reference and is not maintained.

## License

MIT
