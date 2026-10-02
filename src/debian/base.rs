//! Pinning an apt container base at lock time (LD-17): the rootfs artifact, the snapshot
//! repository indexes and the package closure. Only metadata is fetched; no package or rootfs
//! is downloaded, unpacked or run.
//!
//! The module keeps its Debian name in 0.3 (M-0.3 T-4, LD-72) but assumes no distro: every
//! repository URL, suite, component and rootfs pin comes from the base definition
//! (`catalogue/bases/debian.toml`, `catalogue/bases/ubuntu.toml`), so Debian bookworm and Ubuntu
//! noble travel the same code and differ only where their definitions differ.
//!
//! Since M-Arch-Base T-1 it assumes no package manager either: [`lock_base`] asks the base's
//! own family (`src/distro/`) to materialize the repositories, to fetch and verify each index
//! and to resolve the request, and keeps for itself only what is family-neutral — the release
//! and snapshot bounds, the rootfs pinning strategies and the lock's own shape.

use std::io::Write;

use serde_json::Value;

use super::index::is_sha256_hex;
use super::resolve::{ClosurePackage, ResolveError};
use super::version::DebVersion;
use crate::catalogue::{BaseDefinition, RootfsDefinition, render};
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::util::{format_utc, sha256_hex};

/// Largest decompressed `Packages` index accepted (bookworm main amd64 is about 50 MB).
pub const MAX_INDEX_BYTES: usize = 512 * 1024 * 1024;

/// What pinned the rootfs artifact: the provenance the strategy of the base definition gives
/// it. Both forms end in an exact SHA-256 of the layer; they differ in what states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootfsPin {
    /// `github_oci_layout`: a git commit of the artifact repository and the OCI image manifest
    /// (verified against the digest `index.json` gives it) that names the layer.
    GithubOciLayout {
        repo: String,
        commit: String,
        /// `sha256:` of the OCI image manifest that names the layer.
        oci_manifest: String,
    },
    /// `dated_checksum_dir`: the `sha256sum`-format file published beside the artifact in its
    /// dated directory, recorded with its own SHA-256 so that a later fetch of it is checkable.
    ChecksumFile { url: String, sha256: String },
    /// `release_image` (LD-435): the release's checksum file, recorded with its own SHA-256,
    /// and the OCI image manifest the archive must name.
    ReleaseImage {
        url: String,
        sha256: String,
        oci_manifest: String,
    },
}

/// The base rootfs, pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedRootfs {
    pub url: String,
    /// Lowercase hex SHA-256 of the layer.
    pub sha256: String,
    /// The layer's size when the pin states one (the OCI manifest does; a checksum file
    /// does not, and locking downloads nothing to measure it).
    pub size: Option<u64>,
    pub format: String,
    /// `builtin:debian.toml`, `builtin:ubuntu.toml`.
    pub source: String,
    pub source_sha256: String,
    /// When the base artifact was built, `YYYY-MM-DDTHH:MM:SSZ`.
    pub epoch: String,
    pub pin: RootfsPin,
    pub package_list_url: Option<String>,
    pub package_list_sha256: Option<String>,
    /// Prefix stripped when importing a rootfs whose archive has one top-level directory.
    pub subdir: Option<String>,
}

/// One `Release` + `Packages` pair that fed the closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedIndex {
    pub suite: String,
    pub component: String,
    /// Relative to the repository URL.
    pub release_path: String,
    pub release_sha256: String,
    pub packages_path: String,
    pub packages_sha256: String,
    pub packages_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedRepository {
    pub name: String,
    pub url: String,
    pub suites: Vec<String>,
    pub components: Vec<String>,
    pub indexes: Vec<LockedIndex>,
}

/// Everything a lock records about a container base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedBase {
    pub distro: String,
    pub release: String,
    pub arch: String,
    pub deb_arch: String,
    pub snapshot: String,
    pub rootfs: LockedRootfs,
    pub repositories: Vec<LockedRepository>,
    /// The manifest's `packages.common`, sorted and deduplicated.
    pub requested: Vec<String>,
    /// Every package of the environment, base packages included, sorted by name.
    pub closure: Vec<ClosurePackage>,
}

/// What to lock.
pub struct BaseRequest<'a> {
    pub release: &'a str,
    /// Lodi architecture, `x86_64`.
    pub arch: &'a str,
    /// Seconds since the Unix epoch.
    pub snapshot: i64,
    /// Current time, for `E_SNAPSHOT_FUTURE`.
    pub now: i64,
    pub requested: &'a [String],
}

pub(crate) fn repo_error(e: crate::fetch::FetchError) -> Diagnostic {
    Diagnostic::new("E_REPO_UNREACHABLE", e.to_string())
}

/// Resolve and pin the base, the repositories and the closure of `request.requested`.
pub fn lock_base(
    fetcher: &dyn Fetcher,
    def: &BaseDefinition,
    request: &BaseRequest,
) -> Result<LockedBase, Diagnostic> {
    let release = def.releases.get(request.release).ok_or_else(|| {
        Diagnostic::new(
            "E_UNSUPPORTED",
            format!("{} {} has no base definition", def.distro, request.release),
        )
    })?;
    let unsupported_arch = || {
        Diagnostic::new(
            "E_UNSUPPORTED_ARCH",
            format!(
                "{} {} has no {} base",
                def.distro, request.release, request.arch
            ),
        )
    };
    let ops = def.family.ops()?;
    let deb_arch = ops
        .package_arch(request.arch)
        .ok_or_else(unsupported_arch)?;
    let rootfs_def = release
        .rootfs
        .get(request.arch)
        .ok_or_else(unsupported_arch)?;
    if request.snapshot < release.snapshot_min {
        return Err(Diagnostic::new(
            "E_SNAPSHOT_TOO_OLD",
            format!(
                "snapshot {} is before {} ({} is not available earlier)",
                format_utc(request.snapshot),
                format_utc(release.snapshot_min),
                release.codename
            ),
        ));
    }
    if request.snapshot > request.now {
        return Err(Diagnostic::new(
            "E_SNAPSHOT_FUTURE",
            format!("snapshot {} is in the future", format_utc(request.snapshot)),
        ));
    }
    if def.family == crate::distro::Family::Dnf && request.snapshot != release.snapshot_min {
        return Err(Diagnostic::new(
            "E_UNSUPPORTED",
            format!(
                "a {} {} base has no snapshot to choose: its repository and image are the \
                 release's own, which never change",
                def.distro, request.release
            ),
        )
        .hint("remove `snapshot` from [container]"));
    }
    if def.family == crate::distro::Family::Pacman {
        let latest = crate::arch::base::latest_published_day(request.now);
        if request.snapshot >= latest + 86400 {
            return Err(Diagnostic::new(
                "E_SNAPSHOT_FUTURE",
                format!(
                    "snapshot {} is inside today's UTC day, which the Arch Linux Archive has not \
                     published yet (it publishes a day only after the day ends); the latest valid \
                     snapshot is {}",
                    format_utc(request.snapshot),
                    format_utc(latest)
                ),
            )
            .hint(format!(
                "write snapshot = \"{}\" or omit it to take that day by default",
                format_utc(latest)
            )));
        }
    }

    let (rootfs, base_packages) = lock_rootfs(
        fetcher,
        def,
        rootfs_def,
        &release.codename,
        deb_arch,
        request.snapshot,
    )?;

    let mut repositories = Vec::new();
    let mut index = crate::distro::IndexPackages::default();
    if let RootfsDefinition::ReleaseImage { installed, .. } = rootfs_def {
        index.rpm_base = installed.clone();
    }
    for repo in ops.repositories(def, release, request.snapshot, deb_arch)? {
        let indexes = ops.fetch_index(fetcher, &repo, deb_arch, &mut index)?;
        repositories.push(LockedRepository {
            name: repo.name,
            url: repo.url,
            suites: repo.suites,
            components: repo.components,
            indexes,
        });
    }

    let mut requested: Vec<String> = request.requested.to_vec();
    requested.sort();
    requested.dedup();
    let closure = ops.resolve(&index, &base_packages, &requested, &def.distro)?;

    Ok(LockedBase {
        distro: def.distro.clone(),
        release: release.codename.clone(),
        arch: request.arch.to_string(),
        deb_arch: deb_arch.to_string(),
        snapshot: format_utc(request.snapshot),
        rootfs,
        repositories,
        requested,
        closure,
    })
}

pub(crate) fn get_text(fetcher: &dyn Fetcher, url: &str) -> Result<(String, Vec<u8>), Diagnostic> {
    let bytes = fetcher.get(url).map_err(repo_error)?;
    let text = String::from_utf8(bytes.clone())
        .map_err(|_| Diagnostic::new("E_REPO_UNREACHABLE", format!("{url} is not UTF-8 text")))?;
    Ok((text, bytes))
}

pub(crate) fn lock_rootfs(
    fetcher: &dyn Fetcher,
    def: &BaseDefinition,
    rootfs: &RootfsDefinition,
    codename: &str,
    deb_arch: &str,
    snapshot: i64,
) -> Result<(LockedRootfs, Vec<(String, DebVersion)>), Diagnostic> {
    match rootfs {
        RootfsDefinition::GithubOciLayout {
            repo,
            branch,
            dir,
            layer,
        } => lock_rootfs_github(fetcher, def, repo, branch, dir, layer, deb_arch, snapshot),
        RootfsDefinition::ReleaseImage {
            url,
            checksums,
            oci_manifest,
            built,
            ..
        } => lock_rootfs_release_image(fetcher, def, url, checksums, oci_manifest, built),
        RootfsDefinition::DatedChecksumDir {
            index_url,
            date_format,
            checksums,
            layer,
            package_list,
            subdir,
        } => lock_rootfs_dated(
            fetcher,
            def,
            index_url,
            date_format,
            checksums,
            layer,
            package_list.as_deref(),
            subdir.as_deref(),
            codename,
            deb_arch,
            snapshot,
        ),
    }
}

/// `strategy = "release_image"` (LD-435): the archive at `url`, pinned by the SHA-256 and size
/// the release's checksum file states for its file name. Only the checksum file is fetched; its
/// PGP signature is not checked, so it is recorded with its own SHA-256 and HTTPS is what
/// authenticates it, as for every other base.
fn lock_rootfs_release_image(
    fetcher: &dyn Fetcher,
    def: &BaseDefinition,
    url: &str,
    checksums: &str,
    oci_manifest: &str,
    built: &str,
) -> Result<(LockedRootfs, Vec<(String, DebVersion)>), Diagnostic> {
    let file = url.rsplit('/').next().unwrap_or(url);
    let (text, bytes) = get_text(fetcher, checksums)?;
    let unlisted = |what: &str| {
        Diagnostic::new(
            "E_REPO_UNREACHABLE",
            format!("{checksums} lists no {what} for {file}"),
        )
    };
    let sha256 = bsd_checksum(&text, file).ok_or_else(|| unlisted("SHA-256"))?;
    let size = text
        .lines()
        .find_map(|line| {
            let rest = line
                .strip_prefix("# ")?
                .strip_prefix(file)?
                .strip_prefix(": ")?;
            rest.strip_suffix(" bytes")?.parse::<u64>().ok()
        })
        .ok_or_else(|| unlisted("size"))?;
    Ok((
        LockedRootfs {
            url: url.to_string(),
            sha256,
            size: Some(size),
            format: "oci-archive.tar.xz".into(),
            source: format!("builtin:{}", def.file),
            source_sha256: def.sha256.clone(),
            epoch: built.to_string(),
            pin: RootfsPin::ReleaseImage {
                url: checksums.to_string(),
                sha256: sha256_hex(&bytes),
                oci_manifest: oci_manifest.to_string(),
            },
            package_list_url: None,
            package_list_sha256: None,
            subdir: None,
        },
        Vec::new(),
    ))
}

/// The SHA-256 a BSD-style checksum file (`SHA256 (FILE) = HEX`) states for `file`; `None` when
/// it states none, or more than one.
pub fn bsd_checksum(text: &str, file: &str) -> Option<String> {
    let prefix = format!("SHA256 ({file}) = ");
    let mut found = text
        .lines()
        .filter_map(|line| line.trim_end().strip_prefix(&prefix));
    let hex = found.next()?;
    if found.next().is_some() || !is_sha256_hex(hex) {
        return None;
    }
    Some(hex.to_ascii_lowercase())
}

/// The package list of a base rootfs: `name<TAB>version` lines, as both debuerreotype and
/// Canonical publish them.
fn parse_package_list(url: &str, text: &str) -> Result<Vec<(String, DebVersion)>, Diagnostic> {
    let bad = |why: String| Diagnostic::new("E_RECIPE_CTX", format!("{url}: {why}"));
    let mut base = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (name, version) = line
            .split_once('\t')
            .ok_or_else(|| bad(format!("line {}: expected name<TAB>version", n + 1)))?;
        let name = name.split(':').next().unwrap_or(name);
        let version = DebVersion::parse(version.trim()).map_err(bad)?;
        base.push((name.to_string(), version));
    }
    if base.is_empty() {
        return Err(bad("the base package list is empty".into()));
    }
    Ok(base)
}

/// The `YYYYMMDD/` directory names an HTML index lists, newest last. Anything else in the
/// listing — a parent link, `current/`, a file — is ignored.
fn dated_directories(html: &str, date_format: &str) -> Vec<String> {
    let mut dates: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("href=\"") {
        rest = &rest[start + 6..];
        let Some(end) = rest.find('"') else { break };
        let (href, tail) = rest.split_at(end);
        rest = tail;
        if let Some(name) = href.strip_suffix('/')
            && dated_epoch(name, date_format).is_some()
            && !dates.iter().any(|d| d == name)
        {
            dates.push(name.to_string());
        }
    }
    dates.sort();
    dates
}

/// Seconds since the Unix epoch of midnight UTC on a `YYYYMMDD` directory name.
fn dated_epoch(name: &str, date_format: &str) -> Option<i64> {
    let rendered = match date_format {
        "YYYYMMDD" if name.len() == 8 && name.bytes().all(|byte| byte.is_ascii_digit()) => {
            format!("{}-{}-{}", &name[0..4], &name[4..6], &name[6..8])
        }
        "YYYY.MM.DD"
            if name.len() == 10
                && name.as_bytes()[4] == b'.'
                && name.as_bytes()[7] == b'.'
                && name
                    .bytes()
                    .enumerate()
                    .all(|(at, byte)| matches!(at, 4 | 7) || byte.is_ascii_digit()) =>
        {
            name.replace('.', "-")
        }
        _ => return None,
    };
    crate::util::parse_utc(&format!("{rendered}T00:00:00Z"))
}

/// `strategy = "dated_checksum_dir"` (LD-72): the newest dated directory of `index_url` that is
/// not after the snapshot pins the rootfs, by the URL and the SHA-256 its own checksum file
/// states. No artifact is downloaded: the checksum file and the package list are metadata.
#[allow(clippy::too_many_arguments)]
fn lock_rootfs_dated(
    fetcher: &dyn Fetcher,
    def: &BaseDefinition,
    index_url: &str,
    date_format: &str,
    checksums: &str,
    layer: &str,
    package_list: Option<&str>,
    subdir: Option<&str>,
    codename: &str,
    deb_arch: &str,
    snapshot: i64,
) -> Result<(LockedRootfs, Vec<(String, DebVersion)>), Diagnostic> {
    let vars = [("release", codename), ("deb_arch", deb_arch)];
    let expand = |template: &str| {
        render(template, &vars)
            .map_err(|e| Diagnostic::new("E_RECIPE_INVALID", format!("{}: {e}", def.file)))
    };
    let index_url = expand(index_url)?;
    let layer = expand(layer)?;
    let package_list = package_list.map(expand).transpose()?;
    let checksums = expand(checksums)?;

    let (listing, _) = get_text(fetcher, &index_url)?;
    let dates = dated_directories(&listing, date_format);
    if dates.is_empty() {
        return Err(Diagnostic::new(
            "E_REPO_UNREACHABLE",
            format!("{index_url} lists no dated base directory"),
        ));
    }
    let chosen = dates
        .iter()
        .rev()
        .find(|d| dated_epoch(d, date_format).is_some_and(|e| e <= snapshot))
        .ok_or_else(|| {
            Diagnostic::new(
                "E_SNAPSHOT_TOO_OLD",
                format!(
                    "snapshot {} is older than every {} base {index_url} still publishes (the oldest is {})",
                    format_utc(snapshot),
                    def.distro,
                    dates[0]
                ),
            )
            .hint("use a later snapshot, or omit it to take the default snapshot")
        })?;
    let epoch = dated_epoch(chosen, date_format).expect("the chosen directory parsed as a date");
    let dir_url = format!("{index_url}{chosen}/");
    let layer = layer.replace(date_format, chosen);

    let checksums_url = format!("{dir_url}{checksums}");
    let (checksums_text, checksums_bytes) = get_text(fetcher, &checksums_url)?;
    let listed = |file: &str| -> Result<String, Diagnostic> {
        crate::upstream::sha256sums_lookup(&checksums_text, file)
            .filter(|hash| is_sha256_hex(hash))
            .ok_or_else(|| {
                Diagnostic::new(
                    "E_REPO_UNREACHABLE",
                    format!("{checksums_url} lists no SHA-256 for {file}"),
                )
            })
    };
    let layer_sha = listed(&layer)?;
    let (list_url, actual, base) = if let Some(package_list) = package_list {
        let list_sha = listed(&package_list)?;
        let list_url = format!("{dir_url}{package_list}");
        let (list_text, list_bytes) = get_text(fetcher, &list_url)?;
        let actual = sha256_hex(&list_bytes);
        if actual != list_sha {
            return Err(Diagnostic::new(
                "E_HASH_MISMATCH",
                format!("{list_url}: {checksums} lists sha256:{list_sha}, got sha256:{actual}"),
            )
            .hint(
                "the base package list does not match its checksum file; do not use this mirror",
            ));
        }
        let base = parse_package_list(&list_url, &list_text)?;
        (Some(list_url), Some(actual), base)
    } else {
        (None, None, Vec::new())
    };

    let format = if layer.ends_with(".tar.zst") {
        "tar.zst"
    } else if layer.ends_with(".tar.xz") {
        "tar.xz"
    } else {
        "tar.gz"
    };

    Ok((
        LockedRootfs {
            url: format!("{dir_url}{layer}"),
            sha256: layer_sha,
            size: None,
            format: format.into(),
            source: format!("builtin:{}", def.file),
            source_sha256: def.sha256.clone(),
            epoch: format_utc(epoch),
            pin: RootfsPin::ChecksumFile {
                url: checksums_url,
                sha256: sha256_hex(&checksums_bytes),
            },
            package_list_url: list_url,
            package_list_sha256: actual,
            subdir: subdir.map(str::to_string),
        },
        base,
    ))
}

/// `strategy = "github_oci_layout"`: the debuerreotype artifact repository (LD-17).
#[allow(clippy::too_many_arguments)]
fn lock_rootfs_github(
    fetcher: &dyn Fetcher,
    def: &BaseDefinition,
    repo: &str,
    branch: &str,
    dir: &str,
    layer_path: &str,
    deb_arch: &str,
    snapshot: i64,
) -> Result<(LockedRootfs, Vec<(String, DebVersion)>), Diagnostic> {
    let bad = |url: &str, why: String| Diagnostic::new("E_RECIPE_CTX", format!("{url}: {why}"));

    let ref_url = format!("https://api.github.com/repos/{repo}/git/ref/heads/{branch}");
    let (text, _) = get_text(fetcher, &ref_url)?;
    let value: Value = serde_json::from_str(&text).map_err(|e| bad(&ref_url, e.to_string()))?;
    let commit = value["object"]["sha"]
        .as_str()
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| bad(&ref_url, "no commit in the branch reference".into()))?
        .to_ascii_lowercase();
    let raw = format!("https://raw.githubusercontent.com/{repo}/{commit}/{dir}/");

    let (arch_text, _) = get_text(fetcher, &format!("{raw}rootfs.dpkg-arch"))?;
    if arch_text.trim() != deb_arch {
        return Err(Diagnostic::new(
            "E_UNSUPPORTED_ARCH",
            format!("the base at {raw} is {} , not {deb_arch}", arch_text.trim()),
        ));
    }
    let epoch_url = format!("{raw}rootfs.debuerreotype-epoch");
    let (epoch_text, _) = get_text(fetcher, &epoch_url)?;
    let epoch: i64 = epoch_text
        .trim()
        .parse()
        .map_err(|_| bad(&epoch_url, "not a Unix timestamp".into()))?;
    if epoch > snapshot {
        return Err(Diagnostic::new(
            "E_SNAPSHOT_TOO_OLD",
            format!(
                "snapshot {} is older than the current {} base (built from {})",
                format_utc(snapshot),
                def.distro,
                format_utc(epoch)
            ),
        )
        .hint("use a later snapshot, or omit it to take the default snapshot"));
    }

    // OCI image layout: index.json -> image manifest (verified by digest) -> the one layer.
    let index_url = format!("{raw}oci/index.json");
    let (index_text, _) = get_text(fetcher, &index_url)?;
    let index: Value =
        serde_json::from_str(&index_text).map_err(|e| bad(&index_url, e.to_string()))?;
    let manifests = index["manifests"].as_array().cloned().unwrap_or_default();
    let entry = manifests
        .iter()
        .find(|m| m["platform"]["architecture"].as_str() == Some(deb_arch))
        .ok_or_else(|| bad(&index_url, format!("no {deb_arch} image")))?;
    let manifest_digest = entry["digest"]
        .as_str()
        .and_then(crate::util::untag_sha256)
        .ok_or_else(|| bad(&index_url, "no sha256 manifest digest".into()))?
        .to_string();
    let manifest_url = format!("{raw}oci/blobs/image-manifest.json");
    let manifest_bytes = fetcher.get(&manifest_url).map_err(repo_error)?;
    if sha256_hex(&manifest_bytes) != manifest_digest {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!(
                "{manifest_url}: expected sha256:{manifest_digest}, got sha256:{}",
                sha256_hex(&manifest_bytes)
            ),
        ));
    }
    let manifest: Value =
        serde_json::from_slice(&manifest_bytes).map_err(|e| bad(&manifest_url, e.to_string()))?;
    let layers = manifest["layers"].as_array().cloned().unwrap_or_default();
    let [layer] = layers.as_slice() else {
        return Err(bad(
            &manifest_url,
            format!("expected one layer, found {}", layers.len()),
        ));
    };
    if layer["mediaType"].as_str() != Some("application/vnd.oci.image.layer.v1.tar+gzip") {
        return Err(bad(&manifest_url, "the layer is not a gzip tar".into()));
    }
    let layer_sha = layer["digest"]
        .as_str()
        .and_then(crate::util::untag_sha256)
        .ok_or_else(|| bad(&manifest_url, "no sha256 layer digest".into()))?
        .to_string();
    let layer_size = layer["size"]
        .as_u64()
        .ok_or_else(|| bad(&manifest_url, "no layer size".into()))?;

    // The package list is the one file of the base no digest above covers. The commit's own
    // tree names it by its git blob id, so it is held to that id before it is read, as the
    // dated path holds its list to the checksum file (M-1.0 u-4, LD-363).
    let list_url = format!("{raw}rootfs.manifest");
    let tree_url = format!("https://api.github.com/repos/{repo}/contents/{dir}?ref={commit}");
    let (tree_text, _) = get_text(fetcher, &tree_url)?;
    let tree: Value =
        serde_json::from_str(&tree_text).map_err(|e| bad(&tree_url, e.to_string()))?;
    let listed = tree
        .as_array()
        .and_then(|entries| {
            entries.iter().find(|e| {
                e["name"].as_str() == Some("rootfs.manifest") && e["type"].as_str() == Some("file")
            })
        })
        .and_then(|e| e["sha"].as_str())
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            Diagnostic::new(
                "E_REPO_UNREACHABLE",
                format!("{tree_url} lists no git blob id for rootfs.manifest"),
            )
        })?;
    let (list_text, list_bytes) = get_text(fetcher, &list_url)?;
    let actual = super::gitblob::blob_id(&list_bytes);
    if actual != listed {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!("{list_url}: commit {commit} lists git blob {listed}, got {actual}"),
        )
        .hint("the base package list does not match its commit's tree; do not use this mirror"));
    }
    let base = parse_package_list(&list_url, &list_text)?;

    Ok((
        LockedRootfs {
            url: format!("{raw}{layer_path}"),
            sha256: layer_sha,
            size: Some(layer_size),
            format: "tar.gz".into(),
            source: format!("builtin:{}", def.file),
            source_sha256: def.sha256.clone(),
            epoch: format_utc(epoch),
            pin: RootfsPin::GithubOciLayout {
                repo: repo.to_string(),
                commit,
                oci_manifest: format!("sha256:{manifest_digest}"),
            },
            package_list_url: Some(list_url),
            package_list_sha256: Some(sha256_hex(&list_bytes)),
            subdir: None,
        },
        base,
    ))
}

/// Fetch and verify one suite's `Release` and one component's `Packages.xz`.
/// A writer that refuses to grow past a limit (decompression bombs).
struct Bounded {
    out: Vec<u8>,
    limit: usize,
}

impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.out.len() + buf.len() > self.limit {
            return Err(std::io::Error::other(
                "decompressed index exceeds the size limit",
            ));
        }
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Decompress an `.xz` file, at most [`MAX_INDEX_BYTES`].
///
/// An `.xz` file is a sequence of streams, each optionally followed by stream padding of zero
/// bytes (the format's §2.2, a multiple of four). Debian's archive publishes a single stream;
/// Ubuntu's `Packages.xz` carries a second, empty one, and `lzma_rs` decodes exactly one stream
/// and calls the rest "unexpected data", so the file is split on its stream boundaries first
/// and every stream concatenated (LD-73). The bound is on the total.
pub fn xz_decompress(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Bounded {
        out: Vec::new(),
        limit: MAX_INDEX_BYTES,
    };
    for stream in xz_streams(bytes) {
        lzma_rs::xz_decompress(&mut { stream }, &mut out)
            .map_err(|e| format!("not a valid xz stream: {e}"))?;
    }
    Ok(out.out)
}

/// The header magic every `.xz` stream begins with (the format's §2.1.1.1).
const XZ_MAGIC: &[u8] = b"\xfd7zXZ\x00";
/// The footer magic every `.xz` stream ends with (§2.1.2.4).
const XZ_FOOTER: &[u8] = b"YZ";

/// Split an `.xz` file into its streams, dropping the padding between them.
///
/// A stream is a multiple of four bytes long and so is the padding, so a stream can only begin
/// at a multiple of four; a boundary is taken only where the magic follows a footer, which
/// keeps the same six bytes occurring inside compressed data from splitting a stream in half.
/// Anything this does not recognise is left to the decoder to refuse.
fn xz_streams(bytes: &[u8]) -> Vec<&[u8]> {
    let mut starts = vec![0usize];
    let mut at = 4;
    while at + XZ_MAGIC.len() <= bytes.len() {
        if &bytes[at..at + XZ_MAGIC.len()] == XZ_MAGIC {
            let before = &bytes[..at];
            let end = before.len() - before.iter().rev().take_while(|b| **b == 0).count();
            if end >= XZ_FOOTER.len() && &before[end - XZ_FOOTER.len()..end] == XZ_FOOTER {
                starts.push(at);
            }
        }
        at += 4;
    }
    starts.push(bytes.len());
    starts
        .windows(2)
        .map(|w| {
            let stream = &bytes[w[0]..w[1]];
            let end = stream.len() - stream.iter().rev().take_while(|b| **b == 0).count();
            &stream[..end]
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// The distribution's name as it reads in a sentence: `debian` -> `Debian`.
fn distro_title(distro: &str) -> String {
    let mut c = distro.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// A resolver failure as a `spec/11` diagnostic (all exit 4).
pub fn resolve_diagnostic(e: ResolveError, distro: &str) -> Diagnostic {
    match e {
        ResolveError::Limit { what, limit } => Diagnostic::new(
            "E_NO_MATCH",
            format!(
                "{} resolution exceeds the {what} limit of {limit}",
                distro_title(distro)
            ),
        ),
        ResolveError::NotFound { name, suggestions } => {
            let d = Diagnostic::new(
                "E_NO_MATCH",
                format!(
                    "package `{name}` is not in the pinned {} indexes",
                    distro_title(distro)
                ),
            );
            if suggestions.is_empty() {
                d
            } else {
                d.hint(format!("did you mean {}?", suggestions.join(", ")))
            }
        }
        ResolveError::Unsatisfiable {
            dependency,
            required_by,
            available,
        } => {
            let d = Diagnostic::new(
                "E_NO_MATCH",
                format!("no package satisfies `{dependency}` (required by {required_by})"),
            );
            if available.is_empty() {
                d
            } else {
                d.hint(format!("available: {}", available.join(", ")))
            }
        }
        ResolveError::AmbiguousVirtual {
            name,
            required_by,
            providers,
        } => Diagnostic::new(
            "E_NO_MATCH",
            format!(
                "`{name}` (required by {required_by}) is a virtual package with several providers"
            ),
        )
        .hint(format!(
            "add one of {} to [packages] common",
            providers.join(", ")
        )),
        ResolveError::ForeignArch {
            dependency,
            required_by,
        } => Diagnostic::new(
            "E_UNSUPPORTED_ARCH",
            format!("`{dependency}` (required by {required_by}) names another architecture"),
        ),
        ResolveError::Conflict {
            package,
            relation,
            other,
        } => Diagnostic::new(
            "E_NO_MATCH",
            format!("{package} {relation}, but the closure contains {other}"),
        ),
    }
}
