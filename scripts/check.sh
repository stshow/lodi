#!/usr/bin/env sh
# The check a contributor runs before a pull request: formatting, clippy with warnings as
# errors, the build and every cargo test. It needs cargo on PATH; `nix develop` gives the
# pinned toolchain (docs/BUILDING.md).
#
#   sh scripts/check.sh
set -eu
# No host command in a test may reach this machine: without --root, each one is refused.
export LODI_HOST_REQUIRE_ROOT=1
cd "$(dirname -- "$0")/.."
if ! command -v cargo >/dev/null 2>&1; then
	echo "check: cargo is not on PATH (see docs/BUILDING.md)" >&2
	exit 127
fi
echo "check: cargo fmt --check"
cargo fmt --check
echo "check: cargo clippy"
cargo clippy --locked --quiet --all-targets -- -D warnings
echo "check: cargo build"
cargo build --locked --quiet
echo "check: cargo test"
cargo test --locked --quiet
echo "check: OK"
