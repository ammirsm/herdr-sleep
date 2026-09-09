# herdr-sleep

Free RAM from idle Claude chats in herdr without losing them.

An idle Claude chat is a Node process holding about 340 MB. The chat itself is on disk in
`~/.claude/projects/<cwd-slug>/<id>.jsonl`. So exiting the process loses nothing, and
`claude --resume <id>` brings it back.

- sleep: send `/exit` to the chat. The pane, tab and shell stay. RAM drops to ~0.
- wake: `herdr agent start ... -- --resume <id>` in the same pane.

## Use

```
bun run src/main.ts list                    # every chat, idle time, RAM, who would sleep
bun run src/main.ts sleep niloo w1:p5       # sleep one
bun run src/main.ts sleep --all-idle --hours 24 --dry-run
bun run src/main.ts wake niloo w1:p5        # wake one
bun run src/main.ts wake --all
bun run src/main.ts install --hours 24 --every 15m   # launchd timer, runs `auto`
bun run src/main.ts uninstall
```

Idle time comes from the transcript file mtime, not from herdr's `agent_status`.
Only chats with status `idle` sleep. Working or blocked chats never sleep.

State: `~/.config/herdr-sleep/sleeping.json`. Log: `~/.config/herdr-sleep/auto.log`.

## Known limits

- Sleep drops in-memory state: running subagents, background tasks, artifact watches.
- If text is typed in the chat prompt but not sent, `/exit` gets appended to it and may not exit.
  The tool detects that (agent still present) and reports a failed sleep.
- Wake reloads the transcript. Big chats take a few seconds.
