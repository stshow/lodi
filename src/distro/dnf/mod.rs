//! The dnf family: a Fedora container base (LD-435).
//!
//! **Lock time** fetches metadata only: each repository's `repodata/repomd.xml` and the
//! `primary` index it names, held to the digests and sizes `repomd.xml` states and decompressed
//! under a cap ([`metadata`]). The closure is resolved on top of the base image's own builds,
//! which the base definition pins as measured (`installed`), the way dnf5 resolves it
//! ([`resolve`]). Every build of the closure — the base image's and the ones to install — is
//! recorded with its name, epoch, version, release, arch, SHA-256 and repository.
//!
//! **Image build** is a pure function of the lock: the verified package files are mounted
//! read-only and installed by one `rpm -U` transaction with no network, and the read-back
//! compares every installed build (`rpm -qa`) with the lock. The base image's signing key
//! (`gpg-pubkey`) is an rpm database entry, not a build, and is not compared.
//!
//! The Fedora 44 base resolves against the release's own `Everything` repository, which never
//! changes after the release, and the base image of the same release, whose every build that
//! repository serves (measured). The updates repository and the updates archive are not used:
//! no build's identity and checksum has been proven in the archive (LD-434, LD-435).

pub mod metadata;
pub mod resolve;

use std::collections::BTreeMap;

use crate::catalogue::{BaseDefinition, ReleaseDefinition, render};
use crate::container::IMAGE_FORMAT;
use crate::debian::base::LockedIndex;
use crate::debian::resolve::{Artifact, ClosurePackage, Origin};
use crate::debian::version::DebVersion;
use crate::diag::Diagnostic;
use crate::distro::{Family, FamilyOps, ImageOps, IndexPackages, ReadBack, Repository};
use crate::fetch::Fetcher;
use crate::lock::{BaseEntry, ClosureEntry};
use crate::util::{sha256_hex, snapshot_id};

pub use metadata::RpmPackage;

/// The dnf implementation of the family seam.
pub struct Dnf;

/// Where the verified package files of a build are mounted, read-only.
const PACKAGE_MOUNT: &str = "/lodi-pkgs";
/// The query format of the read-back: one build per line.
const QUERY_FORMAT: &str = "%{NAME}\\t%{EPOCHNUM}\\t%{VERSION}\\t%{RELEASE}\\t%{ARCH}\\n";
/// The rpm database entry that holds an imported signing key; it is not a build.
const KEY_ENTRY: &str = "gpg-pubkey";

impl FamilyOps for Dnf {
    fn package_arch(&self, arch: &str) -> Option<&'static str> {
        Family::Dnf.package_arch(arch)
    }

    fn repositories(
        &self,
        def: &BaseDefinition,
        release: &ReleaseDefinition,
        snapshot: i64,
        package_arch: &str,
    ) -> Result<Vec<Repository>, Diagnostic> {
        let invalid =
            |why: String| Diagnostic::new("E_RECIPE_INVALID", format!("{}: {why}", def.file));
        let snapshot_id = snapshot_id(snapshot);
        release
            .repositories
            .iter()
            .map(|name| {
                let definition = &def.repositories[name];
                if !definition.suites.is_empty() || !definition.components.is_empty() {
                    return Err(invalid(format!(
                        "dnf repository `{name}` must have empty suites and components"
                    )));
                }
                let url = render(
                    &definition.snapshot_url,
                    &[
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

    fn fetch_index(
        &self,
        fetcher: &dyn Fetcher,
        repository: &Repository,
        _package_arch: &str,
        into: &mut IndexPackages,
    ) -> Result<Vec<LockedIndex>, Diagnostic> {
        let (repomd_sha256, primary, packages) =
            metadata::fetch_primary(fetcher, &repository.url, &repository.name)?;
        into.rpm.extend(packages);
        Ok(vec![LockedIndex {
            suite: String::new(),
            component: String::new(),
            release_path: "repodata/repomd.xml".into(),
            release_sha256: repomd_sha256,
            packages_path: primary.location,
            packages_sha256: primary.sha256,
            packages_size: primary.size,
        }])
    }

    fn resolve(
        &self,
        index: &IndexPackages,
        installed: &[(String, DebVersion)],
        requested: &[String],
        _distro: &str,
    ) -> Result<Vec<ClosurePackage>, Diagnostic> {
        if !installed.is_empty() {
            return Err(Diagnostic::new(
                "E_RECIPE_INVALID",
                "a Fedora base must not declare an apt package list".to_string(),
            ));
        }
        Ok(resolve::resolve(&index.rpm, &index.rpm_base, requested)?
            .into_iter()
            .map(|r| {
                let p = r.package;
                ClosurePackage {
                    name: p.name.clone(),
                    version: p.evr.version.clone(),
                    arch: Some(p.arch.clone()),
                    origin: if r.installed {
                        Origin::Base
                    } else {
                        Origin::Install
                    },
                    artifact: Some(Artifact {
                        filename: p.location.clone(),
                        sha256: p.sha256.clone(),
                        size: p.size,
                        repository: p.repository.clone(),
                        suite: String::new(),
                    }),
                    epoch: Some(p.evr.epoch),
                    release: p.evr.release.clone(),
                }
            })
            .collect())
    }
}

impl ImageOps for Dnf {
    fn build_files(&self, _base: &BaseEntry, has_packages: bool) -> BTreeMap<&'static str, String> {
        // One rpm transaction over exactly the verified files; rpm checks each one's signature
        // against the key the base image carries, and resolves nothing.
        let install = if has_packages {
            format!("RUN rpm -U --excludedocs {PACKAGE_MOUNT}/*.rpm && rm -rf /tmp/* /var/tmp/*\n")
        } else {
            String::new()
        };
        let containerfile = format!(
            "# Generated by Lodi ({IMAGE_FORMAT}); the build runs without network.\n\
             FROM {{BASE}}\n\
             {install}"
        );
        BTreeMap::from([("Containerfile", containerfile)])
    }

    fn closure_query_arguments<'a>(&self, tag: &'a str) -> Vec<&'a str> {
        vec![
            "run",
            "--rm",
            "--log-driver=none",
            "--network=none",
            "--pull=never",
            "--security-opt",
            "label=disable",
            "--entrypoint",
            "rpm",
            tag,
            "-qa",
            "--qf",
            QUERY_FORMAT,
        ]
    }

    /// The installed builds must be exactly the lock's closure: same names, and for each the
    /// same epoch, version, release and arch.
    fn closure_differences(&self, read_back: &ReadBack, closure: &[ClosureEntry]) -> Vec<String> {
        let mut out = Vec::new();
        let mut installed: BTreeMap<&str, (String, &str)> = BTreeMap::new();
        for line in read_back.installed.lines().filter(|l| !l.is_empty()) {
            let fields: Vec<&str> = line.split('\t').collect();
            let [name, epoch, version, release, arch] = fields[..] else {
                out.push(format!("unreadable installed package line `{line}`"));
                continue;
            };
            if name == KEY_ENTRY {
                continue;
            }
            let build = format!("{epoch}:{version}-{release}");
            if installed.insert(name, (build, arch)).is_some() {
                out.push(format!("installed twice: {name}"));
            }
        }
        for p in closure {
            let want = format!(
                "{}:{}-{}",
                p.epoch.unwrap_or(0),
                p.version,
                p.release.as_deref().unwrap_or("")
            );
            let want_arch = p.arch.as_deref().unwrap_or("");
            match installed.remove(p.name.as_str()) {
                None => out.push(format!("missing: {} {want}.{want_arch}", p.name)),
                Some((have, arch)) if have != want || arch != want_arch => out.push(format!(
                    "changed: {} expected {want}.{want_arch}, installed {have}.{arch}",
                    p.name
                )),
                Some(_) => {}
            }
        }
        for (name, (build, arch)) in installed {
            out.push(format!("added: {name} {build}.{arch}"));
        }
        out
    }

    fn package_mount(&self) -> &'static str {
        PACKAGE_MOUNT
    }

    fn staged_package_name(&self, name: &str, version: &str) -> String {
        format!("{name}-{}.rpm", sha256_hex(version.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, epoch: u64, version: &str, release: &str) -> ClosureEntry {
        ClosureEntry {
            name: name.into(),
            version: version.into(),
            arch: Some("x86_64".into()),
            origin: "install".into(),
            sha256: None,
            filename: None,
            size: None,
            repository: None,
            suite: None,
            epoch: Some(epoch),
            release: Some(release.into()),
        }
    }

    #[test]
    fn the_installed_builds_must_equal_the_lock() {
        let lock = [
            entry("jq", 0, "1.8.1", "2.fc44"),
            entry("tar", 2, "1.35", "8.fc44"),
        ];
        let same = "jq\t0\t1.8.1\t2.fc44\tx86_64\ntar\t2\t1.35\t8.fc44\tx86_64\n\
                    gpg-pubkey\t0\tabc\tdef\t(none)\n";
        let back = |installed| ReadBack {
            installed,
            recorded_base: None,
        };
        assert!(Dnf.closure_differences(&back(same), &lock).is_empty());
        let drift = "jq\t0\t1.8.1\t3.fc44\tx86_64\nzip\t0\t3.0\t45.fc44\tx86_64\n";
        let d = Dnf.closure_differences(&back(drift), &lock);
        assert_eq!(
            d,
            vec![
                "changed: jq expected 0:1.8.1-2.fc44.x86_64, installed 0:1.8.1-3.fc44.x86_64",
                "missing: tar 2:1.35-8.fc44.x86_64",
                "added: zip 0:3.0-45.fc44.x86_64",
            ]
        );
        let d = Dnf.closure_differences(&back("garbage\n"), &lock);
        assert!(d[0].starts_with("unreadable"), "{d:?}");
    }
}
