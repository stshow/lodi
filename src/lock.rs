//! `lodi.lock`: the M-Spike lock file (LD-16, design `spec/02`, ADR-008).
//!
//! Canonical JSON (keys sorted, 2-space indent, trailing newline), so equal content is equal
//! bytes. The lock pins the container base (rootfs artifact, snapshot repositories and their
//! index hashes, the complete package closure) and every upstream tool (exact version, URL,
//! SHA-256, extraction and exposure data), enough to realize the environment without any
//! catalogue or index discovery. It is written atomically; a failure leaves the previous lock.
//!
//! Staleness follows `spec/02` §6: an entry whose normalized request differs from the manifest
//! is stale, a fresh entry is kept as is even if upstream has something newer, and locking
//! performs no network access when every entry is fresh. `lodi develop` and `lodi run` lock by
//! themselves (LD-496).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalogue::builtin_base;
use crate::debian::base::{BaseRequest, LockedBase, RootfsPin, lock_base};
use crate::debian::resolve::Origin;
use crate::diag::Diagnostic;
use crate::distro::Family;
use crate::fetch::Fetcher;
use crate::manifest::{ProjectManifest, load_project_manifest};
use crate::tools::{self, ToolDecl};
use crate::upstream::LockedTool;
use crate::util::{parse_utc, sha256_tagged, untag_sha256};

/// The lock schema version this build reads and writes.
pub const LOCK_VERSION: u64 = 1;
/// The registry row the project, home and ad-hoc shell locks are read through: they are one
/// schema, so one row owns the read-set and the writer field (`src/schema.rs`, M-1.0 T-1).
pub const LOCK_ARTIFACT: &str = "project-lock";
/// The explicitly versioned spike schema; no compatibility beyond it is promised.
pub const LOCK_FORMAT: &str = "lodi-spike-lock/1";
/// The distro values the lock schema can validate. The manifest's independently closed supported
/// set keeps an Arch base unreachable until convergence.
pub const SUPPORTED_DISTROS: &[&str] = &["arch", "debian", "fedora", "ubuntu"];
pub const MANIFEST_FILE: &str = "lodi.toml";
pub const LOCK_FILE: &str = "lodi.lock";
/// The home scope's manifest and lock live beside one another in the configuration root. They
/// use the same lock schema and parser as the project pair above (M-0.5 T-4, LD-105/LD-108).
pub const HOME_MANIFEST_FILE: &str = "home.toml";
pub const HOME_LOCK_FILE: &str = "home.lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockFile {
    pub version: u64,
    pub format: String,
    pub generated_by: String,
    /// `sha256:` of the manifest bytes; informational (staleness is decided per entry).
    pub manifest_hash: String,
    /// `null` in host-tool mode.
    pub base: Option<BaseEntry>,
    /// Upstream tools by manifest label.
    pub packages: BTreeMap<String, ToolEntry>,
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BaseEntry {
    pub distro: String,
    pub release: String,
    pub arch: String,
    pub deb_arch: String,
    pub snapshot: String,
    pub pinned: bool,
    pub rootfs: RootfsEntry,
    pub repositories: Vec<RepositoryEntry>,
    pub options: BaseOptions,
    /// Per profile, the sorted requested distro package names.
    pub requested: BTreeMap<String, Vec<String>>,
    pub closure: Closure,
}

/// The pinned base rootfs. Which provenance fields are present follows the base definition's
/// rootfs strategy (LD-72), and exactly one of the two sets is: `repo`, `commit` and
/// `ociManifest` for `github_oci_layout` (a Debian lock is unchanged by T-4, byte for byte),
/// `checksums` for `dated_checksum_dir`. `size` is present only when the pin states one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootfsEntry {
    pub url: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub format: String,
    pub source: String,
    pub source_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub epoch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci_manifest: Option<String>,
    /// The `sha256sum`-format file that pinned the artifact, with its own hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksums: Option<FileRef>,
    /// Prefix stripped from a rootfs archive before it becomes the image root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// A package inventory published beside the rootfs, when the distribution has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_list: Option<FileRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileRef {
    /// A URL, or a path relative to the repository URL.
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SizedRef {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryEntry {
    pub name: String,
    pub url: String,
    pub suites: Vec<String>,
    pub components: Vec<String>,
    pub indexes: Vec<IndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexEntry {
    pub suite: String,
    pub component: String,
    pub release: FileRef,
    pub packages: SizedRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BaseOptions {
    pub install_recommends: bool,
    pub locale: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Closure {
    /// `sha256:` of the canonical JSON of `packages`.
    pub hash: String,
    pub packages: Vec<ClosureEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClosureEntry {
    pub name: String,
    pub version: String,
    pub arch: Option<String>,
    /// `base` (in the rootfs) or `install` (downloaded from the pinned repositories).
    pub origin: String,
    pub sha256: Option<String>,
    pub filename: Option<String>,
    pub size: Option<u64>,
    pub repository: Option<String>,
    pub suite: Option<String>,
    /// An rpm build's epoch and release (LD-435); `version` is then rpm's VERSION alone. Absent
    /// for every other family, whose locks keep their bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolEntry {
    pub request: ToolRequest,
    pub provider: String,
    pub version: String,
    pub tag: Option<String>,
    pub recipe: RecipeRef,
    pub artifacts: Vec<ArtifactEntry>,
    pub spec: ToolSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolRequest {
    pub constraint: String,
    pub name: String,
    pub provider: String,
    /// `sha256:` of the declaration, written only for a declaration that used a key beyond
    /// `version` (M-0.4 T-2). A plain `name = "constraint"` entry has no such key and its lock
    /// bytes are exactly what earlier builds wrote; a changed inline URL, hash, `env`, `path`
    /// or per-architecture table moves this digest and makes the entry stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipeRef {
    pub input: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactEntry {
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
    pub format: String,
    pub strip_components: u32,
    pub subdir: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolSpec {
    pub arch: String,
    pub bin: Vec<String>,
    pub path: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Canonical JSON: sorted keys, 2-space indent, `\n` line ends and a trailing newline.
pub fn canonical_json<T: Serialize>(value: &T) -> String {
    // `serde_json::Value` objects are `BTreeMap`s (the `preserve_order` feature is off), so
    // converting first sorts every key bytewise.
    let value = serde_json::to_value(value).expect("lock data serializes");
    let mut text = serde_json::to_string_pretty(&value).expect("JSON values serialize");
    text.push('\n');
    text
}

/// `sha256:` of the canonical JSON of a closure's packages.
pub fn closure_hash(packages: &[ClosureEntry]) -> String {
    sha256_tagged(canonical_json(&packages).as_bytes())
}

impl LockFile {
    pub fn to_canonical_json(&self) -> String {
        canonical_json(self)
    }
}

/// Why a lock file cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockProblem {
    Missing,
    /// A schema version outside this build's read-set (`E_LOCK_VERSION`), with the Lodi the
    /// file says wrote it — read from the raw JSON before anything else, and `None` when the
    /// file records none (M-1.0 T-1, design call D3).
    Version {
        found: u64,
        writer: Option<String>,
    },
    /// Not a well-formed spike lock, or its content is inconsistent (tampered).
    Invalid(String),
    /// Entries that are missing or stale for the manifest.
    Stale(Vec<String>),
    /// A config's lock (`lodi-config-lock`, #696), which shares the file name, never the format.
    ConfigLock,
}

impl LockProblem {
    pub fn diagnostic(&self, file: &str) -> Diagnostic {
        match self {
            LockProblem::Missing => {
                Diagnostic::new("E_LOCK_STALE", format!("{file} does not exist"))
                    .hint("`lodi develop` and `lodi run` write it")
            }
            LockProblem::Version { found, writer } => {
                let reads = crate::schema::artifact(LOCK_ARTIFACT).read_set();
                let mut d = Diagnostic::new(
                    "E_LOCK_VERSION",
                    format!("{file} has lock version {found}; this lodi reads version {reads}"),
                );
                d.notes.push(crate::schema::note_for(writer.as_deref()));
                d.hint("use the lodi that wrote it; this lodi never replaces it")
            }
            LockProblem::Invalid(why) => {
                Diagnostic::new("E_LOCK_STALE", format!("{file} is invalid: {why}"))
                    .hint(format!("remove {file} to resolve every entry again"))
            }
            LockProblem::Stale(entries) => {
                let mut d = Diagnostic::new(
                    "E_LOCK_STALE",
                    format!("{file} is out of date with the manifest"),
                );
                for e in entries {
                    d.notes.push(e.clone());
                }
                d.hint("`lodi develop` and `lodi run` resolve what changed")
            }
            LockProblem::ConfigLock => Diagnostic::new(
                "E_CONFIG",
                format!(
                    "{file} is a config's lock ({}, written by `lodi switch`, `lodi update` and \
                     `lodi pin` for host.toml and home.toml), not a project lock \
                     ({LOCK_FORMAT}); lodi never replaces it",
                    crate::config::lock::FORMAT
                ),
            )
            .hint(format!(
                "keep the config in a folder of its own (`lodi switch PATH` reads it there) and \
                 take {file} out of this project, then run `lodi develop` to lock the project"
            )),
        }
    }
}

/// Parse and validate lock bytes.
pub fn parse_lock(bytes: &[u8]) -> Result<LockFile, LockProblem> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| LockProblem::Invalid(format!("not JSON: {e}")))?;
    // A config's lock is named as one, whatever its version: never a stale project lock.
    if value.get("format").and_then(serde_json::Value::as_str) == Some(crate::config::lock::FORMAT)
    {
        return Err(LockProblem::ConfigLock);
    }
    // The schema version is read from the raw JSON and decided **before** the document is
    // deserialized, and a version outside the read-set is refused whether it is newer or older:
    // there is no upgrade on read, because an upgrade on read is a write (design call D18).
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if crate::schema::artifact(LOCK_ARTIFACT).reads_version(v) => {}
        Some(found) => {
            return Err(LockProblem::Version {
                found,
                writer: crate::schema::writer_of(LOCK_ARTIFACT, &value),
            });
        }
        None => return Err(LockProblem::Invalid("missing or unknown `version`".into())),
    }
    let lock: LockFile =
        serde_json::from_value(value).map_err(|e| LockProblem::Invalid(e.to_string()))?;
    validate(&lock).map_err(LockProblem::Invalid)?;
    Ok(lock)
}

fn check_hash(what: &str, text: &str) -> Result<(), String> {
    untag_sha256(text)
        .map(|_| ())
        .ok_or_else(|| format!("{what}: `{text}` is not sha256:<64 lowercase hex>"))
}

/// Internal consistency of a parsed lock: formats, hashes and the closure hash.
pub fn validate(lock: &LockFile) -> Result<(), String> {
    if lock.format != LOCK_FORMAT {
        return Err(format!("unknown format `{}`", lock.format));
    }
    check_hash("manifestHash", &lock.manifest_hash)?;
    if let Some(base) = &lock.base {
        // `debArch` is the package architecture in the distribution's own vocabulary, so the
        // name it must carry is the one the base's own family uses for `base.arch` (design
        // call D11). This asks the family (`src/distro/`) rather than asserting one family's
        // answer here; the name of the field is now narrower than its meaning.
        let family = builtin_base(&base.distro)
            .and_then(Result::ok)
            .map(|definition| definition.family);
        let package_arch = family.and_then(|family| family.package_arch(&base.arch));
        if !SUPPORTED_DISTROS.contains(&base.distro.as_str())
            || base.arch != "x86_64"
            || package_arch != Some(base.deb_arch.as_str())
        {
            return Err(format!(
                "unsupported base {} {} ({}); this build supports {} x86_64",
                base.distro,
                base.arch,
                base.deb_arch,
                SUPPORTED_DISTROS.join(" and ")
            ));
        }
        if !base.pinned || parse_utc(&base.snapshot).is_none() {
            return Err("the base must be pinned to a snapshot timestamp".into());
        }
        check_hash("base.rootfs.sha256", &base.rootfs.sha256)?;
        check_hash("base.rootfs.sourceSha256", &base.rootfs.source_sha256)?;
        if let Some(package_list) = &base.rootfs.package_list {
            check_hash("base.rootfs.packageList.sha256", &package_list.sha256)?;
        }
        match (family, &base.rootfs.package_list) {
            (Some(crate::distro::Family::Apt), None) => {
                return Err("an apt base.rootfs needs packageList".into());
            }
            (Some(crate::distro::Family::Pacman | crate::distro::Family::Dnf), Some(_)) => {
                return Err("this base family must not have base.rootfs.packageList".into());
            }
            _ => {}
        }
        // `subdir` is interpolated into a Containerfile line (`src/arch/image.rs`), so its
        // grammar is strict: path components of `[A-Za-z0-9._+-]`, nothing else (M-1.0 S-1).
        if base.rootfs.subdir.as_deref().is_some_and(|path| {
            path.is_empty()
                || path.starts_with('/')
                || path.split('/').any(|part| {
                    part.is_empty()
                        || matches!(part, "." | "..")
                        || !part.chars().all(|c| {
                            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')
                        })
                })
        }) {
            return Err(
                "base.rootfs.subdir must be a relative path of [A-Za-z0-9._+-] components \
                 without . or .."
                    .into(),
            );
        }
        if !base.rootfs.url.starts_with("https://") {
            return Err("base.rootfs must have an HTTPS URL".into());
        }
        // Exactly one rootfs pin, complete: a git commit with the OCI manifest that named the
        // layer, or the checksum file that listed it (LD-72).
        match (
            &base.rootfs.repo,
            &base.rootfs.commit,
            &base.rootfs.oci_manifest,
            &base.rootfs.checksums,
        ) {
            (Some(_), Some(commit), Some(oci_manifest), None) => {
                check_hash("base.rootfs.ociManifest", oci_manifest)?;
                if commit.len() != 40 {
                    return Err("base.rootfs.commit must be a full commit".into());
                }
            }
            (None, None, oci_manifest, Some(checksums)) => {
                if let Some(oci_manifest) = oci_manifest {
                    check_hash("base.rootfs.ociManifest", oci_manifest)?;
                    if family != Some(crate::distro::Family::Dnf) {
                        return Err(
                            "only a release image pins base.rootfs.ociManifest with checksums"
                                .into(),
                        );
                    }
                }
                check_hash("base.rootfs.checksums.sha256", &checksums.sha256)?;
                if !checksums.path.starts_with("https://") {
                    return Err("base.rootfs.checksums needs an HTTPS URL".into());
                }
            }
            _ => {
                return Err(
                    "base.rootfs must be pinned by a commit and an OCI manifest or by a checksum file"
                        .into(),
                );
            }
        }
        if base.repositories.is_empty() {
            return Err("base.repositories is empty".into());
        }
        for repo in &base.repositories {
            if !repo.url.starts_with("https://") || repo.indexes.is_empty() {
                return Err(format!(
                    "repository `{}` needs an HTTPS URL and indexes",
                    repo.name
                ));
            }
            for index in &repo.indexes {
                check_hash("index release", &index.release.sha256)?;
                check_hash("index packages", &index.packages.sha256)?;
            }
        }
        if !base.requested.contains_key("default") {
            return Err("base.requested has no `default` profile".into());
        }
        check_hash("base.closure.hash", &base.closure.hash)?;
        let actual = closure_hash(&base.closure.packages);
        if actual != base.closure.hash {
            return Err(format!(
                "base.closure.hash is {} but the recorded packages hash to {actual}",
                base.closure.hash
            ));
        }
        let mut previous: Option<&str> = None;
        for p in &base.closure.packages {
            // The name becomes a file name in the build context (`<name>_<sha>.deb`), so it
            // is held to the package-name grammar before anything is joined (M-1.0 S-1).
            if !crate::hostscope::manifest::is_package_name(&p.name) {
                return Err(format!("`{}` is not a package name", p.name));
            }
            if previous.is_some_and(|prev| prev >= p.name.as_str()) {
                return Err(format!("closure is not sorted by name at `{}`", p.name));
            }
            previous = Some(&p.name);
            match p.origin.as_str() {
                "install" => {
                    let complete = p.sha256.is_some()
                        && p.filename.is_some()
                        && p.repository.is_some()
                        && p.arch.is_some();
                    if !complete {
                        return Err(format!(
                            "installed package `{}` lacks its artifact data",
                            p.name
                        ));
                    }
                }
                "base" => {}
                other => return Err(format!("package `{}` has unknown origin `{other}`", p.name)),
            }
            if let Some(h) = &p.sha256 {
                check_hash(&format!("package {}", p.name), h)?;
            }
        }
    }
    for (label, tool) in &lock.packages {
        if tool.provider != "binary" || tool.request.provider != "binary" {
            return Err(format!("tool `{label}` has an unknown provider"));
        }
        if tool.artifacts.is_empty() {
            return Err(format!("tool `{label}` has no artifacts"));
        }
        for a in &tool.artifacts {
            check_hash(&format!("tool {label}"), &a.sha256)?;
            if !a.url.starts_with("https://") {
                return Err(format!("tool `{label}` has a non-HTTPS artifact URL"));
            }
        }
        check_hash(&format!("tool {label} recipe"), &tool.recipe.sha256)?;
        if tool.spec.arch != "x86_64" {
            return Err(format!("tool `{label}` is locked for {}", tool.spec.arch));
        }
        // `bin` and `path` are symlinked into the environment's `tree/bin` and put first on
        // `PATH` (`src/host.rs`), so a lock is held to the same relative-path rule as the
        // manifest (`src/tools.rs`): no leading `/`, no `..`, no empty component (M-1.0 S-1).
        for (what, paths) in [("bin", &tool.spec.bin), ("path", &tool.spec.path)] {
            for p in paths {
                if p.starts_with('/') || p.split('/').any(|c| c == ".." || c.is_empty()) {
                    return Err(format!(
                        "tool `{label}` spec.{what} `{p}` is not a relative path inside the \
                         artifact"
                    ));
                }
            }
        }
    }
    let default = lock
        .profiles
        .get("default")
        .ok_or("profiles has no `default`")?;
    let labels: Vec<&String> = lock.packages.keys().collect();
    let listed: Vec<&String> = default.packages.iter().collect();
    if labels != listed || lock.profiles.len() != 1 {
        return Err("profiles.default must list exactly the locked tools, sorted".into());
    }
    Ok(())
}

/// The requested distro packages of a manifest, sorted and deduplicated.
fn requested_packages(manifest: &ProjectManifest) -> Vec<String> {
    let mut names = manifest.packages.clone();
    names.sort();
    names.dedup();
    names
}

fn base_is_fresh(manifest: &ProjectManifest, base: &BaseEntry) -> Vec<String> {
    let mut stale = Vec::new();
    let Some(container) = &manifest.container else {
        return vec!["base: the manifest has no [container] but the lock pins one".into()];
    };
    let distro = container.distro.name();
    if base.distro != distro || base.release != container.release || base.arch != "x86_64" {
        stale.push(format!(
            "base: the lock pins {} {} {}, the manifest asks for {distro} {} x86_64",
            base.distro, base.release, base.arch, container.release
        ));
    }
    if let Some(snapshot) = &container.snapshot
        && *snapshot != base.snapshot
    {
        stale.push(format!(
            "base: the lock pins snapshot {}, the manifest asks for {snapshot}",
            base.snapshot
        ));
    }
    if base.options.install_recommends || base.options.locale != "C.UTF-8" {
        stale.push("base: options differ from the manifest".into());
    }
    let requested = requested_packages(manifest);
    if base.requested.get("default") != Some(&requested) {
        stale.push(format!(
            "base: requested packages changed ({} in the lock, {} in the manifest)",
            base.requested
                .get("default")
                .map_or(String::new(), |r| r.join(" ")),
            requested.join(" ")
        ));
    }
    stale
}

/// The architecture this build resolves for (OD-14, `AGENTS.md` §9.3a).
pub const HOST_ARCH: &str = "x86_64";

/// The request a declaration makes, as the lock records it.
fn tool_request(label: &str, decl: &ToolDecl) -> ToolRequest {
    let eff = decl.effective(HOST_ARCH);
    ToolRequest {
        constraint: eff.constraint_text,
        name: eff.name.unwrap_or_else(|| label.to_string()),
        provider: "binary".into(),
        declaration: (!decl.simple).then(|| decl.digest()),
    }
}

/// A declaration this build can tell, without resolving anything, will be left out of the lock:
/// it is `optional` and names a per-architecture table for some other architecture only.
fn skipped_here(decl: &ToolDecl) -> bool {
    decl.effective(HOST_ARCH).optional && decl.declared_for_other_arch_only(HOST_ARCH)
}

/// Missing and stale entries of `lock` for `manifest` (`spec/02` §6); empty means fresh.
pub fn staleness(manifest: &ProjectManifest, lock: &LockFile) -> Vec<String> {
    let mut stale = match (&manifest.container, &lock.base) {
        (None, None) => Vec::new(),
        (Some(_), None) => vec!["base: missing".to_string()],
        (_, Some(base)) => base_is_fresh(manifest, base),
    };
    stale.extend(tools_staleness(&manifest.tools, lock));
    stale
}

/// Missing and stale tool entries, independent of the scope whose manifest declared them. This
/// is the project freshness rule extracted without changing it so the home lock can use the same
/// request comparison (M-0.5 T-4).
pub fn tools_staleness(tools: &tools::ToolSet, lock: &LockFile) -> Vec<String> {
    let recipes = crate::catalogue::user::active().unwrap_or_default();
    let mut stale = Vec::new();
    for (label, decl) in tools {
        match lock.packages.get(label) {
            None if skipped_here(decl) => {}
            None => stale.push(format!("{label}: missing")),
            Some(entry) => stale.extend(moved(label, decl, entry, &recipes)),
        }
    }
    for label in lock.packages.keys() {
        if !tools.contains_key(label) {
            stale.push(format!("{label}: no longer in the manifest"));
        }
    }
    stale
}

/// The tools `lock` holds an entry for whose request, or recipe of your own, changed since: what
/// only `lodi update` locks again for a config, as a switch fills only what is missing (LD-528).
pub fn tools_moved(tools: &tools::ToolSet, lock: &LockFile) -> Vec<String> {
    let recipes = crate::catalogue::user::active().unwrap_or_default();
    tools
        .iter()
        .filter_map(|(label, decl)| {
            let entry = lock.packages.get(label)?;
            moved(label, decl, entry, &recipes)
        })
        .collect()
}

fn moved(
    label: &str,
    decl: &ToolDecl,
    entry: &ToolEntry,
    recipes: &crate::catalogue::user::UserRecipes,
) -> Option<String> {
    if entry.request != tool_request(label, decl) {
        return Some(format!(
            "{label}: locked for `{}`, the manifest asks for `{}`",
            entry.request.constraint,
            decl.effective(HOST_ARCH).constraint_text
        ));
    }
    recipe_staleness(label, entry, recipes)
}

/// Why the recipe `entry` was resolved through is no longer the one its tool resolves through
/// (LD-409): a recipe of your own that changed, appeared in place of a built-in one, or is gone.
/// A built-in recipe that changed with the binary is not a reason (`docs/DISCREPANCIES.md` D-09),
/// so a lock with no tool of your own is judged exactly as before.
fn recipe_staleness(
    label: &str,
    entry: &ToolEntry,
    recipes: &crate::catalogue::user::UserRecipes,
) -> Option<String> {
    let name = &entry.request.name;
    let shown = format!("{}/{name}.toml", crate::catalogue::user::DIR);
    match (entry.recipe.input.as_str(), recipes.has(name)) {
        ("user", true) if recipes.sha256(name) == Some(entry.recipe.sha256.as_str()) => None,
        ("user", true) => Some(format!("{label}: {shown} changed")),
        ("user", false) => Some(format!("{label}: {shown} is gone")),
        ("builtin", true) => Some(format!(
            "{label}: {shown} is used in place of the built-in recipe"
        )),
        _ => None,
    }
}

/// The semantic identity of a `[tools]` section: labels in canonical order and each shared
/// declaration's deterministic digest. File entries and whitespace are deliberately absent, so
/// the home lock changes only when its resolution inputs do (LD-105/LD-108).
pub fn tool_set_hash(tools: &tools::ToolSet) -> String {
    let declarations: BTreeMap<&str, String> = tools
        .iter()
        .map(|(label, declaration)| (label.as_str(), declaration.digest()))
        .collect();
    sha256_tagged(canonical_json(&declarations).as_bytes())
}

/// Write a pinned base into the lock's schema. One writer for every family: whatever differs
/// between two families is already in the [`LockedBase`] it is handed, never here.
pub fn base_entry(base: LockedBase) -> BaseEntry {
    let packages: Vec<ClosureEntry> = base
        .closure
        .into_iter()
        .map(|p| ClosureEntry {
            name: p.name,
            version: p.version,
            arch: p.arch,
            origin: match p.origin {
                Origin::Base => "base",
                Origin::Install => "install",
            }
            .into(),
            sha256: p.artifact.as_ref().map(|a| format!("sha256:{}", a.sha256)),
            filename: p.artifact.as_ref().map(|a| a.filename.clone()),
            size: p.artifact.as_ref().map(|a| a.size),
            repository: p.artifact.as_ref().map(|a| a.repository.clone()),
            suite: p.artifact.as_ref().map(|a| a.suite.clone()),
            epoch: p.epoch,
            release: p.release,
        })
        .collect();
    let r = base.rootfs;
    BaseEntry {
        distro: base.distro,
        release: base.release,
        arch: base.arch,
        deb_arch: base.deb_arch,
        snapshot: base.snapshot,
        pinned: true,
        rootfs: {
            let (repo, commit, oci_manifest, checksums) = match r.pin {
                RootfsPin::GithubOciLayout {
                    repo,
                    commit,
                    oci_manifest,
                } => (Some(repo), Some(commit), Some(oci_manifest), None),
                RootfsPin::ReleaseImage {
                    url,
                    sha256,
                    oci_manifest,
                } => (
                    None,
                    None,
                    Some(oci_manifest),
                    Some(FileRef {
                        path: url,
                        sha256: format!("sha256:{sha256}"),
                    }),
                ),
                RootfsPin::ChecksumFile { url, sha256 } => (
                    None,
                    None,
                    None,
                    Some(FileRef {
                        path: url,
                        sha256: format!("sha256:{sha256}"),
                    }),
                ),
            };
            RootfsEntry {
                url: r.url,
                sha256: format!("sha256:{}", r.sha256),
                size: r.size,
                format: r.format,
                source: r.source,
                source_sha256: r.source_sha256,
                repo,
                commit,
                epoch: r.epoch,
                oci_manifest,
                checksums,
                subdir: r.subdir,
                package_list: r.package_list_url.zip(r.package_list_sha256).map(
                    |(path, sha256)| FileRef {
                        path,
                        sha256: format!("sha256:{sha256}"),
                    },
                ),
            }
        },
        repositories: base
            .repositories
            .into_iter()
            .map(|repo| RepositoryEntry {
                name: repo.name,
                url: repo.url,
                suites: repo.suites,
                components: repo.components,
                indexes: repo
                    .indexes
                    .into_iter()
                    .map(|i| IndexEntry {
                        suite: i.suite,
                        component: i.component,
                        release: FileRef {
                            path: i.release_path,
                            sha256: format!("sha256:{}", i.release_sha256),
                        },
                        packages: SizedRef {
                            path: i.packages_path,
                            sha256: format!("sha256:{}", i.packages_sha256),
                            size: i.packages_size,
                        },
                    })
                    .collect(),
            })
            .collect(),
        options: BaseOptions {
            install_recommends: false,
            locale: "C.UTF-8".into(),
        },
        requested: BTreeMap::from([("default".to_string(), base.requested)]),
        closure: Closure {
            hash: closure_hash(&packages),
            packages,
        },
    }
}

fn tool_entry(tool: LockedTool) -> ToolEntry {
    ToolEntry {
        request: ToolRequest {
            constraint: tool.constraint,
            name: tool.name,
            provider: "binary".into(),
            declaration: tool.declaration,
        },
        provider: "binary".into(),
        version: tool.version,
        tag: tool.tag,
        recipe: RecipeRef {
            input: tool.input,
            path: tool.recipe_file,
            sha256: tool.recipe_sha256,
        },
        artifacts: tool
            .artifacts
            .into_iter()
            .map(|a| ArtifactEntry {
                url: a.url,
                sha256: format!("sha256:{}", a.sha256),
                size: a.size,
                format: a.format,
                strip_components: a.strip_components,
                subdir: a.subdir,
                exclude: a.exclude,
            })
            .collect(),
        spec: ToolSpec {
            arch: tool.arch,
            bin: tool.bin,
            path: tool.path,
            env: tool.env,
        },
    }
}

/// A failed lock operation: diagnostics and the process exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub diagnostics: Vec<Diagnostic>,
    pub exit_status: u8,
}

impl Failure {
    pub fn one(d: Diagnostic) -> Failure {
        Failure {
            exit_status: crate::diag::exit_status(d.code),
            diagnostics: vec![d],
        }
    }

    pub fn codes(&self) -> Vec<&'static str> {
        self.diagnostics.iter().map(|d| d.code).collect()
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, d) in self.diagnostics.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{d}")?;
        }
        Ok(())
    }
}

/// What locking did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every entry was fresh; nothing was fetched or written.
    UpToDate(LockFile),
    /// A new lock was written; the labels (and `base`) that were resolved.
    Written {
        lock: LockFile,
        resolved: Vec<String>,
    },
}

/// Load the manifest of the project in `root`.
pub fn load_manifest(root: &Path) -> Result<(ProjectManifest, Vec<u8>), Failure> {
    let path = root.join(MANIFEST_FILE);
    let manifest = load_project_manifest(&path).map_err(|e| Failure {
        diagnostics: e.diagnostics,
        exit_status: crate::diag::EXIT_MANIFEST,
    })?;
    let bytes = fs::read(&path).map_err(|e| {
        Failure::one(Diagnostic::new(
            "E_NO_MANIFEST",
            format!("cannot read {}: {e}", path.display()),
        ))
    })?;
    Ok((manifest, bytes))
}

/// Read `root/lodi.lock` if it exists.
pub fn read_lock(root: &Path) -> Result<LockFile, LockProblem> {
    match fs::read(root.join(LOCK_FILE)) {
        Ok(bytes) => parse_lock(&bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(LockProblem::Missing),
        Err(e) => Err(LockProblem::Invalid(format!("cannot read: {e}"))),
    }
}

/// Frozen use (`spec/10` `--frozen`): the lock of the project in `root`,
/// only if it exists, is valid and is fresh for the manifest. Never resolves, fetches or
/// writes, so no version can be substituted.
pub fn frozen(root: &Path) -> Result<LockFile, Failure> {
    let (manifest, _) = load_manifest(root)?;
    crate::catalogue::user::active().map_err(Failure::one)?;
    let lock = read_lock(root).map_err(|p| Failure::one(p.diagnostic(LOCK_FILE)))?;
    let stale = staleness(&manifest, &lock);
    if stale.is_empty() {
        Ok(lock)
    } else {
        Err(Failure::one(
            LockProblem::Stale(stale).diagnostic(LOCK_FILE),
        ))
    }
}

/// Lock the project in `root`, as `lodi develop` and `lodi run` do first (LD-496): resolve
/// missing and stale entries, keep fresh ones byte for byte, drop removed ones, and write the
/// lock atomically. A lock that cannot be read, or one a newer lodi wrote, is never replaced.
/// `now` is seconds since the Unix epoch (the snapshot pinned when the manifest has none is
/// [`default_snapshot`] of the base's family: `now` floored to the hour for an apt base, OD-17,
/// and the previous UTC day for an Arch base, LD-381).
pub fn lock_project(root: &Path, fetcher: &dyn Fetcher, now: i64) -> Result<Outcome, Failure> {
    relock_project(root, fetcher, now, false)
}

/// `lodi update` in a project (#696): every tool and the base resolved afresh, ignoring the
/// lock's entries, and `./lodi.lock` written unless its bytes would not change. `lodi.toml` is
/// only read; a lock that cannot be read, or one a newer lodi wrote, is never replaced.
pub fn update_project(root: &Path, fetcher: &dyn Fetcher, now: i64) -> Result<Outcome, Failure> {
    relock_project(root, fetcher, now, true)
}

/// [`lock_project`] (fresh entries kept, a lock with nothing stale left alone), or with `afresh`
/// [`update_project`] (every entry resolved again, a lock that comes out the same left alone).
fn relock_project(
    root: &Path,
    fetcher: &dyn Fetcher,
    now: i64,
    afresh: bool,
) -> Result<Outcome, Failure> {
    let (manifest, manifest_bytes) = load_manifest(root)?;
    crate::catalogue::user::active().map_err(Failure::one)?;
    let previous = match read_lock(root) {
        Ok(lock) => Some(lock),
        Err(LockProblem::Missing) => None,
        Err(problem) => return Err(Failure::one(problem.diagnostic(LOCK_FILE))),
    };
    if !afresh
        && let Some(lock) = &previous
        && staleness(&manifest, lock).is_empty()
    {
        return Ok(Outcome::UpToDate(lock.clone()));
    }
    let kept = if afresh { None } else { previous.as_ref() };
    let (lock, resolved) = resolve_manifest(
        &manifest,
        sha256_tagged(&manifest_bytes),
        kept,
        fetcher,
        now,
    )?;
    if afresh && previous.as_ref() == Some(&lock) {
        return Ok(Outcome::UpToDate(lock));
    }
    write_atomic(&root.join(LOCK_FILE), lock.to_canonical_json().as_bytes()).map_err(|e| {
        Failure::one(Diagnostic::new(
            "E_STORE_IO",
            format!("cannot write {LOCK_FILE}: {e}; the previous lock is unchanged"),
        ))
    })?;
    Ok(Outcome::Written { lock, resolved })
}

/// Resolve `manifest` into a lock, keeping every entry of `previous` that is still fresh, and
/// return it with the labels that were resolved. Nothing is read from or written to any project
/// directory: the caller decides where the lock goes (`./lodi.lock` for [`lock_project`],
/// `$LODI_HOME/cache/shell/<h32>.lock` for [`crate::shell`] (LD-49), or `<config>/home.lock` for
/// [`crate::home::tools`]). `manifest_hash` is the identity of what was resolved — the manifest's
/// bytes for a project, the canonical request for an ad-hoc shell, or the canonical home tool
/// declarations.
pub fn resolve_manifest(
    manifest: &ProjectManifest,
    manifest_hash: String,
    previous: Option<&LockFile>,
    fetcher: &dyn Fetcher,
    now: i64,
) -> Result<(LockFile, Vec<String>), Failure> {
    let mut resolved = Vec::new();
    let base = match &manifest.container {
        None => None,
        Some(container) => {
            let pinned = previous.and_then(|l| l.base.as_ref());
            let keep = pinned.filter(|b| base_is_fresh(manifest, b).is_empty());
            // A base whose distribution, release and snapshot request are unchanged keeps the
            // snapshot it pins: only its package list changed, so the closure is resolved again
            // in the same archive state and no package already locked moves (LD-496).
            let snapshot_pin = pinned
                .filter(|b| {
                    b.distro == container.distro.name()
                        && b.release == container.release
                        && container.snapshot.as_ref().is_none_or(|s| *s == b.snapshot)
                })
                .and_then(|b| parse_utc(&b.snapshot));
            match keep {
                Some(b) => Some(b.clone()),
                None => {
                    let distro = container.distro.name();
                    let def = builtin_base(distro)
                        .expect("every supported distro has a built-in base definition")
                        .map_err(Failure::one)?;
                    let snapshot = match (&container.snapshot, snapshot_pin) {
                        (_, Some(pinned)) => pinned,
                        (Some(text), None) => parse_utc(text).ok_or_else(|| {
                            Failure::one(Diagnostic::new(
                                "E_TYPE",
                                format!("snapshot `{text}` is not YYYY-MM-DDTHH:MM:SSZ"),
                            ))
                        })?,
                        // A Fedora release's repository and image never change after the
                        // release, so its lock records the release's own instant (LD-435).
                        (None, None) if def.family == Family::Dnf => def
                            .releases
                            .get(container.release.as_str())
                            .map_or(now, |release| release.snapshot_min),
                        (None, None) => default_snapshot(def.family, now),
                    };
                    let request = BaseRequest {
                        release: &container.release,
                        arch: "x86_64",
                        snapshot,
                        now,
                        requested: &manifest.packages,
                    };
                    resolved.push("base".to_string());
                    Some(base_entry(
                        lock_base(fetcher, &def, &request).map_err(Failure::one)?,
                    ))
                }
            }
        }
    };

    let mut packages = BTreeMap::new();
    let catalogue = crate::catalogue::user::active().map_err(Failure::one)?;
    for (label, decl) in &manifest.tools {
        let request = tool_request(label, decl);
        let keep = previous
            .and_then(|l| l.packages.get(label))
            .filter(|e| e.request == request)
            .filter(|e| recipe_staleness(label, e, &catalogue).is_none());
        let entry = match keep {
            Some(entry) => entry.clone(),
            None => {
                match tools::resolve_one(label, decl, &*catalogue, fetcher, HOST_ARCH)
                    .map_err(Failure::one)?
                {
                    tools::Outcome::Locked(tool) => {
                        resolved.push(label.clone());
                        tool_entry(*tool)
                    }
                    // `W_OPTIONAL_SKIPPED`: a warning on standard error and no entry, exactly
                    // as `W_BIN_CONFLICT` is reported by the realization path. The exit status
                    // does not change (`spec/01` §3.8, design call D7's neighbours).
                    tools::Outcome::Skipped(warning) => {
                        eprintln!("{warning}");
                        continue;
                    }
                }
            }
        };
        packages.insert(label.clone(), entry);
    }

    let lock = LockFile {
        version: LOCK_VERSION,
        format: LOCK_FORMAT.into(),
        generated_by: format!("lodi {}", env!("CARGO_PKG_VERSION")),
        manifest_hash,
        base,
        profiles: BTreeMap::from([(
            "default".to_string(),
            Profile {
                packages: packages.keys().cloned().collect(),
            },
        )]),
        packages,
    };
    validate(&lock).map_err(|e| {
        Failure::one(Diagnostic::new(
            "E_RECIPE_CTX",
            format!("resolution produced an invalid lock: {e}"),
        ))
    })?;
    Ok((lock, resolved))
}

/// The temporary file `write_atomic` uses for `path` in this process.
pub fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(Default::default, |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

/// Replace `path` with `bytes` atomically: write and sync a temporary file in the same
/// directory, then rename it over `path`. On any failure the temporary file is removed and
/// `path` keeps its previous content.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(path, bytes, |_| Ok(()))
}

/// [`write_atomic`] with a step between the synced temporary file and the rename, where tests
/// inject faults.
pub fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let tmp = temporary_path(path);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        before_rename(&tmp)?;
        fs::rename(&tmp, path)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() && tmp.symlink_metadata().is_ok_and(|m| m.is_file()) {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The line that says a lock was written: what was resolved, then the whole lock in one line. A
/// lock written with nothing resolved names no parenthesis: an empty one reads as something left
/// out.
pub fn wrote(lock: &LockFile, resolved: &[String]) -> String {
    if resolved.is_empty() {
        format!("wrote {LOCK_FILE}: {}", summary(lock))
    } else {
        format!(
            "wrote {LOCK_FILE} (resolved {}): {}",
            resolved.join(", "),
            summary(lock)
        )
    }
}

/// A one-line summary of a lock for the terminal.
pub fn summary(lock: &LockFile) -> String {
    let mut parts = Vec::new();
    if let Some(base) = &lock.base {
        let installed = base
            .closure
            .packages
            .iter()
            .filter(|p| p.origin == "install")
            .count();
        parts.push(format!(
            "{} {} snapshot {} ({} packages, {} to install)",
            base.distro,
            base.release,
            base.snapshot,
            base.closure.packages.len(),
            installed
        ));
    }
    for (label, tool) in &lock.packages {
        parts.push(format!("{label} {}", tool.version));
    }
    if parts.is_empty() {
        "nothing to lock".into()
    } else {
        parts.join(", ")
    }
}

/// The snapshot a base of `family` is pinned to when the manifest names none, in seconds since
/// the Unix epoch. An apt base takes `now` floored to the hour (OD-17): snapshot.debian.org and
/// snapshot.ubuntu.com serve the newest archive state at or before any past instant, so every
/// hour already begun is valid there. An Arch base takes the previous UTC day at 00:00:00Z,
/// because the Arch Linux Archive publishes a day only once it has ended (LD-381).
pub fn default_snapshot(family: Family, now: i64) -> i64 {
    match family {
        Family::Apt => now - now.rem_euclid(3600),
        Family::Pacman => crate::arch::base::latest_published_day(now),
        // A Fedora release's repository and image never change after the release, so there is
        // no instant to choose: the lock records the release's own (LD-435).
        Family::Dnf => now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::format_utc;

    /// The default snapshot of each family at an instant inside a UTC day (LD-381): an apt base
    /// keeps the hour already begun, and an Arch base takes the previous day's midnight, the
    /// newest day the archive has published — also at midnight itself and one second before it.
    #[test]
    fn default_snapshot_per_family() {
        let at = parse_utc("2026-09-24T13:37:05Z").unwrap();
        assert_eq!(
            format_utc(default_snapshot(Family::Apt, at)),
            "2026-09-24T13:00:00Z"
        );
        assert_eq!(
            format_utc(default_snapshot(Family::Pacman, at)),
            "2026-09-23T00:00:00Z"
        );
        let midnight = parse_utc("2026-09-24T00:00:00Z").unwrap();
        assert_eq!(
            format_utc(default_snapshot(Family::Pacman, midnight)),
            "2026-09-23T00:00:00Z"
        );
        assert_eq!(
            format_utc(default_snapshot(Family::Pacman, midnight - 1)),
            "2026-09-22T00:00:00Z"
        );
    }
}
