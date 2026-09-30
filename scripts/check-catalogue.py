#!/usr/bin/env python3
"""The recipe-catalogue lint (M-0.4 T-1, LD-76, LD-79, LD-90).

`catalogue/` is the recipe catalogue: data the build script embeds, described normatively in
`catalogue/README.md`. It is under the repository's own GPL-3.0-or-later `LICENSE` like every
other file here (M-1.0 R-6). This script is the deterministic, offline check that the directory
still obeys that document. It uses `python3` and the standard library only: no cargo, no network.

One rule per row, each named in its own failure line:

  licence            one licence for the whole repository: the root `LICENSE` is the GNU GPL,
                     `catalogue/LICENSE` and `docs/licenses/RECIPES-MIT.txt` do not exist, and no
                     `catalogue/**/*.toml` carries an SPDX header or names a second licence
  no-strip           `build.rs` embeds a catalogue file verbatim: it strips no header line, so
                     the digest of a recipe is the digest of the file
  name-stem          every `catalogue/tools/*.toml` has `[recipe] name` equal to its file's stem
                     (`spec/06` §2.2)
  description        `description` and `homepage` are present and non-empty — `lodi search` and
                     `lodi info` print them
  https              every URL and URL template begins `https://` (`spec/06` §6)
  hash-source        a hash source is declared: `checksum_file`, `checksum_sidecar`, or a
                     strategy that supplies the digest itself (`spec/05` §3.3)
  strategy-known     the `[versions] strategy` value appears in `catalogue/README.md` **and** in
                     the registration list of `src/upstream/strategy/mod.rs`
  strategy-digest-flag
                     the strategies this lint believes carry their own digest are exactly the
                     ones whose module says `supplies_digest: true`, so `hash-source` can never
                     be laxer than the recipe parser
  no-version-literal no line of a tool recipe outside a comment matches `[0-9]+\\.[0-9]+`
                     (OD-04/OD-08: no version list, anywhere)
  embed-set          the set of `*.toml` files is exactly what `build.rs` would embed — a regular
                     file, a `.toml` name, a valid package-name stem and nothing else
  not-executable     nothing under `catalogue/` is executable

`no-version-literal` is a **tool-recipe** rule. A base definition names release aliases
(`aliases = ["12"]`, `["24.04"]`) and a `snapshot_min` timestamp: those are identities of a
distribution release, not a version list of an upstream tool, and `catalogue/catalogue.toml`
carries a `min_lodi_version` for the same reason (LD-136). The rules above say for each which
files they read.

`--self-test` is the negative control: it copies this tree into a scratch directory, crafts one
violation per rule, and fails unless each is rejected by its own rule and the untouched copy
passes. The development gate runs it.
"""

import argparse
import os
import re
import shutil
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The catalogue carries no per-file licence header (M-1.0 R-6). These are the marks of one
# coming back, in a file or in the build script that would strip it.
SPDX = "SPDX-License-Identifier"

# Strategies that carry the artifact's digest themselves, so a recipe using one needs no
# `checksum_file`. `github_assets` and `json_index` do not: they read a published checksum file,
# and `github_assets` only cross-checks the release's own digest against it. A strategy joins
# this set in the package that implements it, together with its row in `catalogue/README.md` and
# its `supplies_digest: true` in `src/upstream/strategy/mod.rs`, which is what the recipe parser
# reads; this set is this lint's copy of that flag and `strategy-digest-flag` below keeps the two
# from drifting apart.
STRATEGIES_WITH_OWN_DIGEST = frozenset({"adoptium", "github_releases"})

# The keys an `[asset]` (or one `[[asset]]` of several) may state its digest source with. A
# recipe declares exactly one of them per artifact, or uses a strategy that supplies the digest
# itself; `checksum = "index"` means the discovery document lists the digest for the file it
# matched (M-0.4 T-4).
HASH_KEYS = ("checksum_file", "checksum_sidecar", "checksum")
PACKAGE_NAME = re.compile(r"^[a-z0-9][a-z0-9._+-]{0,127}$")
VERSION_LITERAL = re.compile(r"[0-9]+\.[0-9]+")
KINDS = ("tools", "bases")


def problem(problems, rule, where, detail):
    problems.append(f"{rule}: {where}: {detail}")


def registered_strategies(source):
    """The names in the STRATEGIES registration list of src/upstream/strategy/mod.rs."""
    text = source.read_text(encoding="utf-8")
    marker = "pub const STRATEGIES: &[(&str, Strategy)] = &["
    start = text.find(marker)
    if start < 0:
        return None
    body = text[start + len(marker):]
    end = body.find("];")
    if end < 0:
        return None
    return [m.group(1) for m in re.finditer(r'\(\s*"([a-z0-9_]+)"\s*,', body[:end])]


def strategies_with_own_digest(directory):
    """The strategy modules whose `STRATEGY` record says `supplies_digest: true`. It is the flag
    `src/catalogue/recipe.rs` reads, so reading it here keeps this lint and the parser together;
    a module file's stem is the strategy's registered name."""
    found = set()
    for path in sorted(directory.glob("*.rs")):
        if path.name == "mod.rs":
            continue
        text = path.read_text(encoding="utf-8")
        start = text.find("pub const STRATEGY: Strategy = Strategy {")
        if start < 0:
            continue
        end = text.find("};", start)
        if re.search(r"supplies_digest\s*:\s*true", text[start:end]):
            found.add(path.stem)
    return found


def urls_in(value):
    """Every string in a parsed TOML value that looks like a URL."""
    if isinstance(value, str):
        return [value] if "://" in value else []
    if isinstance(value, dict):
        return [u for v in value.values() for u in urls_in(v)]
    if isinstance(value, list):
        return [u for v in value for u in urls_in(v)]
    return []


def check_directory(catalogue, problems):
    """embed-set and not-executable: the directory holds what build.rs accepts, and no more."""
    files = {}
    for kind in KINDS:
        directory = catalogue / kind
        files[kind] = []
        if not directory.is_dir():
            problem(problems, "embed-set", f"catalogue/{kind}", "is missing")
            continue
        for entry in sorted(directory.iterdir()):
            where = f"catalogue/{kind}/{entry.name}"
            if not entry.is_file() or entry.is_symlink():
                problem(problems, "embed-set", where,
                        "is not a regular file; build.rs refuses it")
                continue
            if entry.suffix != ".toml":
                problem(problems, "embed-set", where,
                        "is not a .toml file; build.rs refuses it")
                continue
            if not PACKAGE_NAME.match(entry.stem):
                problem(problems, "embed-set", where,
                        f"`{entry.stem}` is not a valid package name")
                continue
            files[kind].append(entry)
        if not files[kind]:
            problem(problems, "embed-set", f"catalogue/{kind}", "holds no recipe")
    for path in sorted(catalogue.rglob("*")):
        if path.is_file() and os.access(path, os.X_OK):
            problem(problems, "not-executable",
                    f"catalogue/{path.relative_to(catalogue)}", "is executable")
    return files


def check_licence(where, text, problems):
    """No catalogue file claims a licence of its own; the repository's LICENSE covers it."""
    ok = True
    if SPDX in text:
        problem(problems, "licence", where,
                f"carries an {SPDX} header; the catalogue is under the repository's LICENSE")
        ok = False
    for line in text.splitlines():
        if "MIT" in line:
            problem(problems, "licence", where,
                    f"names a second licence: {line.strip()}")
            ok = False
    return ok


def check_recipe(path, where, text, strategies, readme, problems):
    """Every tool-recipe rule, over one catalogue/tools/*.toml."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        problem(problems, "name-stem", where, f"is not valid TOML: {e}")
        return
    recipe = data.get("recipe", {})
    if recipe.get("name") != path.stem:
        problem(problems, "name-stem", where,
                f"[recipe] name is {recipe.get('name')!r}, not the file's stem {path.stem!r}")
    for key in ("description", "homepage"):
        if not str(recipe.get(key, "")).strip():
            problem(problems, "description", where, f"[recipe] {key} is missing or empty")

    strategy = data.get("versions", {}).get("strategy")
    if strategy is None:
        problem(problems, "strategy-known", where, "[versions] strategy is missing")
    else:
        if strategies is not None and strategy not in strategies:
            problem(problems, "strategy-known", where,
                    f"strategy `{strategy}` is not in the STRATEGIES list of "
                    "src/upstream/strategy/mod.rs")
        if f"`{strategy}`" not in readme:
            problem(problems, "strategy-known", where,
                    f"strategy `{strategy}` is not documented in catalogue/README.md")

    # One `[asset]` table, or several `[[asset]]` entries: every artifact needs its own hash
    # source, so the rule is checked once per artifact rather than once per recipe.
    assets = data.get("asset", {})
    assets = assets if isinstance(assets, list) else [assets]
    for nth, asset in enumerate(assets):
        if not isinstance(asset, dict):
            problem(problems, "hash-source", where, f"[asset] {nth} is not a table")
            continue
        declared = [k for k in HASH_KEYS if str(asset.get(k, "")).strip()]
        if not declared:
            if strategy not in STRATEGIES_WITH_OWN_DIGEST:
                problem(problems, "hash-source", where,
                        f"artifact {nth} declares no hash source: [asset] needs one of "
                        f"{', '.join(HASH_KEYS)}, or a strategy that supplies the digest itself")
        elif strategy in STRATEGIES_WITH_OWN_DIGEST:
            # One question, one answer: where the API is the hash source, a second one in the
            # recipe is refused rather than cross-checked. The recipe parser refuses it too.
            problem(problems, "hash-source", where,
                    f"artifact {nth} declares {', '.join(declared)} under `{strategy}`, which "
                    "supplies the artifact's digest itself")

    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if VERSION_LITERAL.search(line):
            problem(problems, "no-version-literal", where,
                    f"looks like a version list (OD-04/OD-08): {stripped}")


def check_urls(where, text, problems):
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return  # reported by the rule that parses it
    for url in urls_in(data):
        if not url.startswith("https://"):
            problem(problems, "https", where, f"{url} is not an HTTPS URL")


def check(root):
    """Every rule over one tree; returns the problems, each prefixed with its rule."""
    problems = []
    catalogue = root / "catalogue"
    if not catalogue.is_dir():
        return [f"embed-set: catalogue: is missing under {root.name}"]
    for gone, why in ((catalogue / "LICENSE", "the catalogue has no licence of its own"),
                      (root / "docs" / "licenses" / "RECIPES-MIT.txt",
                       "the boundary it recorded no longer exists")):
        if gone.exists():
            problem(problems, "licence", str(gone.relative_to(root)),
                    f"exists: {why} — the whole repository is GPL-3.0-or-later")
    repo_licence = root / "LICENSE"
    if not repo_licence.is_file() or "GNU GENERAL PUBLIC LICENSE" not in repo_licence.read_text(
            encoding="utf-8"):
        problem(problems, "licence", "LICENSE",
                "is not the GNU General Public License text this repository is under")

    # build.rs embeds a catalogue file verbatim. A header line it stripped would leave the digest
    # of a recipe unequal to the digest of the file, which is what users reproduce by hand.
    build_rs = (root / "build.rs").read_text(encoding="utf-8")
    if SPDX in build_rs or "strip_prefix(HEADER)" in build_rs:
        problem(problems, "no-strip", "build.rs",
                "strips a licence header before embedding; a catalogue file is embedded as it is")

    strategy_dir = root / "src" / "upstream" / "strategy"
    declared = strategies_with_own_digest(strategy_dir)
    if declared != set(STRATEGIES_WITH_OWN_DIGEST):
        problem(problems, "strategy-digest-flag", "src/upstream/strategy",
                f"the modules saying `supplies_digest: true` are {sorted(declared)}, but this "
                f"lint's STRATEGIES_WITH_OWN_DIGEST is {sorted(STRATEGIES_WITH_OWN_DIGEST)}")
    strategies = registered_strategies(strategy_dir / "mod.rs")
    if strategies is None:
        problem(problems, "strategy-known", "src/upstream/strategy/mod.rs",
                "has no STRATEGIES registration list to read")
    readme_path = catalogue / "README.md"
    readme = readme_path.read_text(encoding="utf-8") if readme_path.is_file() else ""
    if not readme:
        problem(problems, "strategy-known", "catalogue/README.md", "is missing or empty")

    files = check_directory(catalogue, problems)
    for path in sorted(catalogue.glob("*.toml")) + files["tools"] + files["bases"]:
        where = f"catalogue/{path.relative_to(catalogue)}"
        text = path.read_text(encoding="utf-8")
        check_licence(where, text, problems)
        check_urls(where, text, problems)
        if path.parent.name == "tools":
            check_recipe(path, where, text, strategies, readme, problems)
    return problems


# ------------------------------------------------------------------------------------------
# The negative control


def self_test():
    failures = 0
    with tempfile.TemporaryDirectory(prefix="lodi-check-catalogue.") as tmp:
        sound = Path(tmp) / "sound"
        (sound / "src" / "upstream").mkdir(parents=True)
        (sound / "docs" / "licenses").mkdir(parents=True)
        shutil.copytree(ROOT / "catalogue", sound / "catalogue")
        shutil.copy(ROOT / "build.rs", sound / "build.rs")
        shutil.copy(ROOT / "LICENSE", sound / "LICENSE")
        shutil.copytree(ROOT / "src" / "upstream" / "strategy",
                        sound / "src" / "upstream" / "strategy")

        recipe = sorted((sound / "catalogue" / "tools").glob("*.toml"))[0].name
        # `hash-source` needs a recipe that really declares a checksum file: a recipe whose
        # strategy supplies the digest has none to remove, and removing nothing proves nothing.
        with_checksum = [p for p in sorted((sound / "catalogue" / "tools").glob("*.toml"))
                         if re.search(r"(?m)^checksum_file", p.read_text(encoding="utf-8"))]
        if not with_checksum:
            print("  FAIL no recipe declares a checksum file, so `hash-source` cannot be tested")
            return 1
        hash_recipe = with_checksum[0].name
        digest_modules = sorted(strategies_with_own_digest(
            sound / "src" / "upstream" / "strategy"))
        if not digest_modules:
            print("  FAIL no strategy says `supplies_digest: true`, so "
                  "`strategy-digest-flag` cannot be tested")
            return 1
        digest_module = digest_modules[0] + ".rs"

        def break_licence_header(root):
            path = root / "catalogue" / "tools" / recipe
            path.write_text(f"# {SPDX}: MIT\n" + path.read_text(), encoding="utf-8")

        def break_licence_file(root):
            (root / "catalogue" / "LICENSE").write_text("MIT License\n", encoding="utf-8")

        def break_licence_root(root):
            (root / "LICENSE").write_text("MIT License\n", encoding="utf-8")

        def break_no_strip(root):
            path = root / "build.rs"
            path.write_text(
                f'const HEADER: &str = "# {SPDX}: MIT";\n' + path.read_text(), encoding="utf-8")

        def break_name_stem(root):
            path = root / "catalogue" / "tools" / recipe
            path.write_text(path.read_text().replace(
                f'name        = "{recipe[:-5]}"', 'name        = "elsewhere"'), encoding="utf-8")

        def break_description(root):
            path = root / "catalogue" / "tools" / recipe
            path.write_text(re.sub(r"(?m)^description = .*$", 'description = ""',
                                   path.read_text()), encoding="utf-8")

        def break_https(root):
            path = root / "catalogue" / "tools" / recipe
            path.write_text(path.read_text().replace("https://", "http://", 1), encoding="utf-8")

        def break_hash_source(root):
            path = root / "catalogue" / "tools" / hash_recipe
            path.write_text(re.sub(r"(?m)^checksum_file .*$", "", path.read_text()),
                            encoding="utf-8")

        def break_hash_source_doubled(root):
            """A recipe whose strategy is the hash source, given a second one."""
            for path in sorted((root / "catalogue" / "tools").glob("*.toml")):
                text = path.read_text(encoding="utf-8")
                if any(f'"{s}"' in text for s in STRATEGIES_WITH_OWN_DIGEST):
                    path.write_text(
                        text.replace("[spec]", 'checksum_file = "https://example.invalid/s"\n\n'
                                     "[spec]", 1), encoding="utf-8")
                    return

        def break_strategy_digest_flag(root):
            path = root / "src" / "upstream" / "strategy" / digest_module
            path.write_text(path.read_text().replace("supplies_digest: true",
                                                     "supplies_digest: false", 1),
                            encoding="utf-8")

        def break_strategy_known(root):
            path = root / "catalogue" / "tools" / recipe
            path.write_text(re.sub(r'(?m)^strategy( *)= ".*"$', r'strategy\1= "not_a_strategy"',
                                   path.read_text()), encoding="utf-8")

        def break_no_version_literal(root):
            path = root / "catalogue" / "tools" / recipe
            path.write_text(path.read_text().replace("[spec]", 'versions = ["3.12"]\n\n[spec]'),
                            encoding="utf-8")

        def break_embed_set(root):
            (root / "catalogue" / "tools" / "notes.txt").write_text("stray\n", encoding="utf-8")

        def break_embed_set_name(root):
            (root / "catalogue" / "tools" / "Not-A-Name.toml").write_text(
                "[recipe]\n", encoding="utf-8")

        def break_not_executable(root):
            path = root / "catalogue" / "tools" / recipe
            path.chmod(path.stat().st_mode | 0o111)

        cases = [
            ("a recipe carrying a licence header of its own", break_licence_header, "licence"),
            ("a licence file inside the catalogue", break_licence_file, "licence"),
            ("a repository licence that is not the GPL", break_licence_root, "licence"),
            ("a build.rs that strips a licence header", break_no_strip, "no-strip"),
            ("a recipe whose name is not its file's stem", break_name_stem, "name-stem"),
            ("a recipe with an empty description", break_description, "description"),
            ("a recipe with a plain-HTTP URL", break_https, "https"),
            ("a recipe with no hash source", break_hash_source, "hash-source"),
            ("a recipe with a second hash source", break_hash_source_doubled, "hash-source"),
            ("a recipe naming an unregistered strategy", break_strategy_known, "strategy-known"),
            ("a strategy that stops supplying its own digest", break_strategy_digest_flag,
             "strategy-digest-flag"),
            ("a recipe carrying a version literal", break_no_version_literal,
             "no-version-literal"),
            ("a file that is not a recipe", break_embed_set, "embed-set"),
            ("a recipe whose stem is not a package name", break_embed_set_name, "embed-set"),
            ("an executable file in the catalogue", break_not_executable, "not-executable"),
        ]

        problems = check(sound)
        ok = not problems
        print(("  ok   " if ok else "  FAIL ") + "the committed catalogue passes")
        if not ok:
            failures += 1
            for p in problems:
                print("         " + p)

        for name, mutate, rule in cases:
            broken = Path(tmp) / ("broken-" + rule + "-" + str(cases.index((name, mutate, rule))))
            shutil.copytree(sound, broken)
            mutate(broken)
            problems = check(broken)
            ok = any(p.startswith(rule + ":") for p in problems)
            print(("  ok   " if ok else "  FAIL ") + f"{name} is rejected by `{rule}`")
            if not ok:
                failures += 1
                print("         got: " + ("; ".join(problems) or "no problem at all"))
            shutil.rmtree(broken, ignore_errors=True)

    print("check-catalogue: self-test OK" if not failures
          else f"check-catalogue: self-test FAILED ({failures})")
    return 0 if not failures else 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--self-test", action="store_true",
                    help="run the negative controls instead of checking this tree")
    args = ap.parse_args()
    if args.self_test:
        return self_test()

    problems = check(ROOT)
    if problems:
        for p in problems:
            print("check-catalogue: FAIL " + p)
        return 1
    tools = len(list((ROOT / "catalogue" / "tools").glob("*.toml")))
    bases = len(list((ROOT / "catalogue" / "bases").glob("*.toml")))
    print(f"check-catalogue: OK — {tools} tool recipes and {bases} base definitions under "
          "catalogue/, all GPL-3.0-or-later, none naming a version")
    return 0


if __name__ == "__main__":
    sys.exit(main())
