#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/ci-package-excludes.sh"

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
export RUST_BACKTRACE="${RUST_BACKTRACE:-full}"
export SKIP_WASM_BUILD="${SKIP_WASM_BUILD:-1}"

cargo +nightly fmt --check
taplo fmt --check --config .config/taplo.toml

cargo clippy --workspace --all-targets \
	"${CI_PACKAGE_EXCLUDES[@]}" \
	-- -D warnings

zepter run check
