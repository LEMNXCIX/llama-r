#!/usr/bin/env bash
set -euo pipefail
grep -q 'package-ecosystem: "cargo"' .github/dependabot.yml
! grep -q 'package-ecosystem: ""' .github/dependabot.yml
grep -q 'cargo-audit --version 0.22.0' .github/workflows/ci.yml
grep -q 'llvm-tools-preview' .github/workflows/ci.yml
grep -q 'cargo llvm-cov' .github/workflows/ci.yml
# cargo-llvm-cov has no bare `--fail-under` flag (only `--fail-under-lines`
# and friends), so the 40% floor is asserted under its real spelling.
grep -q -- '--fail-under-lines 40' .github/workflows/ci.yml
grep -q 'cargo deny check' .github/workflows/ci.yml
grep -q 'cargo build --release' .github/workflows/ci.yml
test -f deny.toml && test -f rust-toolchain.toml
test -f .github/workflows/release.yml
grep -q 'tags:' .github/workflows/release.yml
grep -q 'matrix:' .github/workflows/release.yml
grep -q 'macos-latest' .github/workflows/release.yml
grep -q 'windows-latest' .github/workflows/release.yml
grep -q 'action-gh-release' .github/workflows/release.yml
! grep -q 'docker' .github/workflows/release.yml
echo CI_CONFIG_OK
