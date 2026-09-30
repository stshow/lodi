# Contributing to lodi

Thanks for looking at lodi. This is the short version of how to build it, test it, and send a
pull request.

## Build and test

[docs/BUILDING.md](docs/BUILDING.md) says what you need, how to get the pinned Rust toolchain
(with `rustup` or Nix), and how to build and test. In short:

```sh
cargo build --locked
cargo test --locked
```

## Check before a pull request

```sh
sh scripts/check.sh
```

It runs `cargo fmt --check`, clippy with warnings as errors, the build and every test, and
prints `check: OK` when they all pass.

## Git hooks

Install the repository's git hooks. They stop personal data (email addresses, home-directory
paths, secrets) and merge leftovers from being committed or pushed:

```sh
sh scripts/install-hooks.sh
```

You commit under your own name and address. The hooks also have an owner mode, which pins the
project's own commit identity and landing rules. It is off unless a clone sets
`git config lodi.enforceIdentity true`, and contributors leave it off. See
[.githooks/README.md](.githooks/README.md) for what each mode checks.

## Sending a pull request

- Start a change of behaviour with a test that fails without it.
- Keep changes focused, and run `sh scripts/check.sh` before opening the PR. It should pass.
- Describe what changed and why in the PR description.
- Don't use `--no-verify` to bypass the git hooks; they're a safety net, not an obstacle.

## Decision ids in code comments

Some code comments cite a decision id (the letters `LD`, a hyphen and a number) or a path to a
milestone record. Those records are kept in lodi's private development repository. The comment
around each one says what was decided, so you don't need the record to follow the code.
Issue numbers in comments (a `#` followed by digits) also refer to that private development
repository, not to this repository's issue tracker.
