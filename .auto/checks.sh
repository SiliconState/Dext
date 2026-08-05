#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo test --locked systems_preserve_tool_protocol_guardrails_and_table_guidance
cargo test --locked tool_registry_covers_every_catalog_entry_and_schema_requirement
cargo test --locked lean_tool_profile_keeps_descriptions_useful_and_schemas_slim
cargo test --locked chatgpt_tools_are_responses_api_shape
cargo test --locked context_state_warns_on_repeated_actions_and_strategy_budget
cargo test --locked compose_system_parts_includes_context_state_section
