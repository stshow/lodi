#!/bin/sh
# .githooks/lib.sh
#
# Shared helpers for the email-leak-prevention git hooks (pre-commit,
# commit-msg, pre-push). POSIX sh only; depends on nothing beyond
# git, grep, sed and awk. This file is meant to be *sourced*, not
# executed directly.

# --- locate the hooks directory / allowlist file -------------------------
#
# Git always changes its working directory to the top of the working tree
# (or to $GIT_DIR for a bare repository) before invoking a hook, regardless
# of core.hooksPath, so this resolves correctly no matter where the hook
# itself lives on disk.
_githooks_lib_dir() {
    top=$(git rev-parse --show-toplevel 2>/dev/null)
    if [ -n "$top" ]; then
        printf '%s/.githooks\n' "$top"
        return
    fi
    gd=$(git rev-parse --git-dir 2>/dev/null)
    printf '%s/.githooks\n' "${gd:-.git}"
}

HOOKS_DIR=$(_githooks_lib_dir)
ALLOWED_EMAILS_FILE="$HOOKS_DIR/allowed-emails"

# Email-like token, close enough to RFC 5322 for our purposes.
EMAIL_TOKEN_REGEX='[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'

# Domains that are never acceptable, no matter what allowed-emails says.
# This exists so nobody can "fix" a failing hook by loosening the allowlist
# to cover a personal address. Common freemail providers, in alphabetical
# order, so that no entry stands out.
BLOCKED_DOMAIN_REGEX='@(aol\.com|fastmail\.com|gmail\.com|gmx\.(com|de|net)|hotmail\.com|icloud\.com|live\.com|mac\.com|mail\.ru|me\.com|msn\.com|outlook\.com|pm\.me|proton\.me|protonmail\.(ch|com)|tutanota\.com|yahoo\.com|yandex\.(com|ru)|zoho\.com)$'

# The provider words AGENTS.md §1.6 names, in the same alphabetical order:
# the one list that pre-commit, the scans below and the development gate use.
# A word is left out of this list, and kept in the domain list above, when
# it is an ordinary substring of source text (`live.com` in
# `live.communicate`, `mail.ru` in `email.run`).
PROVIDER_WORD_REGEX='aol\.com|fastmail|gmail|gmx|hotmail|icloud|outlook|pm\.me|proton|tutanota|yahoo|yandex|zoho'

# Merge leftovers (#338): the backups a hand merge or `patch` leaves next to
# a file, `<file>.orig` and `<file>.rej`, and git mergetool's copies
# `<name>_BACKUP_<pid>[.<ext>]` (and _BASE_, _LOCAL_, _REMOTE_). The one
# list that pre-commit and the development gate use; .gitignore ignores the same.
MERGE_LEFTOVER_REGEX='(^|/)[^/]*\.(orig|rej)$|(^|/)[^/]*_(BACKUP|BASE|LOCAL|REMOTE)_[0-9]+(\.[^/]*)?$'

# --- merge_leftovers / tracked_merge_leftovers ----------------------------
# merge_leftovers reads path names, one per line, and prints the merge
# leftovers among them. tracked_merge_leftovers prints the tracked ones:
# the development gate's backstop for a commit made without the hooks.
merge_leftovers() {
    grep -E "$MERGE_LEFTOVER_REGEX" || true
}
tracked_merge_leftovers() {
    git ls-files -z | tr '\000' '\n' | merge_leftovers
}

# --- allowed_email_regex --------------------------------------------------
# Prints a single ERE alternation built from .githooks/allowed-emails
# (blank lines and '#' comments ignored). If the file is missing or empty,
# prints a regex that matches nothing, i.e. fail closed.
allowed_email_regex() {
    if [ ! -f "$ALLOWED_EMAILS_FILE" ]; then
        # No allowlist file: fail closed with a regex that matches nothing.
        printf 'a^'
        return
    fi
    joined=$(awk '
        {
            sub(/#.*/, "")
            gsub(/^[ \t]+/, "")
            gsub(/[ \t]+$/, "")
            if (length($0) == 0) next
            printf "(%s)|", $0
        }
    ' "$ALLOWED_EMAILS_FILE")
    joined=${joined%|}
    if [ -z "$joined" ]; then
        printf 'a^'
    else
        printf '%s' "$joined"
    fi
}

# --- email_is_disallowed <email> -----------------------------------------
# Returns 0 (true, i.e. "disallowed") if the single email address given
# is on a blocked domain, or does not match any allowed pattern.
# Returns 1 (false, i.e. "allowed") otherwise.
email_is_disallowed() {
    addr=$1
    if printf '%s\n' "$addr" | grep -Eqi "$BLOCKED_DOMAIN_REGEX"; then
        return 0
    fi
    allow_re=$(allowed_email_regex)
    if printf '%s\n' "$addr" | grep -Eq "$allow_re"; then
        return 1
    fi
    return 0
}

# --- _is_placeholder_domain <email> --------------------------------------
# Documentation / example addresses that are always safe to leave in
# content (README examples, test fixtures, etc.), independent of the
# allowlist. Never bypasses BLOCKED_DOMAIN_REGEX (callers must check that
# first).
_is_placeholder_domain() {
    printf '%s\n' "$1" | grep -Eq '@([A-Za-z0-9.-]*\.)?example\.(com|org)$|@lodi\.example$|noreply'
}

# --- scan_text_for_emails -------------------------------------------------
# Reads text on stdin, prints (one per line) every email-like token that is
# NOT allowed: either it is on a blocked domain (always rejected), or it
# fails to match an allowed pattern and isn't a recognised placeholder
# domain (example.com/example.org/lodi.example/*noreply*).
scan_text_for_emails() {
    grep -Eo "$EMAIL_TOKEN_REGEX" 2>/dev/null | while IFS= read -r token; do
        if printf '%s\n' "$token" | grep -Eqi "$BLOCKED_DOMAIN_REGEX"; then
            printf '%s\n' "$token"
            continue
        fi
        if _is_placeholder_domain "$token"; then
            continue
        fi
        if email_is_disallowed "$token"; then
            printf '%s\n' "$token"
        fi
    done
}

# --- scan_text_for_private_paths -------------------------------------------
# Reads text on stdin, prints (one per line) every token that AGENTS.md §1.6
# blocks besides an email address: a home-directory path (/home/<login>/)
# or one of the provider words, case-insensitively. This is the same
# expression the development gate runs over the whole tree; the hook runs it over
# what is being committed so that the gate is not the first thing to notice.
PRIVATE_TOKEN_REGEX="/home/[a-z][a-z0-9_-]*/|$PROVIDER_WORD_REGEX"

scan_text_for_private_paths() {
    grep -Eio "$PRIVATE_TOKEN_REGEX" 2>/dev/null
}

# --- scan_name_for_private_tokens ------------------------------------------
# The same, for a repository path (one per line on stdin). A repository path
# is relative, so it never holds an absolute /home/<login>/ path; what it can
# hold is a directory named after the committer's login, such as a fixture
# copied from a real home directory (`fixtures/home/<login>/.bashrc`). That
# and the provider words are what this looks for; `home/<name>/` with any
# other name (`tests/fixtures/home/several/`) is an ordinary path.
scan_name_for_private_tokens() {
    login=$(id -un 2>/dev/null)
    if [ -n "$login" ]; then
        grep -Eio "(^|/)home/$login/|$PROVIDER_WORD_REGEX" 2>/dev/null
    else
        grep -Eio "$PROVIDER_WORD_REGEX" 2>/dev/null
    fi
}

# --- is_policy_file <path> -------------------------------------------------
# The files that hold the policy's own patterns and fixtures, which
# the development gate excludes from its sanitization check for the same reason.
is_policy_file() {
    case "$1" in
        # export-public: allow the next line
        AGENTS.md|.githooks/*|scripts/gate.sh|scripts/test-hooks.sh) return 0 ;;
        *) return 1 ;;
    esac
}

# --- identity_enforced -------------------------------------------------------
# The owner's own clones pin the identity and the landing rules (#572): they
# opt in with `git config lodi.enforceIdentity true`, which
# `scripts/install-hooks.sh --owner` and the maintainers' lane setup set. A
# clone without it (a contributor's) keeps every content check but commits
# and pushes under its own identity.
identity_enforced() {
    [ "$(git config --bool lodi.enforceIdentity 2>/dev/null)" = "true" ]
}

# --- check_identity --------------------------------------------------------
# Returns 0 at once unless identity_enforced. Otherwise verifies both the author and committer identity resolve to an allowed
# email address (honouring GIT_AUTHOR_EMAIL / GIT_COMMITTER_EMAIL
# overrides, same as git itself). Prints a clear error and returns 1 if
# either identity is not allowed.
check_identity() {
    identity_enforced || return 0
    author_ident=$(git var GIT_AUTHOR_IDENT 2>/dev/null)
    committer_ident=$(git var GIT_COMMITTER_IDENT 2>/dev/null)

    default_author_email=$(printf '%s\n' "$author_ident" | sed -n 's/.*<\(.*\)>.*/\1/p')
    default_committer_email=$(printf '%s\n' "$committer_ident" | sed -n 's/.*<\(.*\)>.*/\1/p')

    author_email=${GIT_AUTHOR_EMAIL:-$default_author_email}
    committer_email=${GIT_COMMITTER_EMAIL:-$default_committer_email}

    bad=0

    if [ -z "$author_email" ] || email_is_disallowed "$author_email"; then
        echo "ERROR: author email '$author_email' is not an allowed identity for this repository." >&2
        bad=1
    fi
    if [ -z "$committer_email" ] || email_is_disallowed "$committer_email"; then
        echo "ERROR: committer email '$committer_email' is not an allowed identity for this repository." >&2
        bad=1
    fi

    if [ "$bad" -ne 0 ]; then
        cat >&2 <<'EOF'

Only one identity may author or commit in this repository:
    name:  stshow
    email: 33067247+stshow@users.noreply.github.com

Fix it with:
    git config user.name  stshow
    git config user.email 33067247+stshow@users.noreply.github.com
EOF
        return 1
    fi
    return 0
}
