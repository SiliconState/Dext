#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo test --locked systems_preserve_tool_protocol_guardrails_and_table_guidance
cargo test --locked tool_registry_covers_every_catalog_entry_and_schema_requirement
cargo test --locked lean_tool_profile_keeps_descriptions_useful_and_schemas_slim
cargo test --locked all_provider_tool_wrappers_preserve_dynamic_tool_semantics
cargo test --locked tool_disabled_models_expose_no_static_or_dynamic_tools
cargo test --locked chatgpt_tools_are_responses_api_shape
cargo test --locked prompt_env_values_are_bounded_and_cannot_inject_lines
cargo test --locked compose_system_parts_quotes_unsafe_environment_values
cargo test --locked prompt_runtime_state_cannot_inject_lines
cargo test --locked canonical_provider_neutral_prompt_fixture_stays_under_six_thousand_bytes
cargo test --locked context_state_warns_on_repeated_actions_and_strategy_budget
cargo test --locked context_state_omits_strategy_budget_before_first_action
cargo test --locked compose_system_parts_includes_context_state_section
