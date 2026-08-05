# Hypotheses

1. Consolidate duplicated universal prose into a compact invariant-driven system prompt; retain unique safety and workflow semantics.
2. Shorten lean tool descriptions where the schema or tool name already conveys behavior.
3. Replace table-rendering implementation detail with one compact rendering rule.
4. Keep exact-path queued-update handling explicit because it protects literal user scope.
5. After reaching <1,500 estimated tokens, evaluate whether adaptive/on-demand default tool exposure can approach <1,000 without reducing task completion quality.

# Rejected shortcuts

- Do not count a repository-specific `DEXT.md` or `recall.md` reduction as universal improvement.
- Do not switch the benchmark to tiny mode.
- Do not remove default tools solely to lower schema bytes.
- Do not weaken approval, sandbox, Bash lifecycle, or verification rules.
