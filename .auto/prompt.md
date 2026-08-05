# Dext universal prompt-efficiency autoresearch

## Objective

Reduce the provider-visible input Dext supplies for the first request in a clean new repository while preserving or improving agent reliability. Optimize the universal Dext agent, not repository-specific `DEXT.md`, `recall.md`, todo, Seat, pack, shelf, or prior-history content.

## Primary metric

- `total_bytes`, lower is better.
- Deterministic composition: `DEFAULT_SYSTEM` UTF-8 bytes + two separators + a canonical clean-repository standard-mode runtime tail + the serialized ChatGPT Responses shape of all 13 default tools using lean descriptions/schemas.
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
4. Tool schemas must remain valid in Anthropic, OpenAI, and ChatGPT wire shapes; required-field registry and permission/parallel metadata must not drift.
5. Every kept result must pass `.auto/checks.sh`. Final completion must pass the full Dext verification matrix and reinstall the binary.
6. Prefer deleting duplicated prose over deleting unique policy. Put tool-specific facts in lean tool descriptions and cross-tool policy in the system prompt.
7. Do not alter this benchmark's canonical runtime-tail fixture merely to claim an improvement. If runtime-tail production is deliberately changed, update the fixture only with paired source tests and log the old/new contribution explicitly.

## Benchmark

```bash
bash .auto/measure.sh
```

The script parses the actual Rust source for `DEFAULT_SYSTEM`, full schemas, default-tool membership, and lean descriptions; it serializes the provider-facing lean tool payload deterministically and adds a fixed canonical first-request runtime tail.

## Correctness backpressure

```bash
bash .auto/checks.sh
```

Each measured experiment runs formatting plus focused prompt, context-state, schema-registry, lean-schema, and provider-shape tests.

## Prior findings

- Measured baseline: 8,781 bytes (~2,196 tokens): 4,250 system, 3,846 tools, 683 runtime tail.
- Compact invariant-driven system prompt plus minimal lean descriptions reached 5,999 bytes (~1,500 tokens) while retaining all 13 default tools, schema fields, and semantic guardrail tests.
- Omitting all-zero pre-action strategy rows reached 5,891 bytes (~1,473 tokens) while preserving explicit post-action reset/pivot budgets.
- The runtime tail still carries toolset/schema labels already evident in tool definitions and host-only compaction thresholds; remove only those, retaining actionable provider/model/effort/context and policy state.
- Under 1,000 tokens is structurally impossible with the current tool payload plus meaningful system/runtime context; reaching it likely requires on-demand/adaptive tool exposure, not unsafe prose deletion.
