# Email-leak-prevention git hooks

These hooks stop the repository owner's personal email address (or anyone
else's) from ever entering this repository's history.

## Two modes

- **Contributor** (the default): every content check below runs; the commit
  identity and the landing rules are not checked, so a contributor commits
  under their own identity and pushes to their own fork.
- **Owner**: a clone with `git config lodi.enforceIdentity true` also pins the
  identity and applies the landing rules. `scripts/install-hooks.sh --owner`
  sets it, and the maintainers' lane setup does too and refuses a lane
  without it; their gate checks the identity only when it is set.

## What is enforced

- **Identity** (owner mode only): the only allowed author/committer identity is
  `stshow <33067247+stshow@users.noreply.github.com>`. Any other
  `user.email` (or `GIT_AUTHOR_EMAIL`/`GIT_COMMITTER_EMAIL`) is rejected,
  by `pre-commit` and, for every commit being pushed, by `pre-push`.
- **Content** (both modes): staged file names, staged added lines, commit messages, and
  (at push time) every commit not yet on the remote are scanned for
  email-like tokens. Tokens on the common freemail domains, listed in
  alphabetical order in `BLOCKED_DOMAIN_REGEX` in `lib.sh` (`aol.com`,
  `fastmail.com`, `gmail.com`, `gmx.{com,de,net}`, `hotmail.com`,
  `icloud.com`, `live.com`, `mac.com`, `mail.ru`, `me.com`, `msn.com`,
  `outlook.com`, `pm.me`, `proton.me`, `protonmail.{ch,com}`,
  `tutanota.com`, `yahoo.com`, `yandex.{com,ru}`, `zoho.com`), are always
  rejected. Anything else must match `.githooks/allowed-emails`.
- **Provider words**: staged file names and added lines (outside the
  policy's own files) are also scanned for the provider words AGENTS.md
  §1.6 names. `PROVIDER_WORD_REGEX` in `lib.sh` is the one list, used by
  `pre-commit` and the development gate alike.
- Enforced by `pre-commit`, `commit-msg`, and `pre-push`.
- **The gate's cheap checks** (`pre-commit`): the changed-line
  style guard, the host-scope safety scan and the documentation check of
  the maintainers' gate run in their `--staged` mode on the staged
  content, each only when a staged path is in its scope, and refuse the
  commit with the gate's own message. Unstaged changes are not checked;
  the gate still runs all three as the backstop. In a clone without those
  scripts the checks are skipped.
- **Merge leftovers** (`pre-commit`): a staged `*.orig`, `*.rej` or git
  mergetool backup (`<name>_BACKUP_<pid>`, `_BASE_`, `_LOCAL_`, `_REMOTE_`)
  is refused, naming `git rm --cached` for each; a deletion commits.
  `MERGE_LEFTOVER_REGEX` in `lib.sh` is the one list; the development gate
  refuses a tracked one as the backstop, and `.gitignore` ignores them.
- **Prevention record** (`commit-msg`): a message that carries any
  of `Cause:`, `Recurs:`, `Prevention:`, `Fingerprint:` at the start of a
  line carries all four, once each and well-formed (the grammar of
  maintainers' failure-record tool, whose self-test runs this hook). A
  message without them passes: whether a fix needs a record is decided at
  landing, by the issue's `kind:failure` label.
- **Landing** (`pre-push`, owner mode only): a push to `main` is refused unless the
  landing command makes it under the landing lock (`LODI_LANDING_LOCK_HELD`),
  and a `claude/` branch behind `main` is refused (`-wip` branches excepted).
  Both refusals name the landing command (`LANDING_COMMAND` in the hook).

## Install

    scripts/install-hooks.sh            # a contributor's clone
    scripts/install-hooks.sh --owner    # the owner's clones

This sets `core.hooksPath` to `.githooks`, makes the hooks executable and
sets `mergetool.keepBackup` to `false`, so git mergetool leaves no `.orig`.
With `--owner` (or in a clone that already has `lodi.enforceIdentity=true`)
it also sets the repository-level no-reply identity and
`lodi.enforceIdentity=true`. The maintainers' hook test covers both modes.

## Adding an allowed pattern

Add a POSIX ERE (one per line, `#` for comments) to
`.githooks/allowed-emails`, e.g. for a new noreply-style service. Never add
a pattern that could match a real personal address.

**Never add the owner's real email address to `allowed-emails`, any hook,
any test fixture, or any other file in this repo — under any name, split
up, obfuscated, or otherwise.**

## Bypass

These hooks are a safety net, not the only control. `--no-verify` is
forbidden by project policy: do not use it to skip these checks.
