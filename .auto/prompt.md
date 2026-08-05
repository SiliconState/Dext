# Dext universal prompt-efficiency autoresearch

## Objective

Reduce the provider-visible input Dext supplies for the first request in a clean new repository while preserving or improving agent reliability. Optimize the universal Dext agent, not repository-specific `DEXT.md`, `recall.md`, todo, Seat, pack, shelf, or prior-history content.

## Primary metric

- `total_bytes`, lower is better.
- Deterministic canonical fixture: `DEFAULT_SYSTEM` UTF-8 bytes + a clean-repository standard-mode runtime tail with neutral cwd/OS/provider/model/Git values + canonical JSON for all 13 default lean `{name, description, schema}` descriptors.
- The normalized descriptor payload is provider-independent comparison data, not an actual provider request, tokenizer count, or billing count. Actual tool-array bytes and wrapper overhead are reported separately for Anthropic cache-on/cache-off, OpenAI Chat Completions, OpenAI Responses, and ChatGPT Responses.
- `approx_tokens = ceil(total_bytes / 4)` is the target-facing secondary metric.
- Goal: strictly below 6,000 bytes (~1,500 tokens). Stretch goal below 4,000 bytes (~1,000 tokens) only if behavior and capabilities remain intact.

## Scope

- `src/main.rs`: `DEFAULT_SYSTEM` and genuinely redundant clean-repository runtime-tail text.
- `src/tools.rs`: lean descriptions and, only with evidence, default exposure.
- `src/main_tests.rs`: prompt/tool behavior and budget regression tests.
- `docs/index.html`, `docs/ARCHITECTURE.md`, `docs/USAGE.md`: same-change documentation.
- `.auto/measure.sh`, `.auto/checks.sh`, `.auto/prompt.md`, `.auto/ideas.md`: benchmark evidence and evolving findings.

Do not optimize repository-specific injected prompt files, full-schema mode, tiny mode, unrelated runtime behavior, or output caps.

## Invariants

1. Keep the 13 default native tools and every schema field unless an experiment demonstrates a safer equivalent architecture; do not hide capability merely to win the metric.
2. Preserve real provider tool-call protocol, unavailable-tool discipline, approval/sandbox compliance, read-before-edit, native-tool-before-Bash policy, exact-path handling for queued user updates, bounded reads/results, atomic Bash lifecycle, supervised persistent services, narrow verification, and concise final reporting.
3. Preserve `DEXT.md`/`recall.md` project-context behavior, packs/shelves behavior, and context-state pivot semantics.
4. Static and active runtime tool names, descriptions, and schemas must remain identical through Anthropic Messages, OpenAI Chat Completions, OpenAI Responses, and ChatGPT Responses adapters; required-field registry and permission/parallel metadata must not drift.
5. Every kept result must pass `.auto/checks.sh`. Final completion must pass the full Dext verification matrix and reinstall the binary.
6. Prefer deleting duplicated prose over deleting unique policy. Put tool-specific facts in lean tool descriptions and cross-tool policy in the system prompt.
7. Do not alter this benchmark's canonical runtime-tail fixture merely to claim an improvement. If runtime-tail production is deliberately changed, update the fixture only with paired source tests and log the old/new contribution explicitly.

## Benchmark

```bash
bash .auto/measure.sh
```

The Rust regression composes the real `DEFAULT_SYSTEM`, a clean-repository environment tail with neutral provider/model placeholders, default-tool membership, and canonical JSON for lean provider-neutral descriptors. `.auto/measure.sh` relays that fixture's primary metrics plus actual per-contract tool-array bytes and wrapper overhead. The normalized payload is a reproducible comparison fixture, not a provider request, tokenizer count, billing count, or maximum for arbitrary runtime strings.

## Correctness backpressure

```bash
bash .auto/checks.sh
```

Each measured experiment runs formatting plus focused prompt, context-state, schema-registry, lean-schema, provider-shape, and real clean-repository composition tests.

## Prior findings

- Final kept experiment result before review: 5,775 bytes (~1,444 tokens): 1,747 system, 3,567 tools, 459 runtime tail. It retained all 13 default tools/schema fields, removed pre-action all-zero strategy rows, and removed only redundant/host-only environment diagnostics.
- Fresh-eye review replaced the benchmark's brittle Python source parser with real Rust composition. A second review corrected the provider-specific ChatGPT measurement: the current provider-neutral normalized result is 5,504 bytes (~1,376 diagnostic tokens): 1,753 system, 3,296 normalized tools, and 455 runtime tail. Actual tool-array bytes are reported separately per contract. Essential lean cues remain for line-numbered windows, line-based symbol lookup, HTTPie-style arguments, absolute-path reads, Bash pipefail/caps, create/overwrite semantics, and stat-first broad diffs.
- Under 1,000 tokens is structurally impossible with the current tool payload plus meaningful system/runtime context; reaching it likely requires on-demand/adaptive tool exposure with task-quality evaluation, not further prose deletion.
