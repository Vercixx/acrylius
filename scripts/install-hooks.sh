#!/usr/bin/env bash
#
# Point git at the hooks in this repository. Run once per clone:
# .git/hooks is not version controlled, so a fresh clone has none.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
git config core.hooksPath .githooks
echo "hooks installed from .githooks"
echo
echo "  commit-msg  refuses attribution footers"
echo "  pre-commit  refuses anything shaped like a credential"
echo
echo "CI checks the same things, so this is a faster failure and not the only one."
