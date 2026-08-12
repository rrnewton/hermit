#!/usr/bin/env bash
# Install Hermit's tracked pre-commit checks for this clone/worktree repository.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit .githooks/prepare-commit-msg .githooks/commit-msg

echo "core.hooksPath -> .githooks"
echo "Active: Reverie pin pre-commit and compatibility-scorecard commit-message gates"
echo "Policy: docs/updating-reverie.md"
