# AGENTS.md — instructions for every agent and person working in this repository

`CLAUDE.md` points here. Section 1 is the project's privacy policy, the same text in every
repository of the project. It is mandatory, and it overrides everything else in this file.

## 1. Privacy (mandatory, overrides everything else)

The owner's privacy is preserved in this repository, in every repository created from it, and in
everything an agent does on the owner's behalf. These rules are not advisory.

### 1.1 Identity

- The only allowed git author and committer identity is `stshow <33067247+stshow@users.noreply.github.com>`. It is set at repository level by `scripts/install-hooks.sh`; confirm `git config user.email` prints it before every commit.
- The owner's real email address is a secret. Never write it into any file, commit, message, allowlist, fixture, log, prompt, or remote service. Never look it up, ask for it, split it, encode it, or "test" with it. If it appears anywhere, stop and report; do not copy it around while fixing.
- Never use `--no-verify` or any other hook bypass. The hooks in `.githooks/` (`pre-commit`, `commit-msg`, `pre-push`) are a safety net, not the control.
- No attribution trailers, no personal names beyond the handle `stshow`, no `Co-authored-by` lines.

### 1.2 Content that must never enter a repository

- Personal data of any kind; email addresses other than no-reply identities matched by `.githooks/allowed-emails`.
- Home-directory paths, user names, machine hostnames, internal hostnames, customer identifiers.
- Transcripts of any assistant (Claude Code, Pi, others), raw JSON event streams, session logs, and identifiers that link to an account or conversation. They embed working directories and are never committed; `.gitignore` covers the known locations. Verbatim reviewer text is kept as Markdown instead.
- Credentials, tokens, `auth.json` files, API keys, SSH keys, real `authorized_keys` entries.

### 1.3 Nothing is shared or published by default

- Never publish an artifact, page, document, Slack message, or file from repository or session content, and never send a file to the owner through an external service, unless the owner explicitly asks for that specific item in that conversation. A general "share it" in one context does not carry to the next.
- Never make a repository public, add a collaborator, create a remote outside the owner's own account, or change repository visibility. New repositories are private until the owner says otherwise.
- Never send repository content to a service that is not needed for the task at hand.

### 1.4 What may leave the machine, and how

- Only a sanitized packet or prompt that the agent wrote deliberately for the purpose, after running the check in §1.6, and after stating in the conversation what will be sent and to which provider.
- External model reviewers get **no tools** (no file, shell, or network access), a custom system prompt that replaces the tool's default so that no working directory is transmitted, and discovery of extensions, skills, context files and prompt templates disabled.
- Fact-checking agents may use public web search and fetch on public documentation only; they never receive repository content beyond the specific claim to verify.
- Any request that a subagent sends to a paid provider is disclosed in the final report, including failed probes.

### 1.5 Every new repository starts private and protected

Before the first commit in any repository created for this project (implementation, registry, tooling):

1. Copy `.githooks/` (all hooks, `lib.sh`, `allowed-emails`, `README.md`) and `scripts/install-hooks.sh`; run the installer; confirm `core.hooksPath` is `.githooks`.
2. Copy §1 of this file verbatim into that repository's `AGENTS.md` (use `templates/AGENTS.md`), and add a `CLAUDE.md` that points to it.
3. Copy the session-transcript patterns from `.gitignore`.
4. Set the repository-level identity to the no-reply identity.
5. Create the remote only under the owner's own account, private.

An agent that cannot complete these steps does not commit.

### 1.6 Sanitization check (run before any commit and before anything leaves the machine)

```sh
grep -rn -i -E 'aol\.com|fastmail|gmail|gmx|hotmail|icloud|outlook|pm\.me|proton|tutanota|yahoo|yandex|zoho|/home/[a-z][a-z0-9_-]*/|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[a-z]{2,}' <paths> \
  | grep -v 'users\.noreply\.'
```

Any hit that is not a no-reply identity blocks the action. Also search for the owner's login name and
machine hostnames if the agent knows them; do not write those names into this file.

### 1.7 Subagents

Every spawned agent is told: the allowed identity, that the real address must never be looked up or
written, that it must not send project content to unrelated services, and that it must not modify
files unless the task says so. Agents that commit are told to run `scripts/install-hooks.sh` first.

## 2. Contributing

- **Hooks.** Run `sh scripts/install-hooks.sh` once per clone. The hooks refuse personal data,
  secrets and merge leftovers. Never bypass them with `--no-verify`.
- **Build and test.** [docs/BUILDING.md](docs/BUILDING.md) says what you need and how to build.
  Before a pull request, `sh scripts/check.sh` must pass: formatting, clippy with warnings as
  errors, the build and every cargo test.
- **Test first.** A change of behaviour starts with a test that fails without it. Commit that
  test first, then the change that makes it pass. Tests assert behaviour, never the wording of
  a document.
- **Never run the host scope against your own machine.** `lodi host` commands change system
  packages and files as root. Tests run them only against a scratch `--root`, and
  `scripts/check.sh` sets `LODI_HOST_REQUIRE_ROOT=1`, which refuses any host command without
  one. Try a real apply only in a disposable virtual machine or container.
- **Style.** Default `rustfmt`, `cargo clippy --all-targets -- -D warnings` clean, and lines of
  at most 100 characters in Rust, Markdown and shell. User-facing pages are written for someone
  who has never seen how lodi is built: the command to type first, then what happens.
- **Pull requests.** One focused change each. Say what changed and why.
