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

## Kept fork (one-shot CLI)

`dext --fork-to NEW_SEAT [--at N] [--resume SELECTOR] [--seat SOURCE] --cd ROOT --output stream-json` copies a coherent source snapshot and exits without a provider request. It conflicts with `--input ndjson`, unsaved `--fork`, `--no-session`, `-p`, packs, eval and prompts. stdout is one `{"event":"session_fork","data":{"seat":"NEW_SEAT","session_id":"NEW_ID","source_session_id":"SOURCE_ID","at":N}}` record, exit 0. Invalid/missing sources, nonportable or existing targets, out-of-range counts and storage errors exit 1 with stderr diagnostics.

`--at` counts core `Message` records after the header (nonblank JSONL lines), not host journal sequence numbers or text deltas. It defaults to all source messages; a cut splitting a tool pair rounds backward to a pair-safe boundary and reports the actual retained count. Empty prefix (`--at 0`) is valid. Hosts must map their sequence numbers to exact core text/tool identities in a coherent idle transcript, and reject ambiguous or compacted-away selections rather than treating host seq as message count. Each message is `{role,content:[...]}`; tool uses have `{type:"tool_use",id,name,input}`, results `{type:"tool_result",tool_use_id,content,...}`. Source header must carry a valid session id and matching project sandbox provenance. The new Seat has a new session id and an empty journal; session sidecars, pack runtime/queued continuations, approval grants, accounting and work ledger are not copied. The source is never reconciled, executed or modified by this command.

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
