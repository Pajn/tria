# tria

A terminal chat client for [T3 Code](https://github.com/pingdotgg/t3code), written in Rust
with ratatui. It does one thing: a thread picker, a chat view, and a composer, driven with
Vim-style keys. No diff viewer, terminal, browser, or settings. Use the desktop, web, or mobile
app for those.

## Requirements

- A running T3 Code server, either the desktop app or `npx t3@latest`, on the same machine or
  reachable over the network.
- Rust 1.88 or newer to build.

## Setup

Build and install:

```sh
cargo install --path .
```

Mint a pairing credential on the machine that runs the server. The desktop app offers this
under Connections; the CLI prints one with:

```sh
t3 pair
```

Exchange it once for a stored bearer token. Either the raw token or the full `/pair#token=...`
URL works:

```sh
tria pair <credential>
```

The token is stored in `$XDG_CONFIG_HOME/tria/config.toml` (default `~/.config/tria/config.toml`)
with mode 0600. `tria` finds a local server through the runtime file the server writes under the
T3 home directory. For a remote server pass `--url https://host:port` to `tria pair` and it is
remembered.

Then run:

```sh
tria
```

## Keys

Press `?` inside the app for the full list.

| Normal mode | |
| --- | --- |
| `j` `k` `Ctrl-d` `Ctrl-u` `gg` `G` | scroll the conversation |
| `J` `K` | next / previous thread |
| `/` or `Space` | fuzzy thread picker |
| `Tab` | focus the thread list; `j`/`k`, `Enter` opens a thread or folds a section, `Esc` |
| `s` `S` | toggle the sidebar / the settled shelf |
| `n` | new thread (pick a project) |
| `m` | change model |
| `i` or `Enter` | write a message |
| `za` `zR` `zM` | toggle / expand all / collapse all tool groups |
| `1`..`9` | answer a pending approval |
| `a` | answer the agent's question: digits pick, `Space` toggles, `c` types a custom answer, `Enter` advances |
| `y` | copy the last assistant message (OSC 52) |
| `Ctrl-c` | interrupt the running turn |
| mouse wheel | scroll the conversation, or the thread list when the pointer is over it |
| left click | in the thread list: open a thread, or fold and unfold a section |

| Insert mode | |
| --- | --- |
| `Enter` | send |
| `Alt-Enter` or `Ctrl-j` | newline |
| `Up` `Down` or `Ctrl-p` `Ctrl-n` | prompt history |
| `Esc` | back to normal mode |

Commands, entered after `:` in normal mode: `new [project]`, `model`, `effort [level]`,
`mode plan|default`, `perm <runtime mode>`, `rename [title]`, `archive`, `delete!`, `stop`,
`older`, `answer`, `dismiss`, `settle`, `unsettle`, `wake`, `settled`, `sidebar`, `help`, `q`.

## Thread list

The sidebar mirrors the desktop app's sections: pinned, active, snoozed, and settled. Settled
threads are ones the server has parked after their turn finished; they render grayed out in a
shelf that starts folded (113 of them is normal). Press `S`, `:settled`, or `Enter` on the shelf
header to unfold it. `J`/`K` and the picker only cycle through visible threads.

Each row shows the project on the right and a status glyph on the left: `!` needs an approval
or an answer, a spinner is working, `◔` is monitoring (background work still running after the
turn), `▤` has a plan ready, `✗` failed. The status bar spells the status out for the open
thread. `:settle` and `:unsettle` move the current thread between the active list and the
shelf; `:wake` ends a snooze.

## Other subcommands

- `tria probe` connects, prints the server config keys and the thread list, and exits.
- `tria dump <thread-id>` opens a thread through the same connection code as the UI and prints
  reduced state for a few seconds. Both are useful when checking a new server release.

Logs go to `tria.log` next to the config file. Set `TRIA_LOG=debug` for more detail.

## How it talks to the server

See [PLAN.md](./PLAN.md) for the protocol notes: pairing and WebSocket tickets, the Effect RPC
JSON framing, the shell and thread subscriptions, and the dispatch commands this client uses.
