//! The apt family: the only package-manager implementation in this build.
//!
//! Every apt and dpkg name the container path uses lives here — the generated `lodi.sources`,
//! `90lodi`, `policy-rc.d` and `locale`, the `Containerfile`'s install command, the mount the
//! verified `.deb` files are staged into, the `dists/<suite>/<component>/binary-<arch>` index
//! path shapes and the `dpkg-query` read-back. It is the code M-0.3 T-4 landed, moved behind
//! [`super::FamilyOps`] **unchanged**: `tests/distro_seam.rs` pins the digest of every file this
//! module renders, and those digests were recorded before the seam existed.
//!
//! The formats themselves — `Packages`, `Release`, dpkg version ordering and the resolver —
//! stay in `src/debian/`, which keeps its name (LD-74) and is reached only through here.

use std::collections::BTreeMap;

use super::{FamilyOps, ImageOps, IndexPackages, LockedIndex, ReadBack, Repository};
use crate::catalogue::{BaseDefinition, ReleaseDefinition, render};
use crate::container::IMAGE_FORMAT;
use crate::debian::base::{repo_error, xz_decompress};
use crate::debian::index::{BinaryPackage, Release, parse_packages};
use crate::debian::resolve::{ClosurePackage, resolve};
use crate::debian::version::DebVersion;
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::lock::{BaseEntry, ClosureEntry};
use crate::util::{sha256_hex, snapshot_id};

/// Where the verified `.deb` files of a build are mounted.
const DEBS_MOUNT: &str = "/lodi-debs";

/// The apt implementation of the family seam.
pub struct Apt;

impl FamilyOps for Apt {
    fn package_arch(&self, arch: &str) -> Option<&'static str> {
        crate::distro::Family::Apt.package_arch(arch)
    }

    fn repositories(
        &self,
        def: &BaseDefinition,
        release: &ReleaseDefinition,
        snapshot: i64,
        _package_arch: &str,
    ) -> Result<Vec<Repository>, Diagnostic> {
        let id = snapshot_id(snapshot);
        let invalid = |e: String| Diagnostic::new("E_RECIPE_INVALID", format!("{}: {e}", def.file));
        let mut out = Vec::new();
        for name in &release.repositories {
            let repo = &def.repositories[name];
            let url = render(&repo.snapshot_url, &[("snapshot_id", &id)]).map_err(invalid)?;
            let suites: Vec<String> = repo
                .suites
                .iter()
                .map(|s| render(s, &[("release", &release.codename)]))
                .collect::<Result<_, _>>()
                .map_err(invalid)?;
            out.push(Repository {
                name: name.clone(),
                url,
                suites,
                components: repo.components.clone(),
            });
        }
        Ok(out)
    }

    fn fetch_index(
        &self,
        fetcher: &dyn Fetcher,
        repository: &Repository,
        package_arch: &str,
        into: &mut IndexPackages,
    ) -> Result<Vec<LockedIndex>, Diagnostic> {
        let mut indexes = Vec::new();
        for suite in &repository.suites {
            for component in &repository.components {
                let (locked, found) = fetch_one(
                    fetcher,
                    &repository.name,
                    &repository.url,
                    suite,
                    component,
                    package_arch,
                )?;
                indexes.push(locked);
                into.apt.extend(found);
            }
        }
        Ok(indexes)
    }

    fn resolve(
        &self,
        index: &IndexPackages,
        installed: &[(String, DebVersion)],
        requested: &[String],
        distro: &str,
    ) -> Result<Vec<ClosurePackage>, Diagnostic> {
        resolve(&index.apt, installed, requested)
            .map_err(|e| crate::debian::base::resolve_diagnostic(e, distro))
    }
}

impl ImageOps for Apt {
    fn build_files(&self, base: &BaseEntry, has_packages: bool) -> BTreeMap<&'static str, String> {
        let mut sources = String::new();
        for repo in &base.repositories {
            if !sources.is_empty() {
                sources.push('\n');
            }
            sources.push_str(&format!(
                "Types: deb\nURIs: {}\nSuites: {}\nComponents: {}\nCheck-Valid-Until: no\n",
                repo.url,
                repo.suites.join(" "),
                repo.components.join(" ")
            ));
        }
        let apt = format!(
            "APT::Sandbox::User \"root\";\nAPT::Install-Recommends \"{}\";\n\
             Acquire::Retries \"3\";\nAcquire::Languages \"none\";\nAPT::Get::Assume-Yes \"true\";\n\
             Dpkg::Options {{ \"--force-confdef\"; \"--force-confold\"; }};\n",
            u8::from(base.options.install_recommends)
        );
        let install = if has_packages {
            format!(" && apt-get install --no-install-recommends --yes {DEBS_MOUNT}/*.deb")
        } else {
            String::new()
        };
        // The distribution's own sources file goes with the list: Debian's image ships
        // `debian.sources`, Ubuntu's `ubuntu.sources`, and the build must install from the pinned
        // repositories alone.
        let distro = &base.distro;
        let containerfile = format!(
            "# Generated by Lodi ({IMAGE_FORMAT}); the build runs without network.\n\
             FROM {{BASE}}\n\
             COPY lodi.sources /etc/apt/sources.list.d/lodi.sources\n\
             COPY 90lodi /etc/apt/apt.conf.d/90lodi\n\
             COPY policy-rc.d /usr/sbin/policy-rc.d\n\
             COPY locale /etc/default/locale\n\
             RUN set -eu; chmod 0755 /usr/sbin/policy-rc.d \
             && rm -f /etc/apt/sources.list /etc/apt/sources.list.d/{distro}.sources\
             {install} \
             && rm -rf /var/lib/apt/lists/* /var/cache/apt/*.bin /var/log/apt/* /var/log/dpkg.log \
             /root/.cache /tmp/* \
             && : > /etc/machine-id\n"
        );
        let mut files = BTreeMap::new();
        files.insert("Containerfile", containerfile);
        files.insert("lodi.sources", sources);
        files.insert("90lodi", apt);
        files.insert("policy-rc.d", "#!/bin/sh\nexit 101\n".to_string());
        files.insert("locale", format!("LANG={}\n", base.options.locale));
        files
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
            "dpkg-query",
            tag,
            "--show",
            "--showformat",
            "${db:Status-Abbrev}\t${Package}\t${Version}\t${Architecture}\n",
        ]
    }

    /// A Debian or Ubuntu rootfs publishes its own `name<TAB>version` list, so the lock's
    /// closure already names every package the image must hold and
    /// [`ImageOps::recorded_base_arguments`] asks for nothing: `read_back.recorded_base` is
    /// always `None` here and is not read.
    fn closure_differences(&self, read_back: &ReadBack, closure: &[ClosureEntry]) -> Vec<String> {
        let mut installed: BTreeMap<String, (String, String)> = BTreeMap::new();
        let mut broken = Vec::new();
        for line in read_back.installed.lines().filter(|l| !l.trim().is_empty()) {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 4 {
                broken.push(format!("unreadable package database line `{line}`"));
                continue;
            }
            let (status, name, version, arch) = (f[0], f[1], f[2], f[3]);
            match status.as_bytes().get(1) {
                // Not installed, or only its configuration files remain.
                Some(b'n') | Some(b'c') => continue,
                Some(b'i') if status.as_bytes().get(2).is_none_or(|b| *b == b' ') => {
                    installed.insert(name.to_string(), (version.to_string(), arch.to_string()));
                }
                _ => broken.push(format!("{name} {version} is in state `{}`", status.trim())),
            }
        }
        let mut out = broken;
        let locked: BTreeMap<&str, &ClosureEntry> =
            closure.iter().map(|p| (p.name.as_str(), p)).collect();
        for (name, p) in &locked {
            match installed.get(*name) {
                None => out.push(format!("missing: {name} {}", p.version)),
                Some((version, arch)) => {
                    let arch_ok = p.arch.as_deref().is_none_or(|a| a == arch);
                    if version != &p.version || !arch_ok {
                        out.push(format!(
                            "changed: {name} locked {} {}, installed {version} {arch}",
                            p.version,
                            p.arch.as_deref().unwrap_or("-")
                        ));
                    }
                }
            }
        }
        for (name, (version, _)) in &installed {
            if !locked.contains_key(name.as_str()) {
                out.push(format!("added: {name} {version}"));
            }
        }
        out
    }

    fn package_mount(&self) -> &'static str {
        DEBS_MOUNT
    }

    fn staged_package_name(&self, name: &str, version: &str) -> String {
        // The name only has to end in `.deb`; apt reads the package's own control data.
        format!("{name}_{}.deb", sha256_hex(version.as_bytes()))
    }
}

/// One suite and component of one repository: the `Release` file, the `Packages.xz` it lists,
/// and the packages that index carries.
fn fetch_one(
    fetcher: &dyn Fetcher,
    repository: &str,
    url: &str,
    suite: &str,
    component: &str,
    deb_arch: &str,
) -> Result<(LockedIndex, Vec<BinaryPackage>), Diagnostic> {
    let release_path = format!("dists/{suite}/Release");
    let release_url = format!("{url}{release_path}");
    let (release_text, release_bytes) = crate::debian::base::get_text(fetcher, &release_url)?;
    let release = Release::parse(&release_text)
        .map_err(|e| Diagnostic::new("E_REPO_UNREACHABLE", format!("{release_url}: {e}")))?;
    // Which field names the suite differs by distro: Debian's snapshot `Release` carries
    // `Codename: bookworm-updates` under `Suite: oldstable-updates`, while Ubuntu's carries
    // `Suite: noble-updates` under the bare `Codename: noble` (both observed 2026-09-20).
    // apt matches either, and so does this check.
    if release.codename != suite && release.suite != suite {
        return Err(Diagnostic::new(
            "E_REPO_UNREACHABLE",
            format!(
                "{release_url} is for `{}` (suite `{}`), not `{suite}`",
                release.codename, release.suite
            ),
        ));
    }
    if !release.architectures.iter().any(|a| a == deb_arch) {
        return Err(Diagnostic::new(
            "E_UNSUPPORTED_ARCH",
            format!("{release_url} does not list architecture {deb_arch}"),
        ));
    }
    // The security archive lists `updates/main` while its index paths use `main/`; apt
    // matches the last path element, and so does this check.
    let listed = |c: &String| c == component || c.rsplit('/').next() == Some(component);
    if !release.components.iter().any(listed) {
        return Err(Diagnostic::new(
            "E_REPO_UNREACHABLE",
            format!("{release_url} has no component `{component}`"),
        ));
    }
    let relative = format!("{component}/binary-{deb_arch}/Packages.xz");
    let (expected_sha, expected_size) =
        release.sha256.get(&relative).cloned().ok_or_else(|| {
            Diagnostic::new(
                "E_REPO_UNREACHABLE",
                format!("{release_url} lists no SHA-256 for {relative}"),
            )
        })?;
    let packages_path = format!("dists/{suite}/{relative}");
    let packages_url = format!("{url}{packages_path}");
    let compressed = fetcher.get(&packages_url).map_err(repo_error)?;
    let actual = sha256_hex(&compressed);
    if actual != expected_sha || compressed.len() as u64 != expected_size {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!(
                "{packages_url}: {release_path} lists sha256:{expected_sha} ({expected_size} bytes), got sha256:{actual} ({} bytes)",
                compressed.len()
            ),
        )
        .hint("the index does not match its Release file; do not use this mirror"));
    }
    let plain = xz_decompress(&compressed)
        .map_err(|e| Diagnostic::new("E_HASH_MISMATCH", format!("{packages_url}: {e}")))?;
    let plain_relative = format!("{component}/binary-{deb_arch}/Packages");
    if let Some((sha, size)) = release.sha256.get(&plain_relative)
        && (sha256_hex(&plain) != *sha || plain.len() as u64 != *size)
    {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!(
                "{packages_url}: the decompressed index does not match {plain_relative} in {release_path}"
            ),
        ));
    }
    let text = String::from_utf8(plain).map_err(|_| {
        Diagnostic::new("E_REPO_UNREACHABLE", format!("{packages_url} is not UTF-8"))
    })?;
    let packages = parse_packages(&text, deb_arch, repository, suite)
        .map_err(|e| Diagnostic::new("E_REPO_UNREACHABLE", format!("{packages_url}: {e}")))?;
    Ok((
        LockedIndex {
            suite: suite.to_string(),
            component: component.to_string(),
            release_path,
            release_sha256: sha256_hex(&release_bytes),
            packages_path,
            packages_sha256: actual,
            packages_size: expected_size,
        },
        packages,
    ))
}

/// The apt family is the one this build implements.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::distro::Family;

    #[test]
    fn this_family_answers_the_whole_seam() {
        assert!(Family::Apt.ops().is_ok());
        assert_eq!(Apt.package_arch("x86_64"), Some("amd64"));
        assert_eq!(Apt.package_arch("aarch64"), None);
        assert_eq!(Apt.package_mount(), "/lodi-debs");
        assert!(
            Apt.staged_package_name("zlib1g", "1:1.2.13")
                .ends_with(".deb")
        );
    }

    /// An apt read-back: the installed set alone, because this family records no base set.
    fn installed(output: &str) -> ReadBack<'_> {
        ReadBack {
            installed: output,
            recorded_base: None,
        }
    }

    fn entry(name: &str, version: &str, arch: Option<&str>) -> ClosureEntry {
        ClosureEntry {
            name: name.into(),
            version: version.into(),
            arch: arch.map(Into::into),
            origin: "base".into(),
            sha256: None,
            filename: None,
            size: None,
            repository: None,
            suite: None,
            epoch: None,
            release: None,
        }
    }

    #[test]
    fn the_installed_closure_must_equal_the_lock() {
        let lock = [
            entry("libc6", "2.36-9", Some("amd64")),
            entry("tzdata", "2025b-0", None),
        ];
        let same = "ii \tlibc6\t2.36-9\tamd64\nii \ttzdata\t2025b-0\tall\nrc \told\t1\tamd64\n";
        assert!(Apt.closure_differences(&installed(same), &lock).is_empty());
        let drift = "ii \tlibc6\t2.36-10\tamd64\niU \ttzdata\t2025b-0\tall\nii \textra\t1\tamd64\n";
        let d = Apt.closure_differences(&installed(drift), &lock);
        assert!(d.iter().any(|l| l.starts_with("changed: libc6")), "{d:?}");
        assert!(
            d.iter()
                .any(|l| l.contains("tzdata") && l.contains("state")),
            "{d:?}"
        );
        assert!(d.iter().any(|l| l == "added: extra 1"), "{d:?}");
        assert!(d.iter().any(|l| l.starts_with("missing: tzdata")), "{d:?}");
        let arch = "ii \tlibc6\t2.36-9\ti386\nii \ttzdata\t2025b-0\tall\n";
        assert_eq!(Apt.closure_differences(&installed(arch), &lock).len(), 1);
    }
}
