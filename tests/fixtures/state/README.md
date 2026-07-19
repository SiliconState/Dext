# Durable-state compatibility fixtures

These fixtures are synthetic, minimal examples derived from Dext's retained on-disk schemas and the pre-campaign session replay fixtures. They contain no live prompts, credentials, tokens, absolute machine paths, or repository-specific object IDs.

| Area | Retained/current schema | Fixture intent |
|---|---|---|
| `sessions/` | implicit v1 · v2 · current v3 | Valid migrations, current metadata, future-version rejection, truncated JSONL rejection |
| `providers/` | catalog v1 · current v2 | Legacy/current normalization, future-version and corrupt JSON rejection |
| `auth/` | legacy provider map · current v1 | Legacy/current normalization using environment references only; future/corrupt rejection |
| `todo/` | unversioned JSON array | Valid list and corrupt/non-array rejection |
| `settings/` | unversioned JSON object | Valid compact threshold and corrupt/out-of-range rejection |
| `checkpoints/` | unversioned tab-delimited manifest | Valid entry, incomplete line, duplicate identity, unsafe path, and missing-ref cases; object IDs are test placeholders |
| `tool-journal/` | current v1 | Valid terminal/unresolved records, future-version, corrupt JSON, and mismatched identity rejection |

Older supported state is migrated only in memory. Reading a fixture must not rewrite it. Unsupported, corrupt, truncated, or tampered state is expected to fail without project mutation. Tests compare normalized behavior rather than JSON formatting or object order.
