# Host protocol: `dext --input ndjson --output stream-json`

Version 1. For hosts (DextUI's agentlinkd, editors, scripts) that keep one
long-lived dext process per seat instead of one `-p` process per prompt.
Requires `--output stream-json`; `-p`, `--pack`, and positional prompts are
rejected before reading stdin. Invoke packs with a `/pack run …` control frame.

## stdout / stderr contract

- stdout carries **only** JSON objects, one per line, in event order. Loop
  chatter is `{"event":"info","data":…}`, refusals `warn`, failures `error`.
  No banner, prompt, or blank line — including under `--fork`.
- stderr is free text and may be discarded.
- Wait for `ready` before sending input. Resume diagnostics and startup approval
  events can precede it; startup failure can exit without `ready`. Startup
  permission requests resolve to deny immediately because replies cannot yet
  be consumed; retry the protected action after `ready`. The stdin
  reader starts only after `ready` has been written.
- `tool_output_delta` carries live bash `{call_id, name, stream, text}` before
  the final tool result, including split-read UTF-8 decoding. It is emitted in
  one-shot stream-json mode too. Hosts must drain stdout and bound retained output.
  Credential-bearing runtime helpers still suppress live output; ordinary bash
  chunks may contain sensitive data and are not a secret-scrubbing boundary.
- Unknown events and unknown fields must be ignored by hosts; new ones may be
  added in a minor version.

## Output events (beyond the standard stream-json set)

| event | data |
|---|---|
| `ready` | `input:"ndjson", pid, session_id, provider, model, sandbox, thinking_effort, approval, frames[], ui_protocol:1` |
| `input_ack` | `type, route, seq, detail` — one per stdin frame, in stdin order |
| `permission_request` | `id, tool, input, summary, choices:["once","always","deny"]` |
| `permission_resolved` | `id, tool, choice` |
| `ui.request` | `id, pack, request_id, method, params` — an active pack asks an advertised host UI method; answer with `ui.response` |
| `thinking_preview_discarded` / `thinking_preview_committed` | none — a provider retry discards the thinking deltas streamed so far; commit seals them |

`route` values: `submitted` (prompt queued, not necessarily started),
`steering_queued` (accepted for a running turn),
`runtime_control_queued` (`/effort`… queued for mid-turn application),
`unsupported_busy_slash`, `withheld` (credential-looking text; see below),
`permission_forwarded`, `ui_capabilities_set`, `ui_response_forwarded`,
`interrupted`, `close`, `invalid` (`detail` says why).

## Input frames

| frame | behaviour |
|---|---|
| `{"type":"user","text":…,"seq"?:…,"confirm_secret"?:true}` | prompt; while a turn runs it becomes steering |
| `{"type":"steer","text":…}` | same routing as `user` (explicit intent) |
| `{"type":"control","command":"/effort high"}` | slash command; runtime controls apply mid-turn, others are refused while busy |
| `{"type":"interrupt"}` | stop the running turn (and any pending permission → deny) |
| `{"type":"permission","id":…,"choice":"once"\|"always"\|"deny"}` | answer to a `permission_request`; other ids are ignored |
| `{"type":"ui.capabilities","methods":["form","progress"]}` | replace the host's advertised pack-UI methods; send after `ready` and before invoking a pack |
| `{"type":"ui.response","id":…,"status":"ok","value"?:…}` | successful answer to the matching pending `ui.request`; `value` is opaque JSON |
| `{"type":"ui.response","id":…,"status":"cancelled"}` | cancel the matching pending request; stale, duplicate, or non-matching ids are refused |
| `{"type":"ui.response","id":…,"status":"error","code":…,"message":…}` | host could not complete the request |
| `{"type":"close"}` | end the loop: acked, then the process exits once the current turn finishes |

`seq` is echoed verbatim in the ack. Text that looks like a credential is
`withheld` unless the agent is idle and `confirm_secret` is true. Busy input
never accepts credential confirmation. This mirrors the TUI's double-confirm;
prefer the local auth prompt for sudo/auth secrets.

Acknowledgements preserve stdin order, but turn events may precede the
corresponding acknowledgement. An acknowledgement confirms routing, not
completion; observe turn and steering events for execution.

## Pack UI channel

The UI channel is host-neutral. Dext does not depend on DextUI, a browser, or a particular widget toolkit. A runtime opts in with `"ui_protocol":1` in `runtime.json`; without that declaration, returning `ui_request` fails closed. A host advertises the method names it implements with `ui.capabilities`; the active runtime sees the sorted list as `context.ui_methods`. `"*"` opts into every method. A runtime response may contain one `ui_request`:

```json
{"ui_request":{"id":"profile","method":"form","params":{"title":"Profile","fields":[{"id":"name","label":"Name","type":"text","required":true}]}}}
```

Dext emits:

```json
{"event":"ui.request","data":{"id":"ui-123-1","pack":"example","request_id":"profile","method":"form","params":{"title":"Profile","fields":[…]}}}
```

The host correlates with the transport `data.id`, not the pack-local `request_id`, and replies with a top-level `ui.response` input frame. Dext accepts only the first response matching the one pending request; early, stale, duplicate, and non-matching ids receive an `invalid` acknowledgement and cannot fill the response queue. Dext then invokes the same one-shot runtime with `event:"ui_response"` and:

```json
{"ui":{"request_id":"profile","method":"form","response":{"status":"ok","value":{"name":"Ada"}}}}
```

The runtime may return another `ui_request`, up to 16 sequential round trips per activation/tool/idle chain. This supports forms, progress acknowledgements, pickers, previews, or future host-defined interactions without adding widget types to core. For portable packs, use these baseline method conventions:

- `form`: `params` may contain `title`, `description`, `submit_label`, and `fields[]`; each field uses a stable `id`, human `label`, `type` (`text`, `textarea`, `number`, `boolean`, `select`, or `multiselect`), and optional `required`, `default`, `options`, `placeholder`, and `description`. An `ok` value is an object keyed by field id.
- `progress`: `params` may contain stable `id`, `title`, `message`, `current`, `total`, and `state` (`running`, `completed`, or `error`). The host acknowledges with `status:"ok"`; repeated ids update the same host presentation.

Method params and successful values remain opaque to Dext beyond bounds and terminal-safety validation, so hosts may implement richer namespaced methods. Packs must inspect `context.ui_methods` and handle the synthetic `unsupported` error returned when a method was not advertised. Front ends without this channel continue to work and never receive a `ui.request`.

Request params are privacy-redacted, including object keys as well as string values, and revalidated after redaction before leaving Dext. Responses travel only to the approved local runtime invocation; Dext does not emit, log, add to model history, or persist them itself. A runtime can still copy a response into its returned content/state/effects, after which the ordinary redaction and state rules apply; pack state must not contain secrets. Interrupt aborts the active runtime/UI chain instead of launching another helper after cancellation.

## Background compaction lifecycle (v1)

Opt in through child environment `DEXT_BACKGROUND_COMPACT=1`; off by default. The separate `background_compaction` event has `{version:1,session_id,session_epoch,job_id,origin_turn_id,phase,blocking,reason,elapsed_ms,wait_ms,before_chars,after_chars?,usage_known}`. Phases are `running`, `ready`, `waiting`, `applied`, `failed`, `cancelled`, `discarded`; only `waiting` has `blocking:true`. These events do not themselves start/finish user turns, clear permissions, disable input, or set legacy `compacting`. Terminal phases clear transient status. On application only, `history_context_updated` and `compact_end` with `background:true,job_id` publish installed history; no `compact_start` is emitted. Summary usage is included once in authoritative `usage_update.session`, including discarded attempts; hosts must not add job metrics to that accounting.

Hosts must fence by bridge generation, core session and job identity. Same-bridge reconnect can project the live badge; bridge death/restart clears it rather than restoring a transient job from journal replay. Background-only work must not set G1's interrupted-turn recovery marker. Kept forks inherit no job. G6 `post_compact` fires on application, never compute completion. Manual compaction, interrupt, session/config changes and shutdown settle/cancel work; idle NDJSON completion is serviced without another prompt. Configured budgets and pre-request hooks conservatively disable speculation. Missing provider usage and earlier unobserved retry-attempt billing stay marked unknown even after a later success; usage decoded before terminal validation fails remains counted. Both blocking and background summaries are privacy-redacted before installed history/events/observers and saving. TUI owners checkpoint accumulated accounting after idle completion and shutdown settlement, including failed/discarded jobs. Known usage is frozen after worker termination except synchronous session switches, which retire best-known old-job accounting and mark unfinished billing unknown before restored totals are installed. The v1 contract is ACKed by DextUI; its isolated real-core/host barrier, idle apply, reconnect, interrupt and cumulative accounting gates passed. Same-child installed `compact_end` may arrive buffered after an interrupt; hosts accept it once for a previously known job without reviving a retired badge. Known-job identities are bounded and cleared on bridge/session generation changes. Independent Linux checks against the installed core confirm the two-turn barrier, idle application, reconnect and once-only accounting. An isolated approved `post_compact` observer is absent during computation and sees the installed summary exactly once after application, without starting a user turn; the defaults-off control also passes.

## Kept fork (one-shot CLI)

`dext --fork-to NEW_SEAT [--at N] [--resume SELECTOR] [--seat SOURCE] --cd ROOT --output stream-json` copies a coherent source snapshot and exits without a provider request. It conflicts with `--input ndjson`, unsaved `--fork`, `--no-session`, `-p`, packs, eval and prompts. stdout is one `{"event":"session_fork","data":{"seat":"NEW_SEAT","session_id":"NEW_ID","source_session_id":"SOURCE_ID","at":N}}` record, exit 0. Invalid/missing sources, nonportable or existing targets, out-of-range counts and storage errors exit 1 with stderr diagnostics.

`--at` counts core `Message` records after the header (nonblank JSONL lines), not host journal sequence numbers or text deltas. It defaults to all source messages; a cut splitting a tool pair rounds backward to a pair-safe boundary using a linear transcript scan and reports the actual retained count. Empty prefix (`--at 0`) is valid. Hosts must map their sequence numbers to exact core text/tool identities in a coherent idle transcript, and reject ambiguous or compacted-away selections rather than treating host seq as message count. Each message is `{role,content:[...]}`; tool uses have `{type:"tool_use",id,name,input}`, results `{type:"tool_result",tool_use_id,content,...}`. Source header must carry a valid session id and matching project sandbox provenance. The new Seat has a new session id and an empty journal; session sidecars, pack runtime/queued continuations, approval grants, accounting and work ledger are not copied. The source is never reconciled, executed or modified by this command.

DextUI `921633f` implements `session.fork {id,at_seq?}` by freezing a bounded coherent checkpoint, mapping exact host text/tool identities to core Message counts, and invoking the one-shot command. Invalid or ambiguous selections create no host session. The returned actual count rebuilds only retained history; control state and background jobs are not copied. Joint Linux acceptance against core `4d014a59` verifies pair rounding, source byte immutability, new Seat/session identity, no provider request, host restart and independent continuation. The four source/mapping/real-core checks also pass against the installed PATH binary; the peer reports 308 tests plus browser/chart, Svelte and isolated production-build gates. These are isolated acceptance results, not live-host deployment evidence.

## Crew ownership and host interrupt

Core bash calls expose `DEXT_SESSION_ID` and exact provider `DEXT_TOOL_CALL_ID`; a crew manifest may record `owner:{session,call_id,mode}`. This core identity is not the host session id. Persist `ready.session_id`, or validate checkpoint provenance for one-shot/restored sessions. Discover foreground pending/running/paused runs across configured roots without relying on capped display summaries; key by manifest path rather than display id. Crew accepts 1–64 ASCII letters, digits, dots, underscores or hyphens, excluding `.` and `..`; discovery must include `key-<32 hex>` idempotent runs and custom ids, not only generated `run-<12 hex>` ids.

Coalesce duplicate stops and allow at least 45 seconds per stop: crew may wait 35 seconds for its lease while reviewed cleanup uses one aggregate 30-second execution budget. Stop owned foreground runs before signalling the parent tool process group so cleanup is not killed mid-hook. Block new prompt/timer/steering admissions during the cascade and fence the final signal to the original session/turn/child generation. Leave background, unowned and other-session runs alone; surface cleanup uncertainty rather than replaying it.

Isolated Linux joint checks using the installed core `4d014a59` and crew `206b6bf` pass foreground worker reaping, once-only cleanup, same-session background survival and repeat-interrupt no-replay for generated, custom and actual idempotency-keyed run ids. Slow-cleanup prompt/steering admission tests also preserve the warm bridge when the parent finishes during cleanup. The adapter and protocol now share crew's bounded portable id grammar; internal admissions and the final epoch/turn/child fence are implemented. Joint G7 acceptance is recorded at DextUI `30cc00b`, stacked on kept-fork `921633f` and background-compaction `2a6d18c`. The peer reports 315 tests plus browser/chart, typecheck/Svelte and isolated production-build gates, four installed-core checks, two installed-crew checks, and the final bulk-management/admission regression. Core independently verifies the persisted 12-test fork/compaction/crew suite and seven owned-run checks. These are isolated acceptance results, not live-host deployment evidence. Real-core fixtures require workspaces outside the parent source checkout so recovery checkpoints cannot race unrelated parallel-test files; checkpoint refusal is not bypassed.

## Lifecycle

- stdin EOF ends the loop exactly like `close`. Neither interrupts a running
  turn; accepted queued prompts drain before exit. Send `interrupt` first to
  abort the active turn; interrupt does not cancel queued prompts.
- A pending `permission_request` is denied on interrupt, on host EOF, or after
  30 minutes without a reply. A pending `ui.request` aborts with the active
  operation on interrupt; explicit host cancellation is delivered as `cancelled`,
  while EOF and timeout are delivered to the runtime as `disconnected` and
  `timeout` responses for fallback/cleanup.

## Limits

- Frame: 256 KiB per line. A longer line gets an `invalid` ack and closes the
  bridge (the remainder cannot be resynchronised).
- Prompts, busy steering, and runtime controls share 32 pending slots. Each
  command in a busy comma-separated control sequence uses one slot; a sequence
  that does not fit is refused before any command is queued. Slots are reserved
  before publication and released by the consumer. Excess input gets `invalid`
  ("input queue full"); interrupt, close, and permission replies bypass this budget.
  This bounds ingress, not accumulated conversation history.
- Permission replies: 8-entry bounded channel. UI responses: one-slot channel for the one pending request; only the first matching id is admitted, and excess/stale/duplicate frames get `invalid`.
- UI method lists: at most 64 names; method names, request ids, error codes, and response correlation ids are 1–64 safe ASCII characters. Params and successful values are each capped at 64 KiB, error messages at 2,000 bytes, and one runtime chain may perform at most 16 UI round trips. Unsafe terminal controls in object keys or values fail closed.
