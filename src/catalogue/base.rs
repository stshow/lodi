//! Distro base definitions: `catalogue/bases/<distro>.toml`, parsed and validated
//! (`catalogue/README.md`). A base names its releases, how each release's rootfs is pinned and
//! which snapshot repositories it installs from; `src/debian/` reads the result (LD-73).

use std::collections::BTreeMap;

use crate::catalogue::{Reader, parse_document};
use crate::diag::Diagnostic;
use crate::distro::Family;
use crate::util::{parse_utc, sha256_tagged};

/// A distro base definition (`debian.toml`, `ubuntu.toml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseDefinition {
    pub distro: String,
    /// The package-manager family this base installs with: the optional `family` key, or, when
    /// the definition names none, the family the shape of its repositories implies. A name this
    /// project does not know, and a shape that implies none, are both `E_RECIPE_INVALID`
    /// (M-Arch-Base T-1); there is no default.
    pub family: Family,
    pub file: String,
    pub sha256: String,
    pub releases: BTreeMap<String, ReleaseDefinition>,
    pub repositories: BTreeMap<String, RepositoryDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseDefinition {
    pub codename: String,
    pub aliases: Vec<String>,
    /// Earliest usable snapshot, seconds since the Unix epoch.
    pub snapshot_min: i64,
    pub repositories: Vec<String>,
    /// Per Lodi architecture (`x86_64`).
    pub rootfs: BTreeMap<String, RootfsDefinition>,
    /// The release's per-package interface (M-Pin, LD-395): an HTTPS template with `${name}`,
    /// `${release}` and `${deb_arch}` that lists every publication of a package, each with the
    /// instant it was published and its file's SHA-256. `None` for a release whose family has no interface the
    /// host scope can verify, where a version is not a pin (P1).
    pub versions_url: Option<String>,
}

/// How a release's base rootfs is pinned. Exactly three strategies exist in this build; an
/// unknown `strategy` value is `E_RECIPE_INVALID` (LD-72, LD-435).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootfsDefinition {
    /// `strategy = "github_oci_layout"`: the HEAD commit of `branch` in `repo` holds an OCI
    /// image layout and the debuerreotype metadata files under `dir`. The layer is pinned by
    /// the digest the OCI image manifest gives it, and the whole artifact by the commit.
    GithubOciLayout {
        repo: String,
        branch: String,
        dir: String,
        /// Path of the rootfs layer blob below `dir`; its digest comes from the OCI manifest.
        layer: String,
    },
    /// `strategy = "dated_checksum_dir"`: `index_url` lists dated directories, each holding the
    /// rootfs tarball, an optional package list and a `sha256sum`-format `checksums` file that
    /// pins them. The newest directory not after the lock's snapshot is the one pinned.
    /// Templates take `${release}` and `${deb_arch}`; a literal `date_format` token in `layer`
    /// is replaced by the chosen directory name (for example `YYYY.MM.DD`).
    DatedChecksumDir {
        index_url: String,
        date_format: String,
        checksums: String,
        layer: String,
        package_list: Option<String>,
        subdir: Option<String>,
    },
    /// `strategy = "release_image"` (LD-435): one released OCI image archive at a fixed URL,
    /// whose SHA-256 and size the release's checksum file states (BSD `SHA256 (FILE) = HEX`
    /// lines and `# FILE: N bytes` comments). The image manifest the archive must name is the
    /// measured `oci_manifest`, and `installed` is every build the image holds, measured as
    /// `NAME-EPOCH:VERSION-RELEASE.ARCH`: what the lock's closure starts from.
    ReleaseImage {
        url: String,
        checksums: String,
        oci_manifest: String,
        /// When the image was built, `YYYY-MM-DDTHH:MM:SSZ`.
        built: String,
        installed: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryDefinition {
    pub name: String,
    /// Template with `${snapshot_id}`.
    pub snapshot_url: String,
    /// Templates with `${release}`.
    pub suites: Vec<String>,
    pub components: Vec<String>,
}

/// Parse and validate a base definition.
pub fn parse_base(text: &str, file: &str) -> Result<BaseDefinition, Diagnostic> {
    let r = Reader::new(file);
    let doc = parse_document(text, &r)?;
    let root = doc.as_table();
    r.keys(root, "root", &["base", "release", "repository"])?;
    let base = r.table(root, "base")?;
    r.keys(base, "base", &["distro", "family"])?;
    let distro = r.string(base, "distro", "base")?;
    let named = r.opt_string(base, "family", "base")?;

    let mut repositories = BTreeMap::new();
    for (name, item) in r.table(root, "repository")?.iter() {
        let at = format!("repository.{name}");
        let t = item
            .as_table_like()
            .ok_or_else(|| r.invalid(format!("[{at}] must be a table")))?;
        r.keys(t, &at, &["snapshot_url", "suites", "components"])?;
        let snapshot_url = r.string(t, "snapshot_url", &at)?;
        r.check_template(&snapshot_url, &["snapshot_id", "snapshot_day", "deb_arch"])?;
        if !snapshot_url.starts_with("https://") || !snapshot_url.ends_with('/') {
            return Err(r.invalid(format!("[{at}] snapshot_url must be HTTPS and end with /")));
        }
        let suites = r.strings(t, "suites", &at)?;
        for s in &suites {
            r.check_template(s, &["release"])?;
        }
        repositories.insert(
            name.to_string(),
            RepositoryDefinition {
                name: name.to_string(),
                snapshot_url,
                suites,
                components: r.strings(t, "components", &at)?,
            },
        );
    }

    let mut releases = BTreeMap::new();
    for (codename, item) in r.table(root, "release")?.iter() {
        let at = format!("release.{codename}");
        let t = item
            .as_table_like()
            .ok_or_else(|| r.invalid(format!("[{at}] must be a table")))?;
        r.keys(
            t,
            &at,
            &[
                "aliases",
                "snapshot_min",
                "repositories",
                "rootfs",
                "versions_url",
            ],
        )?;
        let versions_url = r.opt_string(t, "versions_url", &at)?;
        if let Some(url) = &versions_url {
            r.check_template(url, &["name", "release", "deb_arch"])?;
            if !url.starts_with("https://") {
                return Err(r.invalid(format!("[{at}] versions_url must be HTTPS")));
            }
        }
        let snapshot_min_text = r.string(t, "snapshot_min", &at)?;
        let snapshot_min = parse_utc(&snapshot_min_text)
            .ok_or_else(|| r.invalid(format!("[{at}] snapshot_min is not YYYY-MM-DDTHH:MM:SSZ")))?;
        let repos = r.strings(t, "repositories", &at)?;
        for repo in &repos {
            if !repositories.contains_key(repo) {
                return Err(r.invalid(format!("[{at}] names unknown repository `{repo}`")));
            }
        }
        let mut rootfs = BTreeMap::new();
        for (arch, item) in r.table(t, "rootfs")?.iter() {
            let at = format!("{at}.rootfs.{arch}");
            let rt = item
                .as_table_like()
                .ok_or_else(|| r.invalid(format!("[{at}] must be a table")))?;
            let strategy = r.string(rt, "strategy", &at)?;
            let definition = match strategy.as_str() {
                "github_oci_layout" => {
                    r.keys(rt, &at, &["strategy", "repo", "branch", "dir", "layer"])?;
                    RootfsDefinition::GithubOciLayout {
                        repo: r.string(rt, "repo", &at)?,
                        branch: r.string(rt, "branch", &at)?,
                        dir: r.string(rt, "dir", &at)?,
                        layer: r.string(rt, "layer", &at)?,
                    }
                }
                "dated_checksum_dir" => {
                    r.keys(
                        rt,
                        &at,
                        &[
                            "strategy",
                            "index_url",
                            "date_format",
                            "checksums",
                            "layer",
                            "package_list",
                            "subdir",
                        ],
                    )?;
                    let index_url = r.string(rt, "index_url", &at)?;
                    r.check_template(&index_url, &["release", "deb_arch"])?;
                    if !index_url.starts_with("https://") || !index_url.ends_with('/') {
                        return Err(
                            r.invalid(format!("[{at}] index_url must be HTTPS and end with /"))
                        );
                    }
                    let date_format = r
                        .opt_string(rt, "date_format", &at)?
                        .unwrap_or_else(|| "YYYYMMDD".to_string());
                    if !matches!(date_format.as_str(), "YYYYMMDD" | "YYYY.MM.DD") {
                        return Err(r.invalid(format!(
                            "[{at}] date_format must be YYYYMMDD or YYYY.MM.DD"
                        )));
                    }
                    let checksums = r.string(rt, "checksums", &at)?;
                    let layer = r.string(rt, "layer", &at)?;
                    let package_list = r.opt_string(rt, "package_list", &at)?;
                    for (key, name) in [("checksums", &checksums), ("layer", &layer)]
                        .into_iter()
                        .chain(package_list.as_ref().map(|name| ("package_list", name)))
                    {
                        r.check_template(name, &["release", "deb_arch"])?;
                        if name.contains('/') || name.starts_with('.') {
                            return Err(r.invalid(format!(
                                "[{at}] `{key}` must be a file name in the dated directory"
                            )));
                        }
                    }
                    let subdir = r.opt_string(rt, "subdir", &at)?;
                    if subdir.as_deref().is_some_and(|path| {
                        path.is_empty()
                            || path.starts_with('/')
                            || path
                                .split('/')
                                .any(|part| part.is_empty() || matches!(part, "." | ".."))
                    }) {
                        return Err(r.invalid(format!(
                            "[{at}] `subdir` must be a non-empty relative path without . or .."
                        )));
                    }
                    RootfsDefinition::DatedChecksumDir {
                        index_url,
                        date_format,
                        checksums,
                        layer,
                        package_list,
                        subdir,
                    }
                }
                "release_image" => {
                    r.keys(
                        rt,
                        &at,
                        &[
                            "strategy",
                            "url",
                            "checksums",
                            "oci_manifest",
                            "built",
                            "installed",
                        ],
                    )?;
                    let url = r.string(rt, "url", &at)?;
                    let checksums = r.string(rt, "checksums", &at)?;
                    for (key, value) in [("url", &url), ("checksums", &checksums)] {
                        if !value.starts_with("https://") || value.ends_with('/') {
                            return Err(
                                r.invalid(format!("[{at}] `{key}` must be an HTTPS file URL"))
                            );
                        }
                    }
                    let oci_manifest = r.string(rt, "oci_manifest", &at)?;
                    if crate::util::untag_sha256(&oci_manifest).is_none() {
                        return Err(r.invalid(format!("[{at}] oci_manifest must be sha256:HEX")));
                    }
                    let built = r.string(rt, "built", &at)?;
                    if parse_utc(&built).is_none() {
                        return Err(r.invalid(format!("[{at}] built is not YYYY-MM-DDTHH:MM:SSZ")));
                    }
                    let installed = r.strings(rt, "installed", &at)?;
                    if installed.is_empty() {
                        return Err(
                            r.invalid(format!("[{at}] installed must name the image's builds"))
                        );
                    }
                    RootfsDefinition::ReleaseImage {
                        url,
                        checksums,
                        oci_manifest,
                        built,
                        installed,
                    }
                }
                other => return Err(r.invalid(format!("unknown rootfs strategy `{other}`"))),
            };
            rootfs.insert(arch.to_string(), definition);
        }
        releases.insert(
            codename.to_string(),
            ReleaseDefinition {
                codename: codename.to_string(),
                aliases: r.strings(t, "aliases", &at)?,
                snapshot_min,
                repositories: repos,
                rootfs,
                versions_url,
            },
        );
    }

    let family = match &named {
        Some(name) => Family::parse(name).ok_or_else(|| {
            r.invalid(format!(
                "[base] family `{name}` is not a package-manager family this build knows"
            ))
        })?,
        None => Family::derive(&repositories).ok_or_else(|| {
            r.invalid(
                "[base] names no family and its repositories imply none; add `family =` to it"
                    .to_string(),
            )
        })?,
    };
    let shape_matches = repositories.values().all(|repository| match family {
        Family::Apt => !repository.suites.is_empty() && !repository.components.is_empty(),
        Family::Pacman | Family::Dnf => {
            repository.suites.is_empty() && repository.components.is_empty()
        }
    });
    if !shape_matches {
        return Err(r.invalid(format!(
            "[base] family `{}` does not match its repository suites/components",
            family.as_str()
        )));
    }

    Ok(BaseDefinition {
        distro,
        family,
        file: file.to_string(),
        sha256: sha256_tagged(text.as_bytes()),
        releases,
        repositories,
    })
}
