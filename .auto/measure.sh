#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import json
import pathlib
import re

main = pathlib.Path("src/main.rs").read_text()
tools_source = pathlib.Path("src/tools.rs").read_text()

system_start_marker = 'const DEFAULT_SYSTEM: &str = "'
system_start = main.index(system_start_marker) + len(system_start_marker)
system_end = main.index('";\n\nconst TINY_SYSTEM', system_start)
system = main[system_start:system_end]

catalog_start = tools_source.index("pub(crate) fn provider_tool_definitions()")
catalog_end = tools_source.index("\n#[derive(Debug, Clone, Copy)]", catalog_start)
catalog = tools_source[catalog_start:catalog_end]

blocks = []
position = 0
while True:
    block_start = catalog.find("Tool {", position)
    if block_start < 0:
        break
    brace_start = catalog.index("{", block_start)
    depth = 0
    in_string = False
    escaped = False
    for index in range(brace_start, len(catalog)):
        char = catalog[index]
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
        else:
            if char == '"':
                in_string = True
            elif char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
                if depth == 0:
                    blocks.append(catalog[block_start:index + 1])
                    position = index + 1
                    break
    else:
        raise SystemExit("unbalanced Tool block")

lean_start = tools_source.index("fn lean_description")
lean_end = tools_source.index("\nfn slim_schema", lean_start)
lean_block = tools_source[lean_start:lean_end]
lean_descriptions = dict(
    re.findall(r'^\s*"([^"]+)"\s*=>\s*"([^"]*)",?$', lean_block, re.MULTILINE)
)

def extract_schema(block):
    marker = "input_schema: json!("
    start = block.index(marker) + len(marker)
    depth = 0
    in_string = False
    escaped = False
    for index in range(start, len(block)):
        char = block[index]
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
        else:
            if char == '"':
                in_string = True
            elif char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
                if depth == 0:
                    return json.loads(block[start:index + 1])
    raise SystemExit("unbalanced input schema")

def slim(value):
    if isinstance(value, dict):
        return {key: slim(child) for key, child in value.items() if key != "description"}
    if isinstance(value, list):
        return [slim(child) for child in value]
    return value

spec_start = tools_source.index("const TOOL_SPECS")
spec_end = tools_source.index("\n];", spec_start)
spec_text = tools_source[spec_start:spec_end]
default_names = set()
position = 0
while True:
    entry_start = spec_text.find("tool(", position)
    if entry_start < 0:
        break
    depth = 0
    in_string = False
    escaped = False
    for index in range(entry_start + len("tool("), len(spec_text)):
        char = spec_text[index]
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
        else:
            if char == '"':
                in_string = True
            elif char == "(":
                depth += 1
            elif char == ")":
                if depth == 0:
                    entry = spec_text[entry_start:index + 1]
                    name = re.search(r'tool\(\s*"([^"]+)"', entry).group(1)
                    if "DEFAULT_PROFILE" in entry:
                        default_names.add(name)
                    position = index + 1
                    break
                depth -= 1
    else:
        raise SystemExit("unbalanced ToolSpec entry")

wire_tools = []
for block in blocks:
    name = re.search(r'name:\s*"([^"]+)"', block).group(1)
    if name not in default_names:
        continue
    if name not in lean_descriptions:
        raise SystemExit(f"missing lean description for {name}")
    wire_tools.append({
        "type": "function",
        "name": name,
        "description": lean_descriptions[name],
        "parameters": slim(extract_schema(block)),
        "strict": None,
    })

if len(wire_tools) != 13:
    raise SystemExit(f"expected 13 default tools, found {len(wire_tools)}")

tool_json = json.dumps(wire_tools, separators=(",", ":"), ensure_ascii=False).encode()

# Canonical clean-repository first request after ObjectiveTracker initialization.
# It intentionally excludes optional DEXT.md/recall/todo/Seat/pack/shelf/history state.
runtime_tail = """## Environment
cwd=/work/new-repo os=linux git=main provider=chatgpt model=gpt-5.6-sol effort=xhigh context=standard toolset=default schemas=lean approval=ask sandbox=workspace-write
history_compact_threshold_chars=3360000 active_history_compact_threshold_chars=3360000

## Work ledger
current_phase: probe

## Context State
Active checkpoints:
- [unresolved] deliver requested outcome with verifiable steps
privacy=redact (user-readable files remain readable; private keys, secret assignments, and labeled SSNs/cards/accounts are redacted before model context/session logs)
""".encode()

system_bytes = len(system.encode())
tools_bytes = len(tool_json)
env_bytes = len(runtime_tail)
total_bytes = system_bytes + 2 + env_bytes + tools_bytes
approx_tokens = (total_bytes + 3) // 4

print(f"METRIC total_bytes={total_bytes}")
print(f"METRIC approx_tokens={approx_tokens}")
print(f"METRIC system_bytes={system_bytes}")
print(f"METRIC tools_bytes={tools_bytes}")
print(f"METRIC env_bytes={env_bytes}")
print(f"METRIC default_tools={len(wire_tools)}")
PY
