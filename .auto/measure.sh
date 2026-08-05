#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

cargo test --locked main_tests::canonical_provider_neutral_prompt_fixture_stays_under_six_thousand_bytes -- --exact --nocapture 2>&1 \
    | grep '^METRIC '
