//! Arch's dated repository configuration, resolved exclusively from the embedded base.
use crate::diag::Diagnostic;
use crate::hostscope::plan::ArchFile;
use crate::hostscope::safety::Distro;
use crate::util::parse_utc;
use std::collections::BTreeMap;
use std::io::Read;

/// A settled unrecorded Arch date can be reused from the machine's *last finished action*,
/// never from `host.lock` as a pin. The journal witnesses that this exact manifest already
/// resolved the immutable archive: no new schema or pin outside the host directory is written.
/// A changed manifest, missing or unfinished journal, or machine drift forces normal resolution.
/// The record is consulted only to check what is installed, not to choose a new version.
pub fn reuse_completed(
    gate: &crate::hostscope::safety::Gate,
    machine: &crate::hostscope::lock::HostLock,
    digest: &str,
    source: &str,
    snapshot: Option<&str>,
    pins: &BTreeMap<String, super::Declared>,
) -> Option<super::Resolution> {
    if pins.is_empty() || machine.source.as_deref()? != source {
        return None;
    }
    let observed = crate::hostscope::pm::backend_for(
        gate.distro,
        &gate.root,
        gate.partial_upgrade,
        gate.operation,
    )
    .observe()
    .ok()?;
    if machine
        .packages
        .iter()
        .any(|(name, record)| observed.installed.get(name) != Some(&record.version))
    {
        return None;
    }
    let dir = crate::hostscope::journal::journal_dir(gate);
    let latest = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|s| s == "json"))
        .max()?;
    if !std::fs::symlink_metadata(&latest).ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(latest)
        .ok()?
        .take(16 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() > 16 * 1024 * 1024 {
        return None;
    }
    let header: crate::hostscope::journal::Header =
        serde_json::from_str(text.lines().next()?).ok()?;
    if header.version != crate::hostscope::journal::VERSION
        || header.root != gate.root.display().to_string()
        || header.distro != "arch"
        || header.manifest_digest != digest
        || !matches!(
            serde_json::from_str::<crate::hostscope::journal::Event>(text.lines().last()?).ok()?,
            crate::hostscope::journal::Event::Finished { .. }
        )
    {
        return None;
    }
    let mut resolution = super::Resolution::default();
    if let Some(requested) = snapshot {
        let instant = parse_utc(requested)?;
        let day = instant - instant.rem_euclid(86400);
        super::check_bounds(
            Distro::Arch,
            "rolling",
            "[host] snapshot",
            day,
            crate::util::now_utc(),
        )
        .ok()?;
        resolution.snapshot = Some(super::Snapshot {
            requested: requested.to_string(),
            instant: day,
            indexes: BTreeMap::new(),
            recorded: false,
        });
    }
    for (name, request) in pins {
        let super::Request::Date(instant) = request.request else {
            return None;
        };
        let day = instant - instant.rem_euclid(86400);
        super::check_bounds(Distro::Arch, "rolling", name, day, crate::util::now_utc()).ok()?;
        let record = machine.packages.get(name)?;
        if !record.held {
            return None;
        }
        let marker = format!("package {name} (pinned ");
        let (version, origin) = header.actions.iter().find_map(|action| {
            let (_, tail) = action.summary.split_once(&marker)?;
            let (version, tail) = tail.split_once(" from ")?;
            let (repo, at) = tail.split_once(" at ")?;
            let date = at.split([';', ')']).next()?;
            (parse_utc(date) == Some(day)
                && matches!(repo, "core" | "extra")
                && version == record.version
                && crate::arch::version::PacmanVersion::parse(version).is_ok())
            .then(|| (version.to_string(), repo.to_string()))
        })?;
        resolution.pins.insert(
            name.clone(),
            super::Resolved {
                name: name.clone(),
                requested: request.requested.clone(),
                policy: "date",
                version,
                repository: origin,
                snapshot: Some(day),
                sha256: String::new(),
                filename: String::new(),
                recorded: false,
                source: None,
            },
        );
    }
    Some(resolution)
}

/// The archive-relative filename is a single safe basename, never a path supplied to the
/// filesystem without validation. A stale or malformed pins.lock cannot escape the stage.
pub fn file(pin: &super::Resolved) -> Result<ArchFile, Diagnostic> {
    let path = &pin.filename;
    if !safe_file_name(path) {
        return Err(Diagnostic::new(
            "E_LOCK_VERSION",
            format!("unsafe Arch pin filename for {}", pin.name),
        ));
    }
    let instant = pin.snapshot.ok_or_else(|| {
        Diagnostic::new(
            "E_UNSUPPORTED",
            "an Arch package file needs a dated archive",
        )
    })?;
    let repository = super::dated_repositories(Distro::Arch, "rolling", instant)?
        .into_iter()
        .find(|repo| repo.name == pin.repository)
        .ok_or_else(|| {
            Diagnostic::new(
                "E_LOCK_VERSION",
                format!("unknown Arch pin repository {}", pin.repository),
            )
        })?;
    if !pin
        .sha256
        .strip_prefix("sha256:")
        .is_some_and(crate::debian::index::is_sha256_hex)
    {
        return Err(Diagnostic::new(
            "E_LOCK_VERSION",
            "invalid Arch pin SHA-256",
        ));
    }
    Ok(ArchFile {
        name: pin.name.clone(),
        url: format!("{}{}", repository.url, path),
        digest: pin.sha256.clone(),
        path: path.clone(),
    })
}

/// A single safe package basename: no path, no leading dot, the archive's own suffix.
pub fn safe_file_name(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('.')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
        && path.ends_with(".pkg.tar.zst")
}

/// `pin.conf` naming a private keyring (`GPGDir`) in place of the machine's (#170).
pub fn with_gpgdir(config: &str, gpgdir: &std::path::Path) -> String {
    config.replacen(
        "[options]\n",
        &format!("[options]\nGPGDir = {}\n", gpgdir.display()),
        1,
    )
}

/// Pacman gets this private file with `--config`; `/etc/pacman.conf` is never edited.
pub fn render_config(day: &str) -> Result<String, Diagnostic> {
    let instant =
        parse_utc(day).ok_or_else(|| Diagnostic::new("E_TYPE", "invalid Arch snapshot day"))?;
    let repositories = super::dated_repositories(Distro::Arch, "rolling", instant)?;
    let mut out = String::from(
        "[options]\nArchitecture = x86_64\nSigLevel = Required DatabaseOptional\n\
             LocalFileSigLevel = Required\n",
    );
    for repository in repositories {
        out.push_str(&format!(
            "\n[{}]\nServer = {}\n",
            repository.name, repository.url
        ));
    }
    Ok(out)
}
