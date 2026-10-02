# The lodi recipe catalogue

This page tells you how to write a recipe, so that a tool name in `lodi.toml` becomes a
pinned, checked download. It is the full recipe format.

## What a recipe does for you

Write a tool name in `lodi.toml`:

```toml
[tools]
ripgrep = "latest"
```

Then open the project:

```sh
lodi develop
```

`lodi develop` locks the project first. It reads the recipe `catalogue/tools/ripgrep.toml`. It
asks GitHub which versions exist and picks the newest one your constraint allows. It writes the
exact download URL, the file's SHA-256 and the recipe's digest into `lodi.lock`.

It then downloads that URL and checks the bytes against the digest. A file that does not match
is never used. The lock alone decides the version, so a second machine with the same
lock gets the same bytes.

A recipe answers one question: where does this tool publish its releases, and how do you check
them? It never names a version. The catalogue check fails on a recipe that does.

The whole catalogue is under the same licence as lodi, GPL-3.0-or-later. No file carries a
licence header of its own.

## A recipe, key by key

This is `catalogue/tools/ripgrep.toml` without its opening comment. It uses `github_releases`,
the most common strategy:

```toml
[recipe]
name        = "ripgrep"
description = "Recursively search directories for a regex pattern, fast"
homepage    = "https://github.com/BurntSushi/ripgrep"

[versions]
strategy = "github_releases"
repo     = "BurntSushi/ripgrep"
tag      = "${version}"
asset    = "ripgrep-${version}-${arch}-unknown-linux-musl.tar.gz"

[asset]
format           = "tar.gz"
strip_components = 1

[spec]
bin  = ["rg"]
path = ["."]
```

- **`[recipe] name`** is the name you write in a manifest. It must equal the file name without
  `.toml`.
- **`description`** and **`homepage`** are what `lodi search` prints. Both are
  required, and the homepage uses HTTPS.
- **`[versions] strategy`** says how lodi finds versions. Each strategy has its own keys, listed
  in [Version strategies](#version-strategies).
- **`repo`** is the GitHub repository.
- **`tag`** is the pattern a release tag must match. The part in `${version}` is the version.
  ripgrep tags `14.1.1`, so the pattern is `${version}` and not the default `v${version}`.
- A tag that does not match, such as `nightly`, is skipped.
- **`asset`** is the file to take from the release. `${arch}` is `x86_64`.
- If upstream spells the architecture differently, add `[arch_names]` with `x86_64 = "x64"` and
  write `${arch_name}`.
- **`[asset] format`** is `tar.gz`, `tar.xz` or `binary`.
- **`strip_components = 1`** drops the one directory the tarball wraps everything in.
- **No `checksum_file`.** GitHub publishes each asset's SHA-256, so this strategy supplies the
  digest. A second hash source beside it is an error.
- An asset GitHub publishes with no digest is `E_NO_CHECKSUM`, and lodi does not download it.
- **`[spec] bin`** lists the programs the tool provides.
- **`[spec] path`** lists the folders added to `PATH`, relative to the unpacked files.
- ripgrep puts `rg` at the top, so `path` is `["."]`. Only executable files in a `path` folder
  are linked, so its `README` stays off `PATH`.

### A sidecar recipe

Not every upstream publishes on GitHub. `catalogue/tools/kubectl.toml` in full reads a one-line
text file for the version and a `.sha256` file for the digest:

```toml
[recipe]
name        = "kubectl"
description = "The Kubernetes command-line tool"
homepage    = "https://kubernetes.io/docs/reference/kubectl"

[versions]
strategy = "text_index"
url      = "https://dl.k8s.io/release/stable.txt"
line     = "v${version}"

[asset]
url              = "https://dl.k8s.io/release/v${version}/bin/linux/${arch_alt}/kubectl"
format           = "binary"
checksum_sidecar = ".sha256"

[spec]
bin  = ["bin/kubectl"]
path = ["bin"]
```

- **`text_index`** reads the document at `url`. Its one line, `v1.37.1`, matches `v${version}`,
  so the version is `1.37.1`. **A text index finds only the version upstream calls current.**
  `latest` works, and an older release is not found.
- **`[asset] url`** is rendered with that version, because `text_index` supplies no URL.
- **`checksum_sidecar = ".sha256"`** takes the digest from the file at the asset's URL plus
  `.sha256`. Locking fetches only that small file. `lodi develop` stops with an error if the
  download does not match it.
- **`format = "binary"`**: the download is the program itself, installed as `bin/kubectl`.

### Keys for harder downloads

From `catalogue/tools/rust.toml`, the first of its three downloads, and from
`catalogue/tools/jdk.toml`:

```toml
[[asset]]
url              = "https://static.rust-lang.org/dist/rustc-${version}-${arch_rust}.tar.gz"
format           = "tar.gz"
strip_components = 2
exclude          = ["manifest.in"]
checksum_sidecar = ".sha256"

[spec]
bin  = ["bin/java", "bin/javac"]
path = ["bin"]
env  = { JAVA_HOME = "${self.path}" }
```

- **`subdir`** is the folder inside the download to treat as its top.
- **`exclude`** lists exact file paths to drop after stripping. No globs, folders or `..`, no
  duplicates, and each one must exist.
- **`[spec] env`** sets variables when you enter the environment. Values may use `${self.path}`
  and `${self.version}` and nothing else.
- **Several `[[asset]]` entries** replace one `[asset]` when a tool needs several files.
- Each `[[asset]]` has its own hash source. lodi unpacks them in order into one place.

## Where the files go

```text
catalogue/README.md         this page: the recipe format
catalogue/catalogue.toml    [catalogue] schema, name, min_lodi_version
catalogue/tools/NAME.toml   one tool recipe. NAME is the tool name
catalogue/bases/DISTRO.toml one container base per distribution
```

The file name is the identity. `tools/python.toml` is the recipe `python`, and
`bases/debian.toml` is the base `debian`.

The build fails on any of these, and never skips them quietly:

- a file that does not end in `.toml`, or a folder inside `tools/` or `bases/`
- a name that does not match `^[a-z0-9][a-z0-9._+-]{0,127}$`
- a recipe whose `[recipe] name` is not its file name

Check everything a machine can check on this page with:

```sh
python3 -B scripts/check-catalogue.py
python3 -B scripts/check-catalogue.py --self-test
```

### The recipe digest

lodi builds each catalogue file into the binary exactly as it is. A lock records the SHA-256 of
the whole file, as `recipe.sha256` for a recipe or `base.rootfs.sourceSha256` for a base. You can
work it out yourself:

```sh
sha256sum catalogue/tools/python.toml
```

So editing a recipe changes the locks made with it. That is on purpose: the lock says which
recipe made it.

## The base files

`catalogue/bases/debian.toml`, `ubuntu.toml` and `arch.toml` are the container bases. A base
names each release, the repositories it installs from, and how its root file system is pinned:

```toml
[base]
distro = "debian"
family = "apt"                  # optional when the repository shape implies it

[release.bookworm]              # the table name is the codename
aliases      = ["12"]           # the other names `[container] release` accepts
snapshot_min = "…Z"             # earliest usable snapshot, YYYY-MM-DDTHH:MM:SSZ
repositories = ["debian"]       # names of [repository.*] tables below

[release.bookworm.rootfs.x86_64]
strategy = "github_oci_layout"  # or "dated_checksum_dir"

[repository.debian]
snapshot_url = "https://…/${snapshot_id}/"   # also `${snapshot_day}` and `${deb_arch}`
suites       = ["${release}"]
components   = ["main"]
```

- **`aliases`** are other names for the same release, not versions. So `["12"]` is allowed
  here, while a tool recipe may never name a version.
- **`versions_url`** is optional. It is an HTTPS pattern with `${name}`, `${release}` and
  `${deb_arch}`.
- It lists every published version of a package with its date and SHA-256. The host scope uses
  it to find a version in `[packages.pin]`.
- A release without `versions_url` pins by date only. Today only Ubuntu noble names one.
- **`github_oci_layout`** is an OCI layout on GitHub, pinned by git commit and manifest digest.
- **`dated_checksum_dir`** is a dated folder with a `sha256sum` file.
- It takes `date_format = "YYYYMMDD"`, the default, or `"YYYY.MM.DD"`. It also takes an optional
  relative `subdir` and an optional `package_list`.
- If the download name carries its date, write the literal `date_format` text in `layer`. lodi
  replaces it with the folder name.
- Apt repositories need `suites` and `components`. Pacman repositories leave both empty.
- `${snapshot_day}` is the snapshot's UTC day as `YYYY/MM/DD`.
- Any other rootfs strategy is `E_RECIPE_INVALID`.

## Format reference

### Hash sources

Every download has exactly one hash source. Two sources is an error, and so is none.

From `catalogue/tools/go.toml`, which reads the digest from Go's own index:

```toml
[asset]
url              = "https://go.dev/dl/go${version}.linux-${arch_alt}.tar.gz"
format           = "tar.gz"
strip_components = 1
checksum         = "index"
checksum_key     = "sha256"
```

| Key | Value | Where the digest comes from |
|---|---|---|
| `checksum_file` | a URL pattern | a `sha256sum` file that lists the download's file name |
| `checksum_sidecar` | a suffix, such as `".sha256"` | the file at `<url><suffix>` |
| | | a bare digest or one `sha256sum` line |
| `checksum` and `checksum_key` | `"index"` and a field name | the index entry for the file |
| | | the two keys go together |

- A sidecar that upstream does not publish is `E_NO_CHECKSUM`. lodi does not download the file.
- `github_releases` and `adoptium` supply the digest themselves. Such a recipe names no source.
- `github_assets` reads a digest from GitHub when there is one. lodi compares it with your
  `checksum_file`, and a difference is `E_HASH_MISMATCH`.
- A download whose bytes do not match is `E_HASH_MISMATCH`. Nothing is kept.
- lodi never downloads a file it cannot check. Every URL and URL pattern uses HTTPS.

### Pattern variables

From `catalogue/tools/python.toml`:

```toml
asset    = "cpython-${version}+${tag}-${arch_rust}-install_only.tar.gz"
```

You may use `version`, `version_major`, `version_minor`, `tag`, `arch`, `arch_alt` and
`arch_rust`. Use `arch_name` only with an `[arch_names]` table. Any other name is
`E_RECIPE_INVALID`.

There are no regular expressions. A pattern with one `${version}` matches its text before and
after, and takes the digits and dots in between.

### Version strategies

The `[versions] strategy` value must be one of these. lodi and the catalogue check stop on any
other. From `catalogue/tools/jdk.toml`:

```toml
[versions]
strategy   = "adoptium"
image_type = "jdk"
jvm_impl   = "hotspot"
```

| Strategy | Key or part | What it does |
|---|---|---|
| `adoptium` | versions | reads the GA releases of one Adoptium feature line |
| | `feature_version` | the line to read. Else the constraint's first number: `jdk = "21"` |
| | `latest` | the newest long-term support release Adoptium names, never early access |
| | order | by the release's `major`, `minor`, `security`, `patch` and `build` |
| | version | `version_data.semver` |
| | URL and SHA-256 | `package.link` and `package.checksum`, or else `E_NO_CHECKSUM` |
| | `image_type` | `jdk`, the default, or `jre` |
| | `jvm_impl` | `hotspot`. There is no other vendor and no fallback |
| `github_assets` | versions | the newest `releases` releases of `repo`, no drafts or pre-releases |
| | `asset` | takes the version from each file name. It must contain `${version}` |
| | URL and SHA-256 | from the release. Its `sha256:` digest is compared with `checksum_file` |
| `github_releases` | versions | reads one page of the newest releases of `repo` |
| | `releases` | how many of them to look at, 1 to 10, default 3 |
| | `prereleases` | `true` includes pre-releases. Drafts are always skipped |
| | `tag` | the release tag pattern. What `${version}` matches is the version |
| | `GITHUB_TOKEN` | sent as `Authorization` when set. lodi never logs or records its value |
| | limits | a 403 or 429 is `E_FETCH`, with a hint on `GITHUB_TOKEN` and `x-ratelimit-reset` |
| | cache | kept with its `ETag` under `$LODI_HOME/cache/api/`, used only on a `304` |

`github_releases` does not fall back to any other source. The two index strategies read a file
you name. From `catalogue/tools/go.toml`:

```toml
[versions]
strategy     = "json_index"
url          = "https://go.dev/dl/?mode=json&include=all"
version_key  = "version"
strip_prefix = "go"
files_key    = "files"
file_key     = "filename"
file         = "go${version}.linux-${arch_alt}.tar.gz"
```

| Strategy | Key or part | What it does |
|---|---|---|
| `json_index` | `url` | a JSON array. Each `version_key`, minus `strip_prefix`, is a version |
| | `files_key` and `file` | together: an entry counts only if that list holds `file` |
| | `file` | may use `${version}` |
| | `file_key` | the list holds tables, compared on this key |
| | digest | `checksum = "index"` reads it from the matched table |
| `text_index` | `url` and `line` | keeps the lines of a text file that match `line` |
| | `line` | a pattern with one `${version}` |
| | URL and SHA-256 | none. The recipe writes each `url` and gives each a hash source |

`text_index` skips a line whose middle is not a version, and a version seen twice. It finds only
the versions the document lists, so a one-line document such as kubectl's gives only the
current version.

## Add a recipe

1. Write `catalogue/tools/NAME.toml`.
2. Check it:

   ```sh
   python3 -B scripts/check-catalogue.py
   cargo test --locked --test catalogue
   ```

3. In the folder of a project that uses it, update the lock and look at the new entry in
   `lodi.lock`:

   ```sh
   lodi update
   ```

A recipe that uses an existing strategy needs no other change. A new strategy is lodi code,
and it is added to the table above in the same change.

## A recipe of your own

Install a tool lodi does not carry, such as `kubectl-convert`. Write its recipe in the `recipes/`
folder of your config: `~/.config/lodi` by default, a path you typed before, or `LODI_REPO`. It
has the format above:

```toml
# ~/.config/lodi/recipes/kubectl-convert.toml
[recipe]
name        = "kubectl-convert"
description = "Convert Kubernetes manifests between API versions"
homepage    = "https://kubernetes.io/docs/tasks/tools/"

[versions]
strategy = "text_index"
url      = "https://dl.k8s.io/release/stable.txt"
line     = "v${version}"

[asset]
url              = "https://dl.k8s.io/release/v${version}/bin/linux/${arch_alt}/kubectl-convert"
format           = "binary"
checksum_sidecar = ".sha256"

[spec]
bin  = ["bin/kubectl-convert"]
path = ["bin"]
```

Name it in your `home.toml` under `[tools]` as `kubectl-convert = "latest"`, then run
`lodi switch`. You get `kubectl-convert` on `PATH`.

- **Where.** Only `recipes/NAME.toml` at the root of your config. No other folder is read. With
  no config, only the built-in recipes are used.
- **Which config.** `switch`, `update`, `pin` and `unpin` read the config they act on.
  `develop`, `run`, `shell` and `search` never look for a config, and use the built-in recipes
  only.
- **What.** Regular files named `NAME.toml`, with `NAME` a package name. A link, a folder or
  another name is skipped with `W_RECIPE_SKIPPED`.
- **Checks.** Your recipe is checked as the built-in ones are: HTTPS URLs only, and no version
  number outside a comment.
- **Which one wins.** Your `recipes/jq.toml` replaces the built-in `jq`, and each command that
  uses it prints `W_RECIPE_SHADOWED` once.
- **A broken file.** It stops only the tool that names it, with `E_RECIPE_INVALID`.
- **The lock.** The entry records `"input": "user"`, the file's name and its SHA-256. Editing the
  file makes only that tool stale.
