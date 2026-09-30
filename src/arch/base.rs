//! The pacman family's lock-time half: the three operations `src/distro/` asks of it before an
//! image exists (M-Arch-Base T-4, joined to the seam by T-6).
//!
//! The lock-time path downloads metadata only: the ISO directory listing and checksum file,
//! then `core.db` and `extra.db`. It never fetches a bootstrap tarball, package, detached
//! signature, keyring or executable. The rootfs pinning, the release and snapshot bounds and the
//! lock's own shape are family-neutral and stay in `crate::debian::base`, which every family's
//! `lodi lock` travels; only what changes with the package manager is here. The artifact
//! verification helpers are for deterministic tests of a previously recorded pin.

use crate::arch::db::{self, Package};
use crate::catalogue::{BaseDefinition, ReleaseDefinition, render};
use crate::debian::base::{LockedIndex, LockedRootfs, repo_error};
use crate::debian::resolve::ClosurePackage;
use crate::debian::version::DebVersion;
use crate::diag::Diagnostic;
use crate::distro::{Family, FamilyOps, IndexPackages, Repository};
use crate::fetch::Fetcher;
use crate::util::{format_utc, sha256_hex, snapshot_id};

/// The pacman implementation of the family seam. The image half of the same type is
/// `crate::arch::image`; the two halves are one type because they are one family, and a lock
/// written by this half is read back by that one.
pub struct Pacman;

impl FamilyOps for Pacman {
    fn package_arch(&self, arch: &str) -> Option<&'static str> {
        Family::Pacman.package_arch(arch)
    }

    fn repositories(
        &self,
        def: &BaseDefinition,
        release: &ReleaseDefinition,
        snapshot: i64,
        package_arch: &str,
    ) -> Result<Vec<Repository>, Diagnostic> {
        repositories(def, release, snapshot, package_arch)
    }

    fn fetch_index(
        &self,
        fetcher: &dyn Fetcher,
        repository: &Repository,
        _package_arch: &str,
        into: &mut IndexPackages,
    ) -> Result<Vec<LockedIndex>, Diagnostic> {
        let (index, packages) = fetch_index(fetcher, repository)?;
        into.pacman.extend(packages);
        Ok(vec![index])
    }

    fn resolve(
        &self,
        index: &IndexPackages,
        installed: &[(String, DebVersion)],
        requested: &[String],
        _distro: &str,
    ) -> Result<Vec<ClosurePackage>, Diagnostic> {
        // An Arch bootstrap publishes no package list, so there is nothing here to read in apt
        // version syntax; a definition that declares one is refused rather than reinterpreted.
        // What the bootstrap does carry is recorded inside the image instead, and the read-back
        // compares against it (`crate::arch::image`).
        if !installed.is_empty() {
            return Err(Diagnostic::new(
                "E_RECIPE_INVALID",
                "an Arch bootstrap must not declare an apt package list".to_string(),
            ));
        }
        Ok(super::resolve::resolve(&index.pacman, &[], requested)?.closure)
    }
}

/// 00:00:00Z of the newest UTC day the Arch Linux Archive has published at `now`: the day
/// before `now`'s own. The archive publishes a day's repositories only after that day has ended,
/// so an instant inside today names a directory that does not exist yet (LD-381). Every instant
/// of a published day selects the same repositories and the same bootstrap directory, so the
/// day's midnight is the canonical value and the default snapshot of an Arch base.
pub fn latest_published_day(now: i64) -> i64 {
    now - now.rem_euclid(86400) - 86400
}

/// Materialize `core` and `extra` at the snapshot's UTC day.
pub fn repositories(
    def: &BaseDefinition,
    release: &ReleaseDefinition,
    snapshot: i64,
    package_arch: &str,
) -> Result<Vec<Repository>, Diagnostic> {
    if def.family != Family::Pacman {
        return Err(Diagnostic::new(
            "E_RECIPE_INVALID",
            format!("{} is not a pacman base definition", def.file),
        ));
    }
    let timestamp = format_utc(snapshot);
    let snapshot_day = timestamp[..10].replace('-', "/");
    let snapshot_id = snapshot_id(snapshot);
    let invalid = |why: String| Diagnostic::new("E_RECIPE_INVALID", format!("{}: {why}", def.file));
    release
        .repositories
        .iter()
        .map(|name| {
            let definition = &def.repositories[name];
            if !definition.suites.is_empty() || !definition.components.is_empty() {
                return Err(invalid(format!(
                    "pacman repository `{name}` must have empty suites and components"
                )));
            }
            let url = render(
                &definition.snapshot_url,
                &[
                    ("snapshot_day", snapshot_day.as_str()),
                    ("snapshot_id", snapshot_id.as_str()),
                    ("deb_arch", package_arch),
                ],
            )
            .map_err(&invalid)?;
            Ok(Repository {
                name: name.clone(),
                url,
                suites: Vec::new(),
                components: Vec::new(),
            })
        })
        .collect()
}

/// Fetch one `<name>.db`, record its digest and size, and parse its bounded real format.
pub fn fetch_index(
    fetcher: &dyn Fetcher,
    repository: &Repository,
) -> Result<(LockedIndex, Vec<Package>), Diagnostic> {
    let path = format!("{}.db", repository.name);
    let url = format!("{}{path}", repository.url);
    let bytes = fetcher.get(&url).map_err(repo_error)?;
    let sha256 = sha256_hex(&bytes);
    let size = bytes.len() as u64;
    let packages = db::parse(&bytes, &repository.name)
        .map_err(|why| Diagnostic::new("E_REPO_UNREACHABLE", format!("{url}: {why}")))?;
    Ok((
        LockedIndex {
            suite: String::new(),
            component: String::new(),
            release_path: path.clone(),
            release_sha256: sha256.clone(),
            packages_path: path,
            packages_sha256: sha256,
            packages_size: size,
        },
        packages,
    ))
}

/// Fetch and verify a rootfs artifact against an already recorded pin.
pub fn verify_rootfs(fetcher: &dyn Fetcher, rootfs: &LockedRootfs) -> Result<Vec<u8>, Diagnostic> {
    verify(fetcher, &rootfs.url, &rootfs.sha256)
}

/// Fetch and verify a database against an already recorded pin, then parse it.
pub fn verify_index(
    fetcher: &dyn Fetcher,
    repository: &str,
    url: &str,
    expected: &str,
) -> Result<Vec<Package>, Diagnostic> {
    let bytes = verify(fetcher, url, expected)?;
    db::parse(&bytes, repository)
        .map_err(|why| Diagnostic::new("E_REPO_UNREACHABLE", format!("{url}: {why}")))
}

fn verify(fetcher: &dyn Fetcher, url: &str, expected: &str) -> Result<Vec<u8>, Diagnostic> {
    let bytes = fetcher.get(url).map_err(repo_error)?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!("{url}: expected sha256:{expected}, got sha256:{actual}"),
        ));
    }
    Ok(bytes)
}
