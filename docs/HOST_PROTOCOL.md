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
- Unknown events and unknown fields must be ignored by hosts; new ones may be
  added in a minor version.

## Output events (beyond the standard stream-json set)

| event | data |
|---|---|
| `ready` | `input:"ndjson", pid, session_id, provider, model, sandbox, thinking_effort, approval, frames[]` |
| `input_ack` | `type, route, seq, detail` — one per stdin frame, in stdin order |
| `permission_request` | `id, tool, input, summary, choices:["once","always","deny"]` |
| `permission_resolved` | `id, tool, choice` |
| `thinking_preview_discarded` / `thinking_preview_committed` | none — a provider retry discards the thinking deltas streamed so far; commit seals them |

`route` values: `submitted` (prompt queued, not necessarily started),
`steering_queued` (accepted for a running turn),
`runtime_control_queued` (`/effort`… queued for mid-turn application),
`unsupported_busy_slash`, `withheld` (credential-looking text; see below),
`permission_forwarded`, `interrupted`, `close`, `invalid` (`detail` says why).

## Input frames

| frame | behaviour |
|---|---|
| `{"type":"user","text":…,"seq"?:…,"confirm_secret"?:true}` | prompt; while a turn runs it becomes steering |
| `{"type":"steer","text":…}` | same routing as `user` (explicit intent) |
| `{"type":"control","command":"/effort high"}` | slash command; runtime controls apply mid-turn, others are refused while busy |
| `{"type":"interrupt"}` | stop the running turn (and any pending permission → deny) |
| `{"type":"permission","id":…,"choice":"once"\|"always"\|"deny"}` | answer to a `permission_request`; other ids are ignored |
| `{"type":"close"}` | end the loop: acked, then the process exits once the current turn finishes |

`seq` is echoed verbatim in the ack. Text that looks like a credential is
`withheld` unless the agent is idle and `confirm_secret` is true. Busy input
never accepts credential confirmation. This mirrors the TUI's double-confirm;
prefer the local auth prompt for sudo/auth secrets.

Acknowledgements preserve stdin order, but turn events may precede the
corresponding acknowledgement. An acknowledgement confirms routing, not
completion; observe turn and steering events for execution.

## Lifecycle

- stdin EOF ends the loop exactly like `close`. Neither interrupts a running
  turn; accepted queued prompts drain before exit. Send `interrupt` first to
  abort the active turn; interrupt does not cancel queued prompts.
- A pending `permission_request` is denied on interrupt, on host EOF, or after
  30 minutes without a reply.

## Limits

- Frame: 256 KiB per line. A longer line gets an `invalid` ack and closes the
  bridge (the remainder cannot be resynchronised).
- Prompts, busy steering, and runtime controls share 32 pending slots. Each
  command in a busy comma-separated control sequence uses one slot; a sequence
  that does not fit is refused before any command is queued. Slots are reserved
  before publication and released by the consumer. Excess input gets `invalid`
  ("input queue full"); interrupt, close, and permission replies bypass this budget.
  This bounds ingress, not accumulated conversation history.
- Permission replies: 8 in flight; excess get `invalid`.
