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
| `Ctrl-d` `Ctrl-u` `Ctrl-f` `Ctrl-b` `Ctrl-e` `Ctrl-y` | scroll the conversation: half page, page, line |
| `J` `K` | next / previous thread |
| `/` | fuzzy thread picker |
| `Tab` / `Shift-Tab` | cycle focus: composer, chat, thread list. `Esc` returns to the composer |
| `s` `S` | toggle the sidebar / the settled shelf |
| `n` | new thread (pick a project) |
| `m` | change model |
| `i` `a` `I` `A` `o` `O` or `Enter` | write a message (Vim insert entry) |
| `za` `zR` `zM` | toggle / expand all / collapse all tool groups; `za` on a tool row inside an open group expands that row |
| `1`..`9` | answer a pending approval; otherwise a count for the next motion |
| `ga` | answer the agent's question: digits pick, `Space` toggles, `c` types a custom answer, `Enter` advances |
| `gy` | copy the last assistant message (OSC 52) |
| `gs` | toggle the thread between settled and active; in the thread list, the selected row |
| `gT` | list the agent's background tasks that are still running |
| `gl` | open lazygit in the thread's directory: a tmux popup, or in tria's place outside tmux |
| `ge` | composer focused: edit the draft in your editor, read back on exit. Chat focused: view the message, plan, tool row, or tool group under the cursor |
| `gE` | view the whole conversation in your editor |
| `gx` | open the thread's pull request in the browser |
| `gt` | switch to the tmux session named after the thread's directory, creating it if needed |
| `Ctrl-c` | interrupt the running turn |
| mouse wheel | scroll the conversation, or the thread list when the pointer is over it |
| left click | in the thread list: open a thread, or fold and unfold a section; in the chat: fold and unfold a tool group or row |
| drag | select text in the conversation; releasing copies it (OSC 52) |

With the chat focused, a line cursor moves through the conversation: `j` `k` with counts,
`{` `}` to the previous or next message, `gg` and `G` (which also resumes following new
output), `Ctrl-d` `Ctrl-u` `Ctrl-f` `Ctrl-b` `Ctrl-e` `Ctrl-y` to scroll with the cursor.
`za`, `Enter`, or `Space` folds the tool group or row under the cursor. `V` starts a linewise
selection and `y` copies it; `yy` or `3y` copy from the cursor. Clicking a chat line focuses
the chat and moves the cursor there.

`/` and `?` search the conversation forward or backward. The cursor previews the first match
while you type, `Enter` accepts, `Esc` goes back to where you were. `n` and `N` step to the
next and previous match, wrapping around with a notice. Matches are highlighted while the
chat has focus. Queries are case-insensitive unless they contain an uppercase letter. In the
chat, `?` searches; help is `?` from the composer or `:help`.

In the thread list, `j`/`k` move, `Enter` opens a thread or folds a section.

Every other key in normal mode with the composer focused edits it with Vim semantics. Motions: `h j k l w b e
W B E 0 ^ $ gg G f F t T ; ,`, all taking counts. Operators `d c y` combine with a motion or a
text object (`iw aw iW aW`, `i" a"`, `i( a( i[ a[ i{ a{ i< a<`), plus `dd cc yy D C Y x X`. `p P`
paste from the single register, `r` replaces a character, `~` toggles case, `u` and `Ctrl-r`
undo and redo. Yanks also go to the system clipboard. The status bar shows a partially typed
command.

| Insert mode | |
| --- | --- |
| `Enter` | send |
| `Alt-Enter` or `Ctrl-j` | newline |
| `Up` `Down` or `Ctrl-p` `Ctrl-n` | prompt history |
| `Esc` | back to normal mode |

Commands, entered after `:` in normal mode: `new [project]`, `model`, `effort [level]`,
`mode plan|default`, `perm <runtime mode>`, `rename [title]`, `archive`, `delete!`, `stop`,
`older`, `answer`, `dismiss`, `pr`, `git`, `edit`, `view`, `tasks`, `tmux`, `settle`, `unsettle`, `wake`, `settled`, `sidebar`, `help`, `q`.

## Branch and pull request

The header shows the thread's branch and, when the server has linked a pull request, its
number, title, draft or merged state, and a checks glyph (`✓` passing, `✗` failing, `○`
pending). The pull request on the thread's branch wins; otherwise the first open linked one.
`gx` opens it in the browser through the platform's URL opener. `:pr` does the same, and
offers a picker when several pull requests are linked.

## Git

`gl` or `:git` runs `lazygit` in the thread's working directory. Inside tmux it opens as a
popup over the pane while tria keeps running; close lazygit to return. Outside tmux, tria
steps aside, runs the command in the same terminal, and redraws when it exits. Set
`git_command` in the config file to run something else, for example `tig` or `git status`.

## Background tasks

The agent runs monitors and background commands that outlive the turn that started them.
`gT` or `:tasks` lists the ones with no reported end, with how long each has been running,
and the status bar shows a count. There is no per-task stop in the protocol, so stopping
means interrupting the turn with `Ctrl-c`.

## Editor

`ge` with the composer focused writes the draft to a private temp file, opens it in your
editor, and reads it back when the editor exits. With the chat focused, `ge` opens the block
under the cursor read only: a message as markdown, or a tool call with its input, output
summary, and changed files. `gE` opens the whole loaded conversation. The editor is `editor`
from the config file, else `$VISUAL`, else `$EDITOR`, else `nvim`; Vim-like editors get `-R`
for read-only views. Like `gl`, this uses a tmux popup inside tmux and takes over the
terminal otherwise. `:edit` and `:view` are the command forms.

Unsent composer text stays with its thread: switch away and back and the half-written
message is still there. New-thread drafts have a slot of their own.

## tmux

When tria runs inside tmux, `gt` or `:tmux` switches the client to the session named after
the thread's working directory: the worktree when the thread has one, else the project root.
The name is the directory's last path component, with `.` and `:` replaced by `_` because
tmux reserves them. If no such session exists, tria creates it in that directory first. This
suits a one-session-per-checkout layout.

## Tool output

Expanding a tool row shows what the server sends: the command or tool input, the first line
of the output or a line count, changed files, and the status. The server projects tool
payloads to that summary before they go on the wire, so the full output is not available to
any client through the orchestration API. Rows without anything beyond their summary have no
fold marker.

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
