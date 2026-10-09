# tria — a terminal chat client for T3 Code

Rust + ratatui client that talks to a running T3 Code server. Scope is deliberately narrow:
a thread picker, a chat view, a composer, and Vim-style navigation, plus the thread's terminals
because the server already runs them. No diff viewer, browser, file explorer, or settings UI.

This document records the protocol findings the design rests on and the decisions taken.
Upstream references are paths inside the [t3code repository](https://github.com/pingdotgg/t3code)
at the commit surveyed (Effect `4.0.0-rc.112`, `t3` npm `0.0.40`). The orchestration sections
describe protocol 2, surveyed again at `t3` `0.0.46`.

## 1. How the server talks

T3 Code is an "environment": one Node server that owns providers, projects, and an
event-sourced SQLite store. Every client, including the official web, desktop, and mobile
apps, speaks the same protocol. There is no third-party client documentation and no
compatibility promise, but the schemas are built for forward compatibility (optional fields
with defaults, open string unions, unknown array members dropped rather than failing).

### Discovery

A running server writes `server-runtime.json` under the T3 home userdata directory
(`~/.t3/userdata` by default): `{ version, pid, host, port, origin, startedAt }`. The client
reads `origin` from there, with a `--url` override for remote servers (Tailscale, LAN). The
unauthenticated descriptor at `GET /.well-known/t3/environment` returns the environment id and
a `capabilities` map used for feature negotiation.

### Authentication

There is no unauthenticated loopback mode. Sessions are scoped; chat needs
`orchestration:read` and `orchestration:operate`.

Bootstrap once:

1. The user mints a pairing credential on the host with `t3 pair` (or
   `t3 auth pairing create --json`). The desktop app also exposes this in Connections settings.
2. The client exchanges it at `POST /oauth/token` (form-urlencoded):
   `grant_type=urn:ietf:params:oauth:grant-type:token-exchange`,
   `subject_token=<credential>`,
   `subject_token_type=urn:t3:params:oauth:token-type:environment-bootstrap`,
   `requested_token_type=urn:ietf:params:oauth:token-type:access_token`,
   optional `client_label`, `client_device_type`.
   Response: `{ access_token, token_type: "Bearer", expires_in, scope }`.
3. The client stores the bearer token.

Shortcut for same-machine use: `t3 auth session issue --token-only` prints a long-lived
bearer token directly.

Per connection: `POST /api/auth/websocket-ticket` with `Authorization: Bearer <token>` returns a
short-lived `{ ticket, expiresAt }`. Open
`ws(s)://<origin>/ws?orchestrationProtocol=2&wsTicket=<ticket>`. The server answers a socket
that names no protocol, or another one, with HTTP 426 (`orchestration_protocol_incompatible`)
before it reads the ticket; HTTP requests to `/api/orchestration` name it in the
`x-t3-orchestration-protocol` header. Optional
query params `clientSurface`, `clientAppVersion`, `clientDeviceType`, `clientOs`,
`connectionMethod=direct` only feed the server's Connections list and analytics.

Sources: `packages/contracts/src/auth.ts`, `packages/contracts/src/environmentHttp.ts`,
`apps/server/src/auth/EnvironmentAuth.ts`, `docs/internals/environment-auth.md`.

### Wire framing: Effect RPC over WebSocket, JSON

The server runs `RpcServer` with `RpcSerialization.layerJson`: one JSON object per WebSocket
text frame. The client implements this by hand; it is small.

Client to server:

```json
{"_tag":"Request","id":1,"tag":"server.getConfig","payload":{},"headers":[]}
{"_tag":"Ack","requestId":2}
{"_tag":"Interrupt","requestId":2}
{"_tag":"Ping"}
```

Server to client:

```json
{"_tag":"Chunk","requestId":2,"values":[ ... ]}
{"_tag":"Exit","requestId":1,"exit":{"_tag":"Success","value":{ ... }}}
{"_tag":"Exit","requestId":1,"exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{"_tag":"EnvironmentAuthorizationError", ...}}]}}
{"_tag":"Defect","defect": ...}
{"_tag":"Pong"}
```

Rules that are easy to get wrong:

- **Ack every Chunk.** The WebSocket protocol has `supportsAck: true`, so the server blocks the
  next chunk of a stream until the client acks the previous one. Ack immediately on receipt,
  off the render path. A slow subscriber is failed by the server after roughly 1000 buffered
  items or 8 MiB, so drain fast and treat that failure as "resubscribe".
- Streams end with an `Exit` whose success value is void. Unary calls return a single `Exit`.
- Send `Ping` about every 5 seconds; treat 20 seconds without a `Pong` as a dead socket.
- Request ids are client-chosen integers. Payloads are Effect Schema encoded JSON: dates are
  ISO-8601 strings, ids are plain non-empty strings (client-generated UUIDv4 is fine).
- Errors are tagged structs; a `Fail` with `_tag` names the contract error class.

Sources: `effect/unstable/rpc/RpcMessage.d.ts`, `RpcServer.js`, `RpcClient.js`;
`apps/server/src/ws.ts` (route `/ws`); `packages/client-runtime/src/rpc/session.ts`.

### The RPCs a chat client needs

| Purpose | Method | Kind |
| --- | --- | --- |
| Provider and model catalog, capabilities, slash commands | `server.getConfig` (or stream `subscribeServerConfig`) | unary |
| Project and thread list, live | `orchestration.subscribeShell` | stream |
| One thread's projection, live | `orchestration.subscribeThread` | stream |
| Thread mutations | `orchestration.dispatchCommand` | unary |
| A new thread with its first message | `orchestration.launchThread` | unary |
| Project mutations | `projects.mutate` | unary |

Also: `orchestration.searchThreads`, `orchestration.getTurnItem` (a tool item's withheld output),
and over HTTP `GET /api/orchestration/threads/:threadId/history?cursor=` for older history.

Sources: `packages/contracts/src/rpc.ts`, `packages/contracts/src/orchestrationV2.ts`,
`packages/contracts/src/environmentHttp.ts`.

### Subscriptions

Both streams deliver a snapshot, then deltas, each carrying a monotonic global `sequence`.

`subscribeShell { afterSequence?, requestCompletionMarker? }` yields items with `kind` one of
`snapshot`, `project.updated`, `project.removed`, `thread.updated`, `thread.removed`,
`synchronized`. The snapshot holds projects, `threads`, and `archivedThreads`, with no message
bodies. A second `snapshot` carrying `resolvedRepositoryIdentityRoots` only refreshes the
projects' repository identities: it has no threads and replaces nothing.

`subscribeThread { threadId, afterSequence?, requestCompletionMarker?, acceptBoundedSnapshot? }`
yields `snapshot`, `event`, `synchronized`. The snapshot is the thread's projection: the app
thread, `runs`, `attempts`, `providerSessions`, `runtimeRequests`, `plans`, `checkpoints`,
`turnItems`, and `visibleTurnItems`, the timeline rows in order. With `acceptBoundedSnapshot`
the rows are the newest window, with `historyCursor`, `hasMoreHistory`, and
`latestLocalTurnOrdinal`; the rest of the projection is whole. Events are upserts of one entity
each: `run.updated`, `turn-item.updated`, `runtime-request.updated`, `plan.updated`,
`checkpoint.captured`, `provider-session.attached|detached`, `thread.*` with the app thread,
and more that a chat client can skip. `packages/client-runtime/src/state/orchestrationV2Projection.ts`
is the reference reducer, and `packages/shared/src/orchestrationV2Timeline.ts` says which items
are visible: none of a rolled-back run, and no queued message whose run was cancelled.

Resume after a disconnect by resubscribing with `afterSequence` set to the last applied
sequence. Dedupe by `sequence`. Only send `acceptBoundedSnapshot` when
`ServerConfig.threadSnapshotPagination` is advertised.

### Data model, as far as chat cares

- **Project**: `{ id, title, workspaceRoot, defaultModelSelection, ... }`.
- **Thread shell**: `{ id, projectId, title, modelSelection, runtimeMode, interactionMode,
  branch, worktreePath, status, latestRunId, latestRun{Requested,Started,Completed}At,
  pendingRuntimeRequest, pendingBackgroundTasks, lastError, pinnedAt, pinOrderKey, archivedAt,
  settledAt, settledOverride, unsettledAt, snoozedUntil, hasActionableProposedPlan,
  updatedAt, ... }`. `status` is the latest run's, or `idle` for a thread with none.
- **Pull requests**: `pullRequests[]` on the shell, each `{ host, repository, number, url,
  source, linkedAt, snapshot?: { state: open|closed|merged, title, headBranch, baseBranch,
  isDraft, checksState, reviewDecision, mergeability, ... } }`, plus `branchPullRequest`
  (`{ repository, number, url }`) for the one whose head is the thread's branch. Changes
  arrive as `thread.updated` shell items; tria relies on those.
- **Sidebar sections** (as the desktop app derives them): *settled* when `settledAt` is set
  (the server parks a thread after its turn finishes; `thread.settle` and `thread.unsettle`
  move it by hand), *snoozed* when `snoozedUntil` is in the future, *pinned* when `pinnedAt`
  is set, otherwise *active*. Active threads sort by `unsettledAt` when it is newer than
  `createdAt`, settled ones by `settledAt`, snoozed ones by `snoozedUntil` ascending.
- **Sidebar status**, first match wins: a pending runtime request that is an approval, one
  that is a question (`user_input`), a run going (working), a failed run, background tasks
  outliving the run (working, or monitoring when all are monitors), plan mode with an
  actionable proposed plan (plan ready), then the latest run's state. A rolled-back run is no
  turn at all.
- **Run**: what tria calls a turn. `{ id, ordinal, status: preparing|queued|starting|running|
  waiting|completed|interrupted|failed|cancelled|rolled_back, requestedAt, startedAt,
  completedAt }`.
- **Turn item**: one timeline entry, by `type`: `user_message`, `assistant_message` (text is
  the whole message each time, with `streaming`), `reasoning`, `proposed_plan`, `todo_list`,
  `command_execution`, `file_change`, `file_search`, `web_search`, `dynamic_tool`,
  `approval_request`, `user_input_request`, `subagent`, `error`, `notification`,
  `system_notice`, `compaction`, `handoff`, `checkpoint`, and more. Each carries `id`, `runId`,
  `ordinal`, `status`, `title`, and its times. Unknown types are skipped.
- **Tool output is withheld on the wire.** A tool item carries `outputOmitted: true` in place
  of its output; `orchestration.getTurnItem` returns the item whole.
- **Background tasks**: `pendingBackgroundTasks` on the thread shell, each `{ taskId,
  description?, kind: command|monitor|subagent|background_task }`. The server keeps the list
  and drops a task when it ends or its session goes.
- **Approval and question**: an `approval_request` or `user_input_request` item names a
  runtime request, which is `pending` until answered. Approvals carry `requestKind`, `prompt`,
  and `options: [{ decision, label, warning? }]`. Both are answered with
  `runtime-request.respond`.
- **Checkpoint**: `{ id, scopeId, runId, parentCheckpointId, appRunOrdinal, status }`, one per
  run, after a baseline with no run. Rolling back to a run's parent checkpoint undoes the run.
  A rollback the server gives up on is set on the app thread as `rollbackFailure`.

### How tria reads it

`src/v2.rs` holds the open thread's projection, applies the stream's events to it, and
derives from it the model the timeline and panes were written against: messages, activities,
proposed plans, and checkpoints keyed by turn. A run's id is its turn id, tool items become
`tool.*` activities, approvals and questions become `approval.*` and `user-input.*`, subagents
become `task.*` with `agentKind: "agent"`, and a todo list becomes `turn.plan.updated`. Items
are stamped no earlier than the one before them, since the timeline orders by time and the
server orders by ordinal.

### Commands (via `orchestration.dispatchCommand`)

Every command carries `type`, a client-minted `commandId`, and `threadId`. The client needs:

| Action | `type` | Notes |
| --- | --- | --- |
| Send a message | `message.dispatch` | `messageId`, `text`, `attachments: []`, `createdBy: "user"`, `creationSource`, optional `modelSelection`, `dispatchMode: { type: "start_immediately" }`, `deliveryIntent: "auto"`: with `serverResolvedCommandContext` advertised the server decides whether to start, steer, or queue. Modes are the thread's, set first where they change |
| Stop | `run.interrupt` | `runId`: the run going, or the last one, whose background work it is; `holdQueue: true` |
| Stop the session | `provider-session.detach` | one per provider session in the projection |
| Approve, deny, or answer | `runtime-request.respond` | `requestId`, and `decision` one of `accept`, `acceptForSession`, `acceptAlways`, `decline`, `cancel`, or `answers` |
| Rewind | `checkpoint.rollback` | `scopeId`, `checkpointId`, `restoreFiles` |
| Rename | `thread.metadata.update` | `title` or `regenerateTitle: true` |
| Change model | `thread.model-selection.set` | `modelSelection` |
| Archive / unarchive / delete | `thread.archive`, `thread.unarchive`, `thread.delete` | |
| Permission mode | `thread.runtime-mode.set` | `approval-required`, `auto-accept-edits`, `auto`, `full-access` |
| Plan mode | `thread.interaction-mode.set` | `default` or `plan` |

`projects.mutate` takes `project.create`, `project.update`, and `project.delete`.

`ModelSelection` is `{ instanceId, model, options?: [{ id, value }] }`. `instanceId` is the
configured provider instance from `ServerConfig.providers[]`; `model` is a slug from that
instance's `models[]`. Reasoning effort is not a field; it is an option whose descriptor comes
from `models[].capabilities.optionDescriptors` (for Claude, `id: "effort"` with values such as
`low`, `medium`, `high`). Omitting `options` uses the provider default.

Slash commands need no encoding: a provider command works when it is the first token of the
text. File mentions can be plain `@path` text. Attachments and the `t3-context://` chip format
are out of scope.

### Starting a thread

`orchestration.launchThread` makes a thread, prepares where it works, and sends its first
message: `{ commandId, threadId, projectId, title, generateTitle, modelSelection, runtimeMode,
interactionMode, workspaceStrategy, initialMessage: { messageId, text, attachments } }`. The
workspace strategy is `{ type: "root", branch? }`, `{ type: "existing_worktree", worktreePath,
branch? }`, or `{ type: "worktree", baseRef, branch?, startFromOrigin? }`, for which the server
creates the worktree and sets the thread's branch and path. It carries no plan reference, so a
plan built in a thread of its own is not marked as built.

Where a new thread runs by default is `defaultThreadEnvMode`, `worktree` or `local`: on the
project when set, otherwise the server setting, whose own default is `local`.

### Checkout state

`subscribeVcsStatus` takes a `cwd` and streams `{ _tag: "snapshot", local, remote }` followed
by `localUpdated` and `remoteUpdated`. `local` carries `isRepo`, `refName` (null on a detached
head), `isDefaultRef`, `hasWorkingTreeChanges` and a per-file working tree summary; `remote`
carries `hasUpstream`, ahead and behind counts, and a pull request.

This is where a branch comes from. `ThreadShell.branch` is set only for threads the server
created a branch for, and is null for the rest, including every thread working in a project's
own checkout.

The status behind that stream is cached and does not follow every edit on disk: a file changed
outside the server can leave `hasWorkingTreeChanges` and the line counts stale indefinitely.
`vcs.refreshStatus` takes the same `cwd`, re-reads, and broadcasts the result to the
subscribers, so a client that wants current numbers has to ask.

### Terminals

Terminals are a subsystem of their own, outside `orchestration.dispatchCommand` and outside the
`orchestration:read`/`operate` scopes. The list needs `terminal:read`, and driving one
`terminal:operate`.

`subscribeTerminalMetadata` streams `{ type: "snapshot", terminals }`, `{ type: "upsert",
terminal }`, and `{ type: "remove", threadId, terminalId }`. A terminal carries `threadId`,
`terminalId`, `cwd`, `worktreePath`, `status` (`starting`, `running`, `exited`, `error`), `pid`,
`exitCode`, `exitSignal`, `hasRunningSubprocess`, `label`, and `updatedAt`. No contents: that is
`subscribeTerminalEvents`, which streams output chunks and needs a VT parser to render.

The calls are `terminal.open` (`threadId`, `terminalId`, `cwd`, `cols`, `rows`; the client mints
the id and gets back the summary plus a `history` string), `terminal.write` (`data`, at most
64 KiB), `terminal.resize`, `terminal.clear`, `terminal.restart` (same shape as open), and
`terminal.close`.

A client that draws a terminal has to answer the capability queries programs send on startup:
device attributes, XTVERSION, DECRQM mode queries, the kitty keyboard flags, the text area
size. A program that gets no answer waits out its own timeout, which is around a second, before
it draws anything. Answers go back through `terminal.write` like keystrokes.

`terminal.attach` is a stream, not a call: it takes the same fields as open plus
`restartIfNotRunning`, and yields `{ type: "snapshot", snapshot }` with the scrollback first,
then the same event union. Feed `history` and every `output` `data` to a VT parser and the
screen is reconstructed exactly, which is why re-attaching after a client restart shows the
session as it was.

### Stopping work

There is no per-task stop. What exists is `run.interrupt`, which stops a run and, sent for a
settled run, the background work it left going; `provider-session.detach`, which ends a
provider session and every process it started; `terminal.close` and `terminal.restart` for
shells; and `server.signalProcess` (`SIGINT` or `SIGKILL`) for host processes listed by
`server.getProcessDiagnostics`. The last one refuses the server's own pid, requires a
`startTimeMs` identity match so a recycled pid cannot be hit, and only signals processes it
classifies as `server-child`, `provider-root`, or `terminal-root`.

## 2. Decisions

**Transport.** Hand-written Effect RPC framing over `tokio-tungstenite`, JSON via `serde`.
About 200 lines. HTTP via `reqwest` for token exchange and tickets. No attempt to reuse
Effect's client; no codegen from the TypeScript schemas.

**Types.** Hand-written `serde` structs for the roughly fifteen schemas above. Every string
union gets an `Other(String)` variant, every struct uses `#[serde(default)]` for optional
fields, and activity payloads stay `serde_json::Value` until a renderer needs a field. Unknown
event or activity kinds are kept and ignored, never fatal.

**State.** A single `App` struct with a pure reducer per stream: apply shell items to a project
and thread map, and apply thread events to the open thread's projection, mirroring
`packages/client-runtime/src/state/orchestrationV2Projection.ts`. Track `lastSequence` per subscription for resume.
Only one thread is subscribed at a time; switching threads interrupts the old subscription and
starts a new one. Keep a small cache of recently viewed threads with their cursor so switching
back is instant.

**Concurrency.** `tokio`. One task owns the socket and translates frames into an `AppEvent`
channel; it acks chunks itself before forwarding. The UI loop selects over key events, socket
events, and a tick, mutates `App`, and redraws. Dispatches go out through a request map keyed
by request id with a `oneshot` for the `Exit`.

**Reconnect.** Exponential backoff capped at 30 seconds. On reconnect: new ticket, new socket,
resubscribe shell and open thread with `afterSequence`. A stream failure with
`OrchestrationGetSnapshotError` is handled the same way without dropping the socket.

**Rendering.** `ratatui` with `crossterm`. Markdown through `tui-markdown` first; swap to a
custom `pulldown-cmark` renderer with `syntect` code highlighting only if `tui-markdown` proves
limiting. The composer is a small in-tree editor: `tui-textarea` pins ratatui 0.29 and does not
link against 0.30. Layout is a left thread list (toggleable) and a right chat column with a fixed
composer at the bottom and a one-line status bar (connection, provider/model, runtime mode,
session status). Each turn renders as: user bubble, a collapsed one-line-per-tool activity
group ("Read foo.rs", "Ran cargo test ... ok"), the assistant markdown, and an optional plan
card. Approvals render as an inline panel above the composer with the option labels mapped to
digit keys. Streaming appends deltas and re-renders at most every 16 ms.

**Vim model.** Two modes plus a command line.

- *Normal* (default): one mode, two foci. With the composer focused, app keys come first
  (`Ctrl-d`/`Ctrl-u`/`Ctrl-e`/`Ctrl-y` scroll the chat, `J`/`K` threads, `Tab` sidebar, `/`
  picker, `z` folds, `g` prefix for `ga` answer, `gy` yank, `gx`, `gt`), and every other key
  is a Vim command on the composer text: motions with counts, `d c y` operators with motions
  and text objects, `f F t T ; ,`, `p P`, `r`, `~`, `u`/`Ctrl-r`, and the insert entries
  `i a I A o O`. Digits answer a pending approval when there is one, else they are counts.
  `Ctrl-c` interrupts the running turn. The mouse wheel scrolls whichever pane it is over and
  a left click in the thread list opens a thread or folds a section, or focuses the chat
  and places its line cursor.
- *Chat focus*: a line cursor over the wrapped content lines (`j k`, `{ }` between message
  blocks, `gg G`, Ctrl scrolling carries the cursor), fold under the cursor, linewise `V`
  plus `y`, and `/` `?` `n` `N` search over the displayed lines with smartcase and
  incremental preview. Yanked and searched text is read back from an off-screen render of
  the covering blocks, so it matches what is displayed. Charwise motions would need tria to
  own the wrapping. Dragging over the
  conversation selects screen cells in reading order and copies them through OSC 52 on
  release, so wrapped lines copy exactly as displayed.
- *Insert*: the composer. `Enter` sends, `Alt-Enter` or `Ctrl-j` inserts a newline, `Esc`
  returns to normal, `Up`/`Down` or `Ctrl-p`/`Ctrl-n` recall prompt history.
- *Command line* (`:`): `:new [project]`, `:model`, `:mode plan|default`,
  `:perm full-access|auto|approval-required`, `:rename <title>`, `:archive`, `:delete`, `:q`.

Model and project pickers open as a centered fuzzy list, reused for `/` thread filtering.

**Configuration.** `$XDG_CONFIG_HOME/tria/config.toml` (default `~/.config/tria/config.toml`)
with `url` and a bearer token, file mode 0600. `tria pair <credential>` accepts a raw pairing
credential or a `/pair#token=` URL and performs the exchange. Keychain storage is a later
option, not v1.

**Explicitly out of scope for v1.** Attachments and image paste, composer context chips,
attachments on question answers, archived thread browsing, thread search, multiple environments, worktree or branch
selection on thread creation, diffs, terminals, PR views, settings.

## 3. Milestones

Status: M0 through M2 are implemented and verified against a live server (pairing, streaming,
approvals derivation, interrupt, new thread with bootstrap, rename, archive, answering agent
questions including multi-select and custom text). M3 items shipped so far: model and effort
pickers, `:` commands, prompt history, yank, config file, `gt` tmux session switching, sectioned sidebar (pinned, active,
snoozed, folded gray settled shelf) with per-thread status labels and settle/unsettle/wake
commands, branch and pull request in the header with `gx` / `:pr` to open it. Not yet done: a recent-thread cache for instant back navigation, packaging.

**M0, spike (proves auth and framing).** CLI binary, no UI. Read `server-runtime.json`,
exchange a pairing credential, fetch a ticket, open the socket, call `server.getConfig`, run
`subscribeShell` until `synchronized`, print projects and threads, exit. Everything after this
is UI work.

**M1, read-only viewer.** Shell and thread subscriptions with reducers. Thread list, chat
view with markdown and collapsed tool groups, normal-mode navigation, live updates while
another client drives the agent, reconnect with resume.

**M2, chat.** Composer, `thread.turn.start` including `bootstrap.createThread`, streaming
deltas, interrupt, approval panel, session and turn state in the status bar.

**M3, polish.** Model picker with option descriptors, `:` commands (rename, archive, modes),
prompt history, yank, config file and first-run pairing flow, packaging.

## 4. Risks

- **Protocol is internal.** Effect 4 is a release candidate and T3 Code makes no client
  compatibility promise. Pin the surveyed shapes in fixtures and re-run M0 against new server
  releases. Mitigation is the lenient decoding rule above.
- **Backpressure.** The server fails slow subscribers. Acking in the socket task, not the UI
  task, is what keeps a long-running turn alive.
- **Large threads.** A full thread snapshot can be large. Accept a bounded snapshot when the
  server advertises pagination, page older history over HTTP, and render lazily from the bottom.
- **Ratatui markdown quality.** Tables, nested lists, and code fences may need the custom
  renderer sooner than hoped.

## 5. Crates

| Crate | Version at survey | Use |
| --- | --- | --- |
| `ratatui` | 0.30 | UI |
| `crossterm` | 0.29 | terminal backend, key events |
| `tui-markdown` | 0.3 | markdown to `Text` |
| `tokio`, `tokio-tungstenite` | 0.30 | async runtime, WebSocket |
| `reqwest` | current | token exchange, tickets |
| `serde`, `serde_json` | current | wire types |
| `uuid` | current | ids |
| `time` or `chrono` | current | ISO-8601 |
| `pulldown-cmark`, `syntect` | 0.13, 5.3 | only if the custom renderer becomes necessary |
