# tria

A terminal chat client for [T3 Code](https://github.com/pingdotgg/t3code), written in Rust
with ratatui. It stays small: a thread picker, a chat view, a composer, and the thread's
terminals, driven with Vim-style keys. No diff viewer, browser, or settings. Use the desktop,
web, or mobile app for those.

## Requirements

- A T3 Code server, either the desktop app or `npx t3@latest`, on the same machine or reachable
  over the network. It need not already be running: see [Starting the server](#starting-the-server).
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

## Starting the server

tria is a client, so it needs a server to talk to. If none is answering on this machine when
you run `tria`, it starts one with `t3 serve` and waits for it to come up — the desktop app
does not have to be open. It says what it is doing, then connects:

```
No T3 Code server running. Starting one with `t3 serve`.
Server at http://127.0.0.1:3773. It keeps running after tria exits.
```

The server is left running on purpose. It owns provider sessions, background tasks, and
terminals, all of which outlive a chat window, so quitting tria is not a reason to take them
down. The next `tria` finds it and connects straight away. Stop it like any other process, or
install it as a background service with `t3 service install` and it will always be up.

Set `server_command` in the config file to start it some other way:

```toml
server_command = "npx t3@latest serve"
```

It runs through a shell, so a full command line works. Set it to `""` to never start a server;
tria then reports that nothing is running and exits. Output from a start goes to
`server-start.log` beside the config file, and a command that exits without serving is reported
with the first lines of it.

Only a server on this machine is ever started. Pointed at another host with `--url` or a stored
remote origin, an unreachable server is reported, not replaced.

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
| `gw` | new thread: start it in a fresh worktree, or the project's checkout |
| `m` | change model |
| `i` `a` `I` `A` `o` `O` or `Enter` | write a message (Vim insert entry) |
| `za` `zR` `zM` | toggle / expand all / collapse all tool groups; `za` on a tool row inside an open group expands that row |
| `1`..`9` | answer a pending approval; otherwise a count for the next motion |
| `ga` | answer the agent's question: digits pick, `Space` toggles, `c` types a custom answer, `Enter` advances |
| `gy` | copy the last assistant message (OSC 52) |
| `gs` | toggle the thread between settled and active; in the thread list, the selected row |
| `gT` | list the agent's background tasks that are still running |
| `gA` | list the subagents the thread has run; `Enter` reads one's transcript (`r` re-reads a running one), `y` yanks its report |
| `gS` | list the thread's terminals; attach to one, or close, restart, open a new one |
| `gl` | open lazygit in a terminal popup for the thread |
| `g!` | a shell in the popup, in the thread's directory |
| `ge` | composer focused: edit the draft in your editor, read back on exit. Chat focused: view the message, plan, tool row, or tool group under the cursor |
| `gE` | view the whole conversation in your editor |
| `gx` | open the link on the cursor's line, else the thread's pull request |
| `gt` | switch to the tmux session named after the thread's directory, creating it if needed |
| `Ctrl-c` | interrupt the running turn |
| mouse wheel | scroll the conversation, or the thread list when the pointer is over it |
| left click | in the thread list: open a thread, or fold and unfold a section; in the chat: open a link, or fold and unfold a tool group or row |
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
| `←` `→` `Home` `End`, `Ctrl-a` `Ctrl-e` | move the cursor; `Alt` with an arrow, or `Alt-b` `Alt-f`, moves by word |
| `Backspace` `Delete`, `Ctrl-w` `Ctrl-k` `Ctrl-u` | delete a character, the word before the cursor, to the end, to the start |
| `Esc` | back to normal mode |

The custom answer typed after `ga` then `c` is edited with those same keys. It is one line:
a long answer scrolls inside its row rather than wrapping, `Enter` confirms it, and `Esc`
goes back to the options without keeping it.

Commands, entered after `:` in normal mode: `new [project]`, `model`, `effort [level]`,
`mode plan|default`, `perm <runtime mode>`, `rename [title]`, `archive`, `delete!`, `stop`,
`older`, `answer`, `dismiss`, `pr`, `git`, `shell`, `edit`, `view`, `tasks`, `terminals`, `tmux`,
`worktree`, `settle`, `unsettle`, `wake`, `settled`, `sidebar`, `agents`, `help`, `q`.

## New threads

`n` picks a project and opens a draft; the message you write starts the thread. The header says
where it will run: `⌂ project checkout on <branch>`, or `⌂ new worktree off <branch>`, which
the server creates when the thread starts. `gw` or `:worktree` switches between them. The
default comes from the project's own setting, then the server's, and the server's default is
the current checkout.

A worktree branches off whatever the project's checkout has at the time, or off the remote's
copy of it where the server is set to start new worktrees from origin. Its branch is named
after the thread rather than the message, since the server names the worktree's directory
after the branch and a message is not a branch name.

The model for a new thread is the last one you picked with `m`, remembered in the config file
between runs. Without one it falls back to the project's default model, then the server's.

## Branch and pull request

The header shows the thread's branch, the name of its worktree after `⌂` when it has one of
its own, and, when the server has linked a pull request, its number, title, draft or merged
state, and a checks glyph (`✓` passing, `✗` failing, `○` pending).

After the branch comes the state of the checkout: `●` with `+` and `−` line counts when the
working tree is dirty, `↑` commits ahead of the upstream and `↓` commits behind.

The thread list only carries a branch for threads the server created one for, so for the rest
tria watches the thread's checkout and shows the branch that is actually checked out there.
Threads sharing a project's checkout therefore show the same branch, which is the truth: they
share it.

The server caches its git status and does not report every edit, so tria asks it to re-read the
checkout when the thread opens, when a turn finishes, when you leave the terminal popup, and
every twenty seconds otherwise. The pull request on the thread's branch wins; otherwise the first open linked one.
`gx` opens it in the browser through the platform's URL opener. `:pr` does the same, and
offers a picker when several pull requests are linked.

## Git

`gl` or `:git` runs `lazygit` in a terminal of the thread's own, as a popup inside tria: the
chat stays on screen around it and tria keeps running. The command replaces the shell rather
than running inside it, so the popup goes straight to lazygit and closes again when you quit
it. Set `git_command` in the config file to run something else, for example `tig` or `gitui`;
it should be interactive, since a command that prints and exits takes the popup with it.

While lazygit is up, `Ctrl-\` leaves it running and `gl` comes back to it exactly as it was.

The terminal belongs to the server, so this works the same whether the server is on this
machine or another one, and lazygit runs where the repository is.

Startup goes through your login shell, which is the whole of what a popup costs, and a thread
pays it once: closing the popup closes the terminal and opens another in its place, so the
shell is already at a prompt when you next ask for lazygit. Leaving a thread lets go of the
one it was keeping. The pane also answers the capability queries a full-screen program sends
on startup, so nothing waits on a timeout.

`g!` or `:shell` opens a plain shell in the same popup, in the thread's directory. Exiting it,
with `Ctrl-d` or `exit`, closes the popup. Both of these are scratch sessions: tria closes the
ones it opened and leaves the desktop app's own alone.

## Links

URLs in the conversation are underlined, and clicking one opens it in the browser. With the
chat focused, `gx` opens the link on the cursor's line, falling back to the thread's pull
request when the line has none.

Links are found on the drawn screen rather than in the message text, so one that wraps across
two lines is still whole, and the same goes for a URL inside a tool row or a plan. Trailing
sentence punctuation is left out of the link; brackets are kept when the URL opened them
itself.

## Background tasks

The agent runs monitors and background commands that outlive the turn that started them.
`gT` or `:tasks` lists the ones with no reported end, with how long each has been running,
and the status bar shows a count. There is no per-task stop in the protocol, so stopping
means interrupting the turn with `Ctrl-c`. Work the agent delegated to a subagent is not
background work and is listed by `gA` instead.

## Subagents

The agent delegates work to subagents that run their own conversation out of sight of the
thread. `gA` or `:agents` lists them: what each was asked for, the agent definition it runs
as, how long it has been going or how long it took, what it is doing now or the first line
of the report it came back with, and its model, tokens, and tool count. The status bar
carries a count while any are working.

They come from the same `task.*` activities as the background tasks, told apart by the
`agentKind` the server stamps on each one as it ingests it. So `gT` lists monitors and
backgrounded commands, `gA` lists subagents, and neither shows the other. Like a background
task, a subagent has no stop of its own in the protocol: `Ctrl-c` interrupts the turn that
started it.

A subagent can run more than once under the same identity. The row then reads `run 2`, and
shows the current run rather than the last one's outcome. If the provider session dies with
subagents still working, they are reported as stopped rather than left spinning, since the
processes that would have finished them died with it.

`y` copies the report. `Enter` reads the whole transcript.

## Subagent transcripts

`Enter` on a row opens the subagent's own conversation in place of the thread's: the brief it
was given, everything it said, and every tool call it made, in the same view with the same
keys. Scrolling, folding, search, `ge`, `gE`, and yanking all work as they do in the chat,
because it is the same renderer — the transcript's rows are translated into the messages and
tool calls the server would have sent for the same work. `q` or `Esc` goes back to the list,
and the conversation returns to where it was.

The transcript is a file the provider wrote on the machine that ran the agent, fetched with
`projects.readFile`, which takes an absolute path for exactly this. It therefore works against
a remote server too. The server stops reading at a megabyte and the header says so when the
tail is missing.

A subagent that is still working can be read as far as it has got: the provider writes the
file from the moment the agent starts, but it only reports the path once the task is over, so
the path is taken from another task in the same thread — they are written side by side in one
directory, each named after its task. The header says the run is still going, and `r` reads it
again for whatever has been written since. In a thread where no task has finished yet there is
nothing to take the directory from, and the row says it has no transcript.

Unlike the tool rows in the thread, these are not projected down to a summary on the way out:
the file has the whole input and the whole output, so an expanded row shows the command it ran
and what came back.

The format is the provider's rather than the protocol's, so this reads what Claude's agent
sessions record. A row that does not fit the shape is skipped rather than fatal, which is why
the header counts what was read.

## Terminals

Each thread can have shells running on the server, the same ones the desktop app shows in its
terminal tabs. `gS` or `:terminals` lists them with their status, working directory, process
id, and whether a command is running right now. `x` closes the selected one, `r` restarts
it in the same directory, and `c` opens a new one in the thread's working directory.

`Enter` attaches: the shell opens as a popup over the chat and every key goes to it, `Ctrl-c`
included. `Ctrl-\` detaches and leaves the shell running, and exiting the shell closes the
popup. A program that asks for the mouse gets it, so clicking and scrolling work inside lazygit
and anything else full screen; otherwise the wheel moves through the pane's own scrollback, as
does `Shift` with the wheel while a program is using the mouse. Typing returns to the live
screen, and resizing the window resizes the pty. The pane is a real terminal emulator, so
full-screen programs work: this is the other way to reach lazygit, and unlike `gl` it stays
inside tria and survives detaching.

The server owns the pty, so a shell you start here also appears in the desktop app's terminal
tabs, keeps running after you detach or quit, and picks up where it left off when you attach
again.

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
fold marker. A subagent's tool calls are the exception: those come from a file rather than the
wire, and keep what they sent and what they got back.

An image the agent read is drawn rather than named: unfolding the row puts the picture under
the line, as wide as the chat and at most half as tall, scrolling with the conversation like
any other lines. The summary the server sends holds the path the tool read and not the
picture, so the file is read from disk — which works when the server is on this machine, and
is why an image row against a remote server has nothing to unfold. A subagent's transcript is
the exception again: the provider's file carries its pictures inside it, and those draw
wherever the server runs.

How it is drawn is up to the terminal, which is asked once at startup: the kitty, iTerm2, or
sixel graphics protocol where it speaks one, and half-blocks where it speaks none, which is
coarse but is still the picture. PNG, JPEG, GIF, and WebP are read. A row says `[image]` in
place of the picture when the file is something else, or when the terminal answered with no
way to draw one, and says so with `nothing at that path now` when what the tool read was a
temporary file that has since been cleaned up.

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
