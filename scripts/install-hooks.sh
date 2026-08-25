#!/usr/bin/env bash
#
# Point git at the hooks in this repository.
#
# Git will not do this for you: hooks live in .git/hooks, which is not version
# controlled, so a fresh clone has none. core.hooksPath redirects it at a
# directory that is. Run this once per clone.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
git config core.hooksPath .githooks
echo "hooks installed from .githooks"
echo
echo "  commit-msg  refuses attribution footers"
echo "  pre-commit  refuses anything shaped like a credential"
echo
echo "CI checks the same things, so this is a faster failure and not the only one."
