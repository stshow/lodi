#!/bin/sh
# scripts/install-hooks.sh
#
# Points this repository's git hooks at .githooks and makes them executable.
# Safe to re-run.
#
#   scripts/install-hooks.sh           a contributor's clone: the content checks only
#   scripts/install-hooks.sh --owner   the owner's clone (#572): also sets the
#                                      repository-level no-reply identity and
#                                      lodi.enforceIdentity=true, which turns on the
#                                      identity pin and the landing rules
#
# A clone that already has lodi.enforceIdentity=true is treated as --owner.

owner=""
case "${1:-}" in
    --owner) owner=1 ;;
    "") ;;
    *) echo "usage: scripts/install-hooks.sh [--owner]" >&2; exit 2 ;;
esac

repo_top=$(git rev-parse --show-toplevel 2>/dev/null)
if [ -z "$repo_top" ]; then
    echo "ERROR: not inside a git repository." >&2
    exit 1
fi
cd "$repo_top" || exit 1

if [ ! -d .githooks ]; then
    echo "ERROR: $repo_top/.githooks not found." >&2
    exit 1
fi

git config core.hooksPath .githooks
[ "$(git config --bool lodi.enforceIdentity 2>/dev/null)" = "true" ] && owner=1
if [ -n "$owner" ]; then
    git config lodi.enforceIdentity true
    # AGENTS.md section 1.1 (#339): the repository-level identity is the one address that
    # .githooks/allowed-emails allows (an anchored pattern), under the handle before its "@".
    allowed=$(grep -v -e '^#' -e '^[[:space:]]*$' .githooks/allowed-emails 2>/dev/null)
    if [ "$(printf '%s\n' "$allowed" | grep -c .)" -eq 1 ]; then
        email=$(printf '%s\n' "$allowed" | sed -e 's/^\^//' -e 's/\$$//' -e 's/\\//g')
        handle=${email%%@*}
        git config user.name "${handle#*+}"
        git config user.email "$email"
    else
        echo "WARNING: .githooks/allowed-emails does not hold exactly one address; identity not set." >&2
    fi
fi
# git mergetool keeps no `<file>.orig` backup to be staged by mistake (#338).
git config mergetool.keepBackup false

for hook in pre-commit commit-msg pre-push; do
    if [ -f ".githooks/$hook" ]; then
        chmod +x ".githooks/$hook"
    fi
done

echo "Installed git hooks for $repo_top"
echo "  core.hooksPath = $(git config core.hooksPath)"
echo "  user.email     = $(git config user.email)"
echo "  lodi.enforceIdentity = $(git config --bool lodi.enforceIdentity 2>/dev/null || echo false)"
echo "Active hooks:"
for hook in pre-commit commit-msg pre-push; do
    if [ -x ".githooks/$hook" ]; then
        echo "  - $hook"
    fi
done
