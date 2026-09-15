# tria — a terminal chat client for T3 Code

Rust + ratatui client that talks to a running T3 Code server. Scope is deliberately narrow:
a thread picker, a chat view, a composer, and Vim-style navigation. No diff viewer, terminal
pane, browser, file explorer, or settings UI.

This document records the protocol findings the design rests on and the decisions taken.
Upstream references are paths inside the [t3code repository](https://github.com/pingdotgg/t3code)
at the commit surveyed (Effect `4.0.0-rc.112`, `t3` npm `0.0.40`).

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
short-lived `{ ticket, expiresAt }`. Open `ws(s)://<origin>/ws?wsTicket=<ticket>`. Optional
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

### The four RPCs a chat client needs

| Purpose | Method | Kind |
| --- | --- | --- |
| Provider and model catalog, capabilities, slash commands | `server.getConfig` (or stream `subscribeServerConfig`) | unary |
| Project and thread list, live | `orchestration.subscribeShell` | stream |
| One thread's messages and activities, live | `orchestration.subscribeThread` | stream |
| Every mutation | `orchestration.dispatchCommand` | unary |

Optional later: `orchestration.getArchivedShellSnapshot`, `orchestration.searchThreads`.
The same reads and dispatch exist over plain HTTP (`GET /api/orchestration/shell`,
`GET /api/orchestration/threads/:threadId`, `POST /api/orchestration/dispatch`), which is
useful for a smoke test before the WebSocket layer exists.

Sources: `packages/contracts/src/rpc.ts`, `packages/contracts/src/orchestration.ts`.

### Subscriptions

Both streams deliver a snapshot, then deltas, each carrying a monotonic global `sequence`.

`subscribeShell { afterSequence?, requestCompletionMarker? }` yields items with `kind` one of
`snapshot`, `project-upserted`, `project-removed`, `thread-upserted`, `thread-removed`,
`synchronized`. The shell snapshot holds projects and thread shells (metadata, session status,
latest turn state, pending approval flags, plan progress) with no message bodies. Archived
threads are excluded.

`subscribeThread { threadId, afterSequence?, requestCompletionMarker?, turnLimit? }` yields
`snapshot`, `event`, `synchronized`. Only six event types appear: `thread.message-sent`,
`thread.activity-appended`, `thread.session-set`, `thread.proposed-plan-upserted`,
`thread.turn-diff-completed`, `thread.reverted`. Thread metadata changes (title, model,
archive) arrive on the shell stream, not here.

Resume after a disconnect by resubscribing with `afterSequence` set to the last applied
sequence. The server replays the gap or, if the gap is too large, sends a fresh snapshot.
Dedupe by `sequence`. Only send `turnLimit` when `ServerConfig.threadSnapshotPagination` is
advertised.

### Data model, as far as chat cares

- **Project**: `{ id, title, workspaceRoot, defaultModelSelection, ... }`.
- **Thread shell**: `{ id, projectId, title, modelSelection, runtimeMode, interactionMode,
  branch, session, latestTurn, hasPendingApprovals, hasPendingUserInput, planProgress,
  pinnedAt, pinOrderKey, archivedAt, settledAt, settledOverride, unsettledAt, snoozedUntil,
  backgroundLiveness, hasActionableProposedPlan, updatedAt, ... }`.
- **Sidebar sections** (as the desktop app derives them): *settled* when `settledAt` is set
  (the server parks a thread after its turn finishes; `thread.settle` and `thread.unsettle`
  move it by hand), *snoozed* when `snoozedUntil` is in the future, *pinned* when `pinnedAt`
  is set, otherwise *active*. Active threads sort by `unsettledAt` when it is newer than
  `createdAt`, settled ones by `settledAt`, snoozed ones by `snoozedUntil` ascending.
- **Sidebar status**, first match wins: pending approval, pending user input, turn running or
  session starting (working), session error (failed), `backgroundLiveness` working or
  monitoring (native background work outliving the turn), plan mode with an actionable
  proposed plan (plan ready), then the last turn's state.
- **Thread detail**: shell fields plus `messages[]`, `activities[]`, `proposedPlans[]`,
  `session`, and `checkpoints[]` (ignore).
- **Message**: `{ id, role: user|assistant|system, text, attachments?, turnId, streaming,
  createdAt, updatedAt }`. Text is flat markdown. There is no parts array.
- **Streaming assistant text**: `thread.message-sent` with `streaming: true` carries a
  **delta**; append it to the message with that id. A final event with `streaming: false`
  carries the full text and replaces it (empty text means keep what you have).
- **Activity**: `{ id, tone: info|tool|approval|error, kind, summary, payload, turnId,
  sequence?, createdAt }`. `kind` is an open string. Kinds worth rendering:
  `tool.started|updated|completed|denied` (payload has `toolCallId`, `itemType`, `status`,
  `title`, `detail`), `approval.requested|resolved`, `user-input.requested|resolved`,
  `turn.plan.updated`, `runtime.error|warning|note`, `context-compaction`. Ignore
  `task.*`, `context-window.updated`, `checkpoint.*`, `worktree-setup`, `setup-script.*`.
- **Reasoning is not on the wire.** Only assistant text becomes messages; reasoning items are
  filtered server-side. Nothing to render.
- **Session**: `{ status: idle|starting|running|ready|interrupted|stopped|error,
  activeTurnId, lastError, runtimeMode, ... }` via `thread.session-set`.
- **Approval**: `approval.requested` payload `{ requestId, requestKind, requestType, detail,
  options: [{ decision, label, warning? }] }`. Answer with `thread.approval.respond`.
- **Proposed plan**: `{ id, turnId, planMarkdown, implementedAt }`.

### Commands (all via `orchestration.dispatchCommand`)

Every command carries `type`, a client-minted `commandId`, `threadId`, and usually
`createdAt`. The client needs:

| Action | `type` | Notes |
| --- | --- | --- |
| Send a message / start a turn | `thread.turn.start` | `message: { messageId, role: "user", text, attachments: [] }`, `runtimeMode`, `interactionMode`, optional `modelSelection`; `bootstrap.createThread` creates the thread in the same dispatch |
| Stop | `thread.turn.interrupt` | optional `turnId` |
| Approve or deny | `thread.approval.respond` | `requestId`, `decision` one of `accept`, `acceptForSession`, `acceptAlways`, `decline`, `cancel` |
| New thread without a first message | `thread.create` | `projectId`, `title`, `modelSelection`, `runtimeMode`, `branch: null`, `worktreePath: null` |
| Rename | `thread.meta.update` | `title` or `regenerateTitle: true` |
| Change model | `thread.meta.update` | `modelSelection` |
| Archive / unarchive / delete | `thread.archive`, `thread.unarchive`, `thread.delete` | |
| Permission mode | `thread.runtime-mode.set` | `approval-required`, `auto-accept-edits`, `auto`, `full-access` |
| Plan mode | `thread.interaction-mode.set` | `default` or `plan` |

`ModelSelection` is `{ instanceId, model, options?: [{ id, value }] }`. `instanceId` is the
configured provider instance from `ServerConfig.providers[]`; `model` is a slug from that
instance's `models[]`. Reasoning effort is not a field; it is an option whose descriptor comes
from `models[].capabilities.optionDescriptors` (for Claude, `id: "effort"` with values such as
`low`, `medium`, `high`). Omitting `options` uses the provider default.

Slash commands need no encoding: a provider command works when it is the first token of the
text. File mentions can be plain `@path` text. Attachments and the `t3-context://` chip format
are out of scope.

## 2. Decisions

**Transport.** Hand-written Effect RPC framing over `tokio-tungstenite`, JSON via `serde`.
About 200 lines. HTTP via `reqwest` for token exchange and tickets. No attempt to reuse
Effect's client; no codegen from the TypeScript schemas.

**Types.** Hand-written `serde` structs for the roughly fifteen schemas above. Every string
union gets an `Other(String)` variant, every struct uses `#[serde(default)]` for optional
fields, and activity payloads stay `serde_json::Value` until a renderer needs a field. Unknown
event or activity kinds are kept and ignored, never fatal.

**State.** A single `App` struct with a pure reducer per stream, mirroring
`packages/client-runtime/src/state/threadReducer.ts`: apply shell items to a project and thread
map, apply thread items to the open thread. Track `lastSequence` per subscription for resume.
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

- *Normal* (default): `j`/`k` scroll, `Ctrl-d`/`Ctrl-u` half page, `gg`/`G`, `J`/`K` next and
  previous thread, `Tab` focus thread list, `/` filter threads, `za` toggle the tool group under
  the cursor, `zM`/`zR` collapse or expand all, `i` or `Enter` to insert, `Esc` clear,
  `Ctrl-c` interrupt the running turn, `y` yank the assistant message under the cursor,
  `1`..`9` answer a pending approval.
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
pickers, `:` commands, prompt history, yank, config file, sectioned sidebar (pinned, active,
snoozed, folded gray settled shelf) with per-thread status labels and settle/unsettle/wake
commands. Not yet done: a recent-thread cache for instant back navigation, packaging.

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
- **Large threads.** A full thread snapshot can be large. Request `turnLimit` when the server
  advertises pagination, and render lazily from the bottom.
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
