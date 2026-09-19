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

Then run `tria` for the thread list, or, in a project you want to work on:

```sh
tria open
```

which opens that project with a new thread ready to write, and adds the project to the
server first where there is none for it yet.

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
| `n` | new thread (pick a project; `^R` renames the one under the cursor) |
| `gw` | new thread: start it in a fresh worktree, or the project's checkout |
| `m` | change model |
| `i` `a` `I` `A` `o` `O` or `Enter` | write a message (Vim insert entry) |
| `za` | fold or unfold the tool group or row under the cursor; `za` on a row inside an open group opens what that call kept |
| `zr` `zm` | open or shut one level everywhere: the groups, then what every call in them kept |
| `zR` `zM` | open every level at once, or shut them all and let go of the folds opened by hand |
| `1`..`9` | answer a pending approval, with nothing written in the composer; otherwise a count for the next motion |
| `ga` | answer the agent's question: digits pick, `Space` toggles, `c` types a custom answer, `Enter` advances |
| `gy` | copy the last assistant message (OSC 52) |
| `gs` | toggle the thread between settled and active; in the thread list, the selected row |
| `gT` | list the agent's background tasks that are still running; `s` stops them, `S` the session, after asking |
| `gA` | list the subagents the thread has run; `Enter` reads one's transcript (`r` re-reads a running one), `y` yanks its report |
| `gS` | list the thread's terminals; attach to one, or close, restart, open a new one |
| `gW` | list the worktrees threads are holding; remove the ones that are done with |
| `gl` | open lazygit in a terminal popup for the thread; `[programs]` in the config file binds more keys |
| `g!` | a shell in the popup, in the thread's directory |
| `ge` | composer focused: edit the draft in your editor, read back on exit. Chat focused: view the message, plan, tool row, or tool group under the cursor |
| `gE` | view the whole conversation in your editor |
| `gx` | open the link on the cursor's line, else the picture under the cursor, else the thread's pull request |
| `gt` | switch to the tmux session named after the thread's directory, creating it if needed |
| `gP` | split a tmux pane beside tria, in the thread's directory (`:split`) |
| `gD` | show the thread's directory in the machine's file browser (`:reveal`) |
| `h` `l` `0` `$` `w` `b` | chat focused: move along the line under the cursor |
| `v` `V` | start a selection by character or by line, in the chat or in the composer; `y` copies it |
| `.` | composer focused: the last change again |
| `gJ` | composer focused: join lines, since `J` opens the next thread |
| `Ctrl-c` | interrupt the running turn, or stop the background work left running without one. Anywhere something is being typed or a list is open, it is `Esc` instead |
| mouse wheel | scroll the conversation, or the thread list when the pointer is over it |
| left click | in the thread list: open a thread, or fold and unfold a section; in the chat: open a link, or fold and unfold a tool group or row; in the composer: put the cursor there and write |
| drag | select text in the conversation; releasing copies it (OSC 52) |

With the chat focused, a cursor moves through the conversation: `j` `k` with counts,
`{` `}` to the previous or next message, `gg` and `G` (which also resumes following new
output), `Ctrl-d` `Ctrl-u` `Ctrl-f` `Ctrl-b` `Ctrl-e` `Ctrl-y` to scroll with the cursor.
`za`, `Enter`, or `Space` folds the tool group or row under the cursor. The cursor is a
block on one character: `h` `l` `0` `$` `w` `b` move along the line, and `j` `k` keep the
column.
`v` starts a selection by character and `V` by line; `y` copies it, and `yy` or `3y` copy
from the cursor without one. Clicking a chat line focuses the chat and puts the cursor on
the character clicked.

What a copy gives you is what was written rather than what was drawn: the marks and indents
the chat decorates its lines with are left out, and a message broken over several rows comes
back as the one line it was written as.

Clicking in the composer puts the cursor where you clicked and leaves you writing, without a
trip through normal mode. Past the end of a line is the end of it, and below the last line is
the last line, so a click anywhere in the box lands somewhere you can type.

`/` and `?` search the conversation forward or backward. The cursor previews the first match
while you type, `Enter` accepts, `Esc` goes back to where you were. `n` and `N` step to the
next and previous match, wrapping around with a notice. Matches are highlighted while the
chat has focus. Queries are case-insensitive unless they contain an uppercase letter. In the
chat, `?` searches; help is `?` from the composer or `:help`.

In the thread list, `j`/`k` move, `Enter` opens a thread or folds a section.

With the composer focused, normal mode edits it with Vim semantics. Motions: `h j k l w b e
W B E 0 ^ $ gg G f F t T ; ,`, all taking counts. Operators `d c y` combine with a motion or a
text object (`iw aw iW aW`, `i" a"`, `i( a( i[ a[ i{ a{ i< a<`), plus `dd cc yy D C Y x X`. `p P`
paste from the single register, `r` replaces a character, `~` toggles case, `u` and `Ctrl-r`
undo and redo. `v` and `V` select by character or by line — `o` swaps which end moves, the
operators take the selection, and `iw` and its kind select an object outright. `.` makes the
last change again, typing what was typed into it. Yanks also go to the system clipboard. The
status bar shows a partially typed command.

Eight letters are the app's rather than the editor's — `s S J K n m / ?` — and so are the
digits, which answer an approval when there is one and nothing is written. They are the
app's only while nothing is half typed at the composer: a count, an operator waiting for its
motion, or a selection being made gives every one of them back to Vim, so `3J` joins three
lines and `viws` substitutes a word. What they displace has another way in: `s` is `cl`, `S` is `cc`, and `J` is
`gJ`. The emulation stops there — no marks, no search within the composer, no macros, no
registers beyond the unnamed one, and no visual block.

`g` and `z` are keys that have not finished being pressed, and the status bar shows them
too, beside the rest of a half-typed command. They wait 1.2 seconds for the key that
completes them and then give it back, so a `g` pressed by mistake does not swallow the
next key. The config file says how long:

```toml
prefix_timeout_ms = 3000
```

Zero waits for as long as it takes, which is what Vim calls `notimeout`: `gA` is then `gA`
however long the pause in the middle, and the `g` on the status bar is what says one is
still waiting.

| Insert mode | |
| --- | --- |
| `Enter` | send |
| `Alt-Enter` or `Ctrl-j` | newline |
| `Ctrl-v` or `Ctrl-q` | the next key as the character it stands for: `Ctrl-v` `Enter` is a newline, `Ctrl-v` `Tab` a tab |
| `Up` `Down` or `Ctrl-p` `Ctrl-n` | prompt history |
| `←` `→` `Home` `End`, `Ctrl-a` `Ctrl-e` | move the cursor; `Alt` with an arrow, or `Alt-b` `Alt-f`, moves by word |
| `Backspace` `Delete`, `Ctrl-w` `Ctrl-k` `Ctrl-u` | delete a character, the word before the cursor, to the end, to the start |
| `Esc` or `Ctrl-c` | back to normal mode |

Quitting is `:q`, and only `:q`. No key does it on its own, because the composer holds
unsent messages that nothing writes to disk, and a chord that throws them away is not one
to find by accident. `Ctrl-v` and `Ctrl-q` mean in insert mode what they mean in Vim,
which is why neither is a quit: the next key goes in as the character it stands for.
`Alt-Enter` is the usual way to write a newline where `Enter` sends, but a terminal has to
be willing to send it; `Ctrl-v` `Enter` needs nothing of the terminal and always works.

The custom answer typed after `ga` then `c` is edited with those same keys, less the ones
that only mean something in a message: it is one line, so there is no newline to write. A
long answer scrolls inside its row rather than wrapping, `Enter` confirms it, and `Esc`
goes back to the options without keeping it.

So is every other line in the client: the `:` command line, a picker's query, and the `/`
search over the chat. All of them are the same one-line editor, so the cursor moves, a
word goes with `Ctrl-w`, and a long one scrolls inside its row. A picker keeps `Ctrl-k`
for itself — in a list, moving the cursor up a row is worth more than killing to the end
of a line that short — and `Ctrl-j`, `Ctrl-n`, `Ctrl-p` with it.

A paste arrives as one event and goes wherever typing would: the composer in either mode,
the command line, a picker's query, the chat search, the custom answer. The fields that
hold one line fold the paste onto one rather than keeping only what came before the first
newline.

Commands, entered after `:` in normal mode: `new [project]`, `model`, `effort [level]`,
`mode plan|default`, `perm <runtime mode>`, `rename [title]`, `project rename <name>`,
`archive`, `delete!`, `approve [n]`, `stop`, `stop!`,
`older`, `answer`, `dismiss`, `pr`, `git`, `shell`, `edit`, `view`, `tasks`, `terminals`, `tmux`,
`reveal`,
`worktree`, `worktrees`, `settle`, `unsettle`, `wake`, `settled`, `sidebar`, `agents`,
`usage`, `split`, `reconnect`, `help`, `q`.

The line is the same one-line editor as the rest of the client, so it takes the same keys:
`Ctrl-w` rubs out a word and `Ctrl-u` the line, `Ctrl-a` and `Ctrl-e` go to the ends, and
the arrows, `Alt-b` and `Alt-f` move by character and by word. `Backspace` on an empty line
still leaves. `Up` and `Down`, or `Ctrl-p` and `Ctrl-n`, walk back through the commands run
this session.

`Tab` completes the command's name, and the names of the programs `[programs]` binds. The
candidates are listed beside the line, each `Tab` takes the next and `Shift-Tab` the one
before, and tabbing past the last gives back what was typed rather than leaving a wrong
guess in the line. An argument that comes from a fixed list completes the same way —
`mode`, `perm`, `project`, and whichever efforts the model offers. The ones that take a
title or a name do not: there is nothing here to complete those against.

## Approvals

The agent stops for an approval whenever its permissions say it should, and the panel
that appears numbers the answers. `1`..`9` answer it — but only with nothing written in
the composer. One of those answers grants a permission for the rest of the session, and
an approval arrives when the agent reaches one rather than when you are ready for it, so
a count typed at a half-written message would otherwise answer it: `2w` is two words, not
"allow this for the session". With a draft in the composer the digits stay the
composer's and the panel says so; `:approve <n>` answers it whatever is written there.

## New threads

`tria open` starts tria on the project for the directory you are in, with a draft ready to
write: it is the way in from a checkout rather than from the thread list. `tria open <path>`
names another directory. A directory inside a project opens that project — `src/` of a
checkout is the checkout — and one the server has no project for adds it first, for the
repository the directory is in, named after it. The directory has to be one: a path that is
not there is said on the terminal before the screen is taken over. A project belongs to the
server, so adding one adds it for the desktop app too.

`n` picks a project and opens a draft; the message you write starts the thread. The header says
where it will run: `⌂ project checkout on <branch>`, or `⌂ new worktree off <branch>`, which
the server creates when the thread starts. `gw` or `:worktree` switches between them. The
default comes from the project's own setting, then the server's, and the server's default is
the current checkout.

A worktree branches off whatever the project's checkout has at the time, or off the remote's
copy of it where the server is set to start new worktrees from origin. Its branch is named
after the thread rather than the message, since the server names the worktree's directory
after the branch and a message is not a branch name.

Worktrees outlive the threads that used them, so `gW` or `:worktrees` lists the ones threads
are still holding: the thread, its project and branch, and whether the checkout has anything
uncommitted in it. `x` removes the selected one and `X` removes it anyway — git refuses a
worktree with modified or untracked files in it, which is the check worth having, and `X` is
how you say you meant it. A worktree the server made for a thread is the only kind offered:
one that a project is rooted in is a place to work rather than something left over.

`X` is the one key in that list that destroys work, and it sits on the shift of the one that
does not, so it asks first: a box naming the worktree and listing every uncommitted and
untracked file in it, re-read from the checkout as it opens rather than taken from the cache.
`Enter` removes it, `Esc` or `q` keeps it, and `j`/`k` move through the list when it is longer
than the box. A worktree with nothing uncommitted has nothing to lose and nothing for git to
refuse over, so `X` on one of those removes it without asking. The branch is named in the box
too, because it stays: what goes is the checkout.

Removing a worktree leaves its branch, so nothing committed is lost by clearing them out.
Threads that are done with a worktree still on the disk are marked `⌂` in the sidebar, and the
settled section says how many there are.

The model for a new thread is the last one you picked with `m`, remembered in the config file
between runs. Without one it falls back to the project's default model, then the server's.

`^R` in that list renames the project under the cursor: the name it has takes the search's
place, ready to be typed over, `Enter` renames it and `Esc` keeps the old one. `:project
rename <name>` does the same for the project the open thread belongs to. The name is the
server's, so it is the name everywhere — this sidebar, the desktop app, the next client to
connect. It is `^R` rather than `r` because the letters in that list go to the search.

The project list `n` opens draws each project with what it is known by, in the order the
desktop app uses it: the emoji it was given, the icon it was named from the Lucide set, the
icon its checkout carries, and failing all three a guess at what the project is, read from
its name. Everything but the emoji is a picture, so it needs a terminal that can draw one.

A named icon arrives as a name and a colour, since the picture itself is nobody's to send.
Lucide publishes the set as a font, so tria looks the name up there and draws the character
into pixels at the size of the room it has. A name from a set newer than the one tria was
built against is not drawn, and the project falls back to the icon its checkout carries.

The guess is the desktop app's own: the name is cut into words, each is looked up in a table
that knows what a backend, a docs site or a mobile app is called, and a name that says
nothing keeps one of five generic icons, picked by hashing it so that it stays the same icon.
Neither end sends this to the other — both work it out — so tria copies the rules rather than
inventing its own, and a project looks like itself in both.

The looking is the server's: the icon the project names, then the one its `t3.json` names in
`iconPath`, then the usual places a favicon lives, then whatever the project's `index.html`
links to. The bytes come over HTTP, so a server on another machine works the same as one
here. A repository that keeps its icon somewhere of its own — a monorepo, say — is worth
naming in `t3.json`, which the whole team then shares. SVG icons are not drawn.

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

## Programs

`g` and one more key run a program in a terminal of the thread's own, as a popup inside tria:
the chat stays on screen around it and tria keeps running. The program replaces the shell
rather than running inside it, so the popup goes straight to it and closes again when you
quit. `gl` is `lazygit` until you say otherwise, and what is on the other keys is the config
file's to decide:

```toml
[programs]
b = "yazi"
l = "gitui"
d = "lazydocker"
```

The key is the one pressed after `g`, and the value is the command, which should be
interactive: one that prints and exits takes the popup with it. A key set to `""` takes its
binding away, including `l`. Each binding also answers to the program's own name on the
command line, so `:yazi` is `gb`; `:git` stays the name for whatever is on `l`, as does the
older `git_command` setting, which `programs.l` overrides. The keys tria answers itself —
`g` `a` `e` `s` `t` `w` `x` `y` `A` `D` `E` `P` `S` `T` `W` `!` — are refused, and tria says which
when it starts rather than binding a key that would never arrive.

While a program is up, `Ctrl-\` leaves it running and its key comes back to it exactly as it
was. Each binding has a terminal of its own, so a file manager and a git client left open in
one thread do not take turns in the same shell.

The terminal belongs to the server, so this works the same whether the server is on this
machine or another one, and the program runs where the repository is.

Startup goes through your login shell, which is the whole of what a popup costs, and a thread
pays it once: closing the popup closes the terminal and opens another in its place, so the
shell is already at a prompt when you next ask for one. Leaving a thread lets go of the
one it was keeping. The pane also answers the capability queries a full-screen program sends
on startup, so nothing waits on a timeout.

A paste in the pane reaches the program as a paste. One that has asked for bracketed paste
gets the text between the markers, so a shell holds a multi-line paste at the prompt instead
of running every line but the last; one that has not gets it plain. Line endings arrive as
carriage returns, which is what Return sends, and the other control bytes are dropped, since
nothing in pasted text is meant as an escape sequence.

`g!` or `:shell` opens a plain shell in the same popup, in the thread's directory. Exiting it,
with `Ctrl-d` or `exit`, closes the popup. Both of these are scratch sessions: tria closes the
ones it opened and leaves the desktop app's own alone.

## Links

URLs in the conversation are underlined, and clicking one opens it in the browser. With the
chat focused, `gx` opens the link on the cursor's line, falling back to the picture under the
cursor — the one its tool row has, or the one its message shows — and then to the thread's
pull request.

Links are found on the drawn screen rather than in the message text, so one that wraps across
two lines is still whole, and the same goes for a URL inside a tool row or a plan. Trailing
sentence punctuation is left out of the link; brackets are kept when the URL opened them
itself.

## Background tasks

The agent runs monitors and background commands that outlive the turn that started them.
`gT` or `:tasks` lists the ones with no reported end, with how long each has been running,
and the status bar shows a count. Work the agent delegated to a subagent is not background
work and is listed by `gA` instead.

A watcher that has been going for hours started further back than the history tria
loads, so there is no row left for the list to show. The server keeps its own register of
what is still running in a thread, and the panel says what that register says when it has
nothing else to show: `the server says this thread is monitoring`.

There is no per-task stop in the protocol. What stops a watcher is the session's own
interrupt sent with no turn to name — `Ctrl-c`, `:stop`, or `s` in the list, which is
what the desktop app's `Monitoring · Stop` sends. `S` or `:stop!` is the harder one: it
ends the provider session, and every process it started goes with it. Either way the
conversation stays, and the next message starts a session again.

`S` asks before it does it. It is `s` with a finger on shift, it takes down work the list
is not showing, and nothing takes it back: `S` again or `y` agrees, and any other key —
`Esc` included — leaves the session running and the list where it was. `:stop!` carries
its answer in the `!`, as `:delete!` does, so it goes straight through.

## When the connection goes

A connection that drops is asked for again with a backoff, and the streams resume from
where they got to, so a server restarting under you costs a moment and nothing else.

A server that refuses tria is the other case: a token that has been revoked or has
expired is not going to be accepted by asking again every few seconds. That stops the
asking, and the line under the header says what the server said and what to do about
it: mint another credential, `tria pair <credential>` again, then `:reconnect`. Only the
server actually refusing this client counts as that — a 500 or a 503 is a server having
a bad minute and is asked again as anything else is.

`:reconnect` throws the connection away and makes another, whatever state it was in.

## When the updates stop

A thread's updates come over a stream of its own. If it ends, tria asks for it again
from where it got to, and a stream that comes back is a moment of trouble nobody needs
told about. One that will not come back is another matter: the conversation stops moving
and looks exactly like one nobody is writing to, which is the worst thing a chat window
can be quietly wrong about.

So it says so, on a line under the header, for as long as it lasts — `⚠ this thread has
stopped updating` — and keeps asking every ten seconds until the thread speaks again.
Opening the thread again, or the next reconnection, also starts a new stream.

## Notifications

A turn takes minutes, which is the whole reason not to sit and watch one. When a thread
stops working — finished, failed, waiting on an approval or a question, or holding a plan
— the terminal is asked to say so, and it turns that into whatever this desktop calls a
notification. Which thread it was does not come into it: the open one finishing while you
are in another window is the case this is for.

By default it only speaks up when the terminal does not have the focus, since a
notification for something already on the screen in front of you is an interruption to
tell you what you are looking at. The config file says otherwise:

```toml
notify = "always"   # or "unfocused", the default, or "never"
```

Two things this leans on the terminal for. The notification itself is OSC 9, which most
terminals understand and some do not. The focus is the terminal reporting it, which fewer
do — and inside tmux it arrives only with `focus-events on`, as the notification itself
arrives only with `allow-passthrough on`. A terminal that never mentions focus is treated
as one that does not have it, so notifications arrive rather than not: an interruption you
did not need is something you can see and turn off, and one that never came looks like a
feature that does not work. `notify = "never"` turns them off, and `"always"` is the
setting for a terminal that will not report focus but will notify.

## Usage limits

`:usage` shows what each signed-in account has left of its subscription: a bar per
rolling window — Claude's five-hour session, the weekly allowance — with how full it is
and when it comes back. The account the next message would be spent from is marked, which
is worth knowing on a server with more than one signed in.

The figures are the provider's own, taken by the server when it last probed, so the panel
says how old they are and `r` asks again. An account with no quota to report — an API key,
a cloud endpoint — is not listed; one whose quota could not be read says so.

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
started it, and stopping the session ends everything running in it.

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

A program in the pane can draw a picture: tria answers the kitty graphics protocol, so `chafa`,
`timg`, `viu` and anything else that asks the terminal whether pictures work is told that they
do, and what it sends is drawn over the pane. An image is placed where the cursor is and rides
the text as that scrolls, and it goes when the screen it was drawn over does — a `clear`, a
full-screen program taking over, or a resize. This needs a terminal underneath that draws
pictures too; where there is none, the pane draws in half-blocks like the chat does. A program
that decides by `$TERM` alone rather than asking will not try, because the pty is a plain
`xterm-256color` and saying otherwise would name a terminfo entry that may not be installed.
Animation and the unicode placeholder scheme are not answered, and a program asking for them is
told so rather than left waiting.

## Editor

`ge` with the composer focused writes the draft to a private temp file, opens it in your
editor, and reads it back when the editor exits. With the chat focused, `ge` opens the block
under the cursor read only: a message as markdown, or a tool call with its input, output
summary, and changed files. `gE` opens the whole loaded conversation. The editor is `editor`
from the config file, else `$VISUAL`, else `$EDITOR`, else `nvim`; Vim-like editors get `-R`
for read-only views. Like `gl`, this uses a tmux popup inside tmux and takes over the
terminal otherwise. `:edit` and `:view` are the command forms.

Unsent composer text stays with its thread for as long as tria is running: switch away and
back and the half-written message is still there. New-thread drafts have a slot of their
own. Nothing is written to disk, so quitting is quitting.

Sending empties the composer, which is what sending looks like. A message the server will
not take never went anywhere, so it comes back: into the composer if that is still where
you are and nothing has been written since, and into the thread's parked draft if you have
gone elsewhere. Where neither is free it is not lost either — every sent message goes into
the composer's history, and the toast says to reach for it with `Ctrl-p`. The same holds
for the message that would have started a new thread: the draft comes back with it, so the
project, the model and the worktree choice are as you left them.

A thread you come back to after a long gap says what it is still carrying, where the
composer's usual hint goes: `101k tokens from earlier · /compact resumes with less
context`. It appears for a thread whose last context count was at least 100k tokens and
was made over an hour ago, whose provider has a `compact` command to send, and which is
not running or waiting on an answer — the same rule the desktop app offers it by. Typing
takes it off the screen, and ignoring it costs nothing.

## The thread's directory

`gD` or `:reveal` hands the thread's working directory — its worktree when it has one, else
the project root — to whatever this machine browses files with: the Finder, the explorer,
the desktop's file manager, through the same platform opener a link goes to. The path is the
server's, so where the server runs on another machine tria says so instead of opening a
directory of that name here, which would be somebody else's.

## tmux

When tria runs inside tmux, `gt` or `:tmux` switches the client to the session named after
the thread's working directory: the worktree when the thread has one, else the project root.
The name is the directory's last path component, with `.` and `:` replaced by `_` because
tmux reserves them. If no such session exists, tria creates it in that directory first. This
suits a one-session-per-checkout layout.

`gP` or `:split` stays where you are instead: it splits tria's own pane and opens a shell in
the other half, in the same directory, in the session and window already on screen. tmux
puts the cursor in a pane it has just made, so the shell has the keys as soon as it is there.
The split is side by side, since tria is a tall window and a shell under it would have a
dozen rows. Nothing is said afterwards — the new pane is on screen with the cursor in it.

Both want the directory to be one this machine has. The path is the server's, so where the
server runs elsewhere tria says so rather than handing tmux a path that is somebody else's
directory here.

## Tool output

Tool calls fold in two levels. A run of them collapses to one line saying how many there are
and what the last one was; opening that gives a line per call; opening a call gives what the
server kept of it. `za` works on whatever is under the cursor, a group or a row. `zr` and `zm`
open and shut a level across the whole conversation, so `zr zr` is every call and everything
in it, which `zR` does in one. `zM` shuts them all, and is the one key that also lets go of
the folds opened by hand.

Expanding a tool row shows what the server sends: the command or tool input, the first line
of the output or a line count, changed files, and the status. The server projects tool
payloads to that summary before they go on the wire, so the full output is not available to
any client through the orchestration API. Rows without anything beyond their summary have no
fold marker. A subagent's tool calls are the exception: those come from a file rather than the
wire, and keep what they sent and what they got back.

An image the agent read is drawn rather than named: unfolding the row puts the picture under
the line, as wide as the chat and at most half as tall, scrolling with the conversation like
any other lines. It is fitted to that room rather than cropped, so a tall screenshot comes
out whole and small; `gx` on the row hands the picture to whatever this machine opens
pictures with, which is where to read one the terminal has shrunk past reading. That works
from a folded row too, and on a terminal that cannot draw pictures at all. A picture that
came inside a subagent's transcript is not a file anywhere, so it is written to a temporary
one first, named after the bytes so the same picture is always the same file. The summary the server sends holds the path the tool read and not the
picture, so the file is read from disk — which works when the server is on this machine, and
is why an image row against a remote server has nothing to unfold. A subagent's transcript is
the exception again: the provider's file carries its pictures inside it, and those draw
wherever the server runs.

A picture the agent points at in what it writes is drawn as well. An agent that takes a
screenshot writes the file out and then shows it, which in markdown is an image: what it
called the picture is kept as the caption and the picture goes under it, in the same room and
by the same rules as a tool row's. There is nothing to unfold — a message's picture is part of the message
— and `gx` on the caption or on the picture itself opens the file. The path is read from the
disk under us, as a tool row's is, so the caption says whose disk it was when that is somewhere
else, and says `nothing at that path now` when the file has been cleaned up since. A source
that is not a path in full is left as the marker the markdown reader wrote: a relative one has
no directory here to be relative to, and the chat does not fetch over the web.

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

A thread that has finished a turn, been written to, or settled into monitoring since you last
had it open shows `●` in place of the quiet `·`; opening it clears the mark. Monitoring keeps
its `◔` and turns green instead, because a watcher sits there for hours and the glyph alone
cannot say whether anything has happened in them. The other statuses are left alone, since
each already says more than "look at me". This is only for as long as tria is running:
what you have read is not written down, so a thread is never unread because of something that
happened before tria started.

The list draws a thread on one line. The config file can give it two instead, which buys room
for the title by putting what the thread is working in under it, and draws the project as an
icon spanning both lines rather than a label competing with the title for the same row:

```toml
sidebar_layout = "two-line"
```

The second line is the thread's branch; a thread working in the project's own checkout has no
branch of its own recorded, so it says the project's name instead, and the open thread falls
back to the ref its checkout is on. `⌂` in front of it means a settled thread still holds a
worktree. The other value is `one-line`, which is the default.

## Other subcommands

- `tria open [path]` opens the project for a directory, adding it when there is none. It is
  the chat, not a subcommand that prints and exits.
- `tria probe` connects, prints the server config keys and the thread list, and exits.
- `tria dump <thread-id>` opens a thread through the same connection code as the UI and prints
  reduced state for a few seconds. Both are useful when checking a new server release.

Logs go to `tria.log` next to the config file. Set `TRIA_LOG=debug` for more detail.

## How it talks to the server

See [PLAN.md](./PLAN.md) for the protocol notes: pairing and WebSocket tickets, the Effect RPC
JSON framing, the shell and thread subscriptions, and the dispatch commands this client uses.
