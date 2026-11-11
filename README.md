# herdr-chat-search

Find any conversation across **all your Herdr agents** by keyword. One Rust binary, three ways to use it:

| Mode | Command | Use it for |
|---|---|---|
| Terminal popup | `herdr-chat-search ui` | A popup inside Herdr. Type, press Enter, land in the right agent. |
| Browser panel | `herdr-chat-search serve` | A local web page (`http://127.0.0.1:5132`) that also works outside Herdr. |
| Index / list | `herdr-chat-search index`, `list` | Build the index ahead of time, or script searches. |

Created 2026-11-07, rewritten in Rust 2026-11-08. Linux only for now.

## Install

```bash
herdr plugin link /path/to/herdr-chat-search      # builds with cargo on first use
```

Bind a key in `~/.config/herdr/config.toml` (the popup is also in the command palette as "Chat Search: open"):

```toml
[[keys.command]]
key = "prefix+/"
type = "shell"
command = "herdr plugin pane open --plugin herdr-chat-search --entrypoint search"
description = "Chat search across all agents"
```

Optional config: copy `config.example.toml` to `~/.config/herdr/plugins/config/herdr-chat-search/config.toml`.
`resume_command` is the command used to reopen a closed session (`claude --resume {id}` by default).

## Terminal popup

| Key | Effect |
|---|---|
| type | search; several words = all must appear in the same message |
| `F1` | open agents only (default) |
| `F2`…`F7` | also closed Claude sessions of the last 3 / 7 / 14 / 30 / 60 / 90 days |
| `Up` / `Down` | move through the results |
| `PgUp` / `PgDn` | scroll the preview |
| `Enter` | open agent: `herdr agent focus`. Closed session: new workspace running `resume_command` |
| `Esc` | close |

## Browser panel

```bash
herdr-chat-search serve --port 5132
```

Search box, age buttons, result list and a preview of the newest matching messages. Buttons: jump to the agent,
open a closed session in a new Herdr workspace or a new terminal window, copy the resume command.
`/` focuses the search box, arrow keys move, `Enter` runs the main action.

To start it at login, use `contrib/herdr-chat-search-panel.service` (systemd user service).
The panel accepts the `Host` names `127.0.0.1`, `localhost` and `chats.localhost`. Behind a reverse proxy with another
name, add it to `extra_hosts` in the plugin config (see `config.example.toml`), otherwise it answers with 403.

**Security.** The panel shows private chat content and can start commands, and any web page in your browser can reach
`127.0.0.1`. It therefore listens on loopback only, rejects unknown `Host` headers (DNS rebinding) and foreign `Origin`
headers (cross-site requests), requires JSON for `POST`, and validates session ids before use.

## How it works

- `herdr agent list` gives each agent's Claude session id; the transcript is `~/.claude/projects/<project>/<id>.jsonl`.
- A plain-text index per session lives in `~/.cache/herdr-chat-search/` (`<id>.txt` messages, `<id>.off` read offset,
  `<id>.meta` title and working directory). Only the new part of a transcript is read on each update, and the text
  has `<system-reminder>` blocks, tool calls and thinking removed.
- A search scans only that plain text, in parallel: about 0.1 s for open agents, below 0.3 s for 90 days (700 sessions, 27 MB).
- First run builds the index: `herdr-chat-search index --age 90` (about 10 s for 700 sessions). The panel keeps it fresh in the background.

## Limits

- **Codex and other non-Claude agents** are matched by title and workspace name only. Herdr reports no session id for them,
  and reading their screen (`herdr agent read`) takes 8 to 23 s per agent.
- The index is plain text of your chats. It sits in your cache folder with the usual user permissions. Delete it with
  `rm -r ~/.cache/herdr-chat-search`; it rebuilds itself.
- Two project folders can hold a copy of the same session id; the newer file counts.

## Files

| Path | Role |
|---|---|
| `herdr-plugin.toml` | plugin manifest: popup pane `search`, action `open`, cargo build |
| `src/core.rs` | sessions, incremental index, search, actions |
| `src/tui.rs` | terminal UI (ratatui), searches in a worker thread |
| `src/panel.rs`, `src/panel.html` | local HTTP server and the single-page UI |
| `contrib/` | systemd user unit for the panel |

License: MIT
