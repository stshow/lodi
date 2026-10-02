//! The archived keyring of #170 (LD-451): an explicit opt-in, `[packages.arch]
//! archived_keyring = true`, for a dated Arch pin whose signer the machine's keyring no longer
//! holds. The `archlinux-keyring` of the pin's own day is fetched from its dated core, held to
//! core.db's SHA-256, and verified by the machine's current trust — read from a private copy, so
//! gpg writes nothing into `/etc/pacman.d/gnupg`. Its keys then fill a private keyring below the
//! pin stage that only `pin.conf` names (`GPGDir`). Package signatures stay required throughout
//! (LD-441 U1); nothing here accepts a package by its hash alone.
use crate::diag::Diagnostic;
use crate::hostscope::pm;
use crate::hostscope::safety::{Distro, Gate};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// The keyring files `pacman-key --populate archlinux` reads.
const KEYRING_FILES: [&str; 3] = ["archlinux.gpg", "archlinux-trusted", "archlinux-revoked"];

/// The issuer of a detached v4 OpenPGP signature, as gpg prints it, for a refusal to name.
/// Pacman, not this reader, decides whether the signature is valid and trusted.
pub fn signing_key(signature: &[u8]) -> Option<String> {
    let (&tag, rest) = signature.split_first()?;
    let (length, rest) = if tag & 0x40 != 0 {
        new_length(rest)?
    } else {
        let width = [1, 2, 4].get(usize::from(tag & 3))?;
        let bytes = rest.get(..*width)?;
        let length = bytes
            .iter()
            .fold(0, |n: usize, b| (n << 8) | usize::from(*b));
        (length, &rest[*width..])
    };
    let body = rest.get(..length)?;
    if body.first() != Some(&4) {
        return None;
    }
    let hashed = usize::from(u16::from_be_bytes([*body.get(4)?, *body.get(5)?]));
    let at = 6 + hashed;
    let unhashed = usize::from(u16::from_be_bytes([*body.get(at)?, *body.get(at + 1)?]));
    issuer(body.get(6..at)?).or_else(|| issuer(body.get(at + 2..at + 2 + unhashed)?))
}

/// An OpenPGP new-format length (RFC 4880 §4.2.2), and what follows it.
fn new_length(bytes: &[u8]) -> Option<(usize, &[u8])> {
    let first = usize::from(*bytes.first()?);
    match first {
        0..192 => Some((first, &bytes[1..])),
        192..255 => Some((
            ((first - 192) << 8) + usize::from(*bytes.get(1)?) + 192,
            &bytes[2..],
        )),
        _ => {
            let length = u32::from_be_bytes(bytes.get(1..5)?.try_into().ok()?);
            Some((length as usize, &bytes[5..]))
        }
    }
}

/// The issuer fingerprint (subpacket 33) or key ID (16) of a subpacket area.
fn issuer(mut area: &[u8]) -> Option<String> {
    while !area.is_empty() {
        let (length, rest) = new_length(area)?;
        let packet = rest.get(..length)?;
        area = &rest[length..];
        let hex = |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("{b:02X}")).collect() };
        // The high bit of a subpacket type only marks it critical.
        match packet {
            [t, 4, key @ ..] if t & 0x7f == 33 && key.len() == 20 => return Some(hex(key)),
            [t, key @ ..] if t & 0x7f == 16 && key.len() == 8 => return Some(hex(key)),
            _ => {}
        }
    }
    None
}

/// Prepare the private keyring for the pin day `day` inside the stage `dir`, and return it.
/// `recorded` is the core.db digest the pin was resolved against, when there is one.
pub fn prepare(
    gate: &Gate,
    day: i64,
    recorded: Option<&String>,
    dir: &Path,
    config: &Path,
    fetcher: &dyn crate::fetch::Fetcher,
) -> Result<PathBuf, Diagnostic> {
    let date = &crate::util::format_utc(day)[..10];
    let package = fetch_keyring(day, date, recorded, dir, fetcher)?;
    let current = dir.join("current-trust");
    copy_current_trust(&gate.root, &current)?;
    let tool = |name: &str, gpgdir: &Path, action: &[&str]| -> Result<pm::Invocation, _> {
        let mut args = vec![
            "--config".to_string(),
            config.display().to_string(),
            "--gpgdir".to_string(),
            gpgdir.display().to_string(),
        ];
        args.extend(action.iter().map(|a| (*a).to_string()));
        program(gate, name, &args)
    };
    let file = package.display().to_string();
    let signature = format!("{file}.sig");
    let verify = tool("pacman-key", &current, &["--verify", &signature, &file])?;
    pm::run(&verify).map_err(|_| {
        Diagnostic::new(
            "E_PIN_UNTRUSTED",
            format!(
                "the archlinux-keyring of {date} is not signed by a key this machine trusts; no \
                 archived key was enabled"
            ),
        )
        .hint(
            "nothing was installed; without a trusted archived keyring this pin cannot be verified",
        )
    })?;
    let files = dir.join("keyring-files");
    private_dir(&files)?;
    for name in KEYRING_FILES {
        let member = format!("usr/share/pacman/keyrings/{name}");
        let args = ["-xOf".to_string(), package.display().to_string(), member];
        let output = program(gate, "bsdtar", &args)?
            .command()
            .output()
            .map_err(|e| {
                Diagnostic::new("E_APPLY", format!("cannot read archlinux-keyring: {e}"))
            })?;
        if !output.status.success() {
            return Err(Diagnostic::new(
                "E_PIN_UNTRUSTED",
                format!(
                    "the archlinux-keyring of {date} has no {name}; no archived key was enabled"
                ),
            ));
        }
        std::fs::write(files.join(name), output.stdout)
            .map_err(|e| Diagnostic::new("E_STORE_IO", format!("cannot stage {name}: {e}")))?;
    }
    let gpgdir = dir.join("archived-trust");
    private_dir(&gpgdir)?;
    let from = files.display().to_string();
    for action in [
        &["--init"][..],
        &["--populate-from", &from, "--populate", "archlinux"],
    ] {
        pm::run(&tool("pacman-key", &gpgdir, action)?).map_err(|e| {
            Diagnostic::new(
                "E_APPLY",
                format!("cannot fill the private archived keyring: {}", e.message),
            )
        })?;
    }
    Ok(gpgdir)
}

/// The signed `archlinux-keyring` of the pin day's core, staged with its detached signature.
fn fetch_keyring(
    day: i64,
    date: &str,
    recorded: Option<&String>,
    dir: &Path,
    fetcher: &dyn crate::fetch::Fetcher,
) -> Result<PathBuf, Diagnostic> {
    let repository = super::dated_repositories(Distro::Arch, "rolling", day)?
        .into_iter()
        .find(|repo| repo.name == "core")
        .ok_or_else(|| Diagnostic::new("E_REPO_UNREACHABLE", "no dated Arch core"))?;
    let (index, packages) = crate::arch::base::fetch_index(fetcher, &repository)?;
    if recorded.is_some_and(|want| *want != format!("sha256:{}", index.packages_sha256)) {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!("the dated core.db of {date} differs from lodi.lock; no keyring was used"),
        ));
    }
    let keyring = packages
        .iter()
        .find(|package| package.name == "archlinux-keyring")
        .filter(|package| super::arch::safe_file_name(&package.filename))
        .ok_or_else(|| {
            Diagnostic::new(
                "E_REPO_UNREACHABLE",
                format!("the dated core of {date} has no archlinux-keyring"),
            )
        })?;
    let url = format!("{}{}", repository.url, keyring.filename);
    let bytes = fetcher.get(&url).map_err(crate::debian::base::repo_error)?;
    if crate::util::sha256_hex(&bytes) != keyring.sha256 {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!("the archlinux-keyring of {date} differs from its core.db; none was used"),
        ));
    }
    let signature = fetcher
        .get(&format!("{url}.sig"))
        .map_err(crate::debian::base::repo_error)?;
    let path = dir.join(&keyring.filename);
    for (target, contents) in [
        (path.clone(), bytes),
        (PathBuf::from(format!("{}.sig", path.display())), signature),
    ] {
        std::fs::write(&target, contents).map_err(|e| {
            Diagnostic::new(
                "E_STORE_IO",
                format!("cannot stage {}: {e}", target.display()),
            )
        })?;
    }
    Ok(path)
}

/// A copy of the machine's current Arch trust, for gpg to read and write instead of the original.
fn copy_current_trust(root: &Path, to: &Path) -> Result<(), Diagnostic> {
    let from = root.join("etc/pacman.d/gnupg");
    private_dir(to)?;
    let mut public = false;
    for name in ["pubring.kbx", "pubring.gpg", "trustdb.gpg", "gpg.conf"] {
        let source = from.join(name);
        match std::fs::symlink_metadata(&source) {
            Ok(meta) if meta.is_file() => {
                std::fs::copy(&source, to.join(name)).map_err(|e| {
                    Diagnostic::new(
                        "E_STORE_IO",
                        format!("cannot copy {}: {e}", source.display()),
                    )
                })?;
                public |= name.starts_with("pubring");
            }
            Ok(_) => {
                return Err(Diagnostic::new(
                    "E_PATH_ESCAPE",
                    format!("{} is not a regular file", source.display()),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Diagnostic::new(
                    "E_STORE_IO",
                    format!("cannot read {}: {e}", source.display()),
                ));
            }
        }
    }
    if !public {
        return Err(Diagnostic::new(
            "E_PIN_UNTRUSTED",
            format!(
                "{} holds no public keys to verify the archived keyring",
                from.display()
            ),
        ));
    }
    Ok(())
}

fn private_dir(path: &Path) -> Result<(), Diagnostic> {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|e| {
            Diagnostic::new(
                "E_STORE_IO",
                format!("cannot create {}: {e}", path.display()),
            )
        })
}

/// A program the opt-in needs beside pacman, resolved as the gate resolves pacman itself.
fn program(gate: &Gate, name: &str, args: &[String]) -> Result<pm::Invocation, Diagnostic> {
    let path = crate::hostscope::safety::resolve_program(name, gate.system_root, gate.euid)
        .map_err(|why| crate::hostscope::safety::untrusted_runtime(name, &why))?
        .ok_or_else(|| {
            Diagnostic::new(
                "E_NO_RUNTIME",
                format!("{name} is needed for [packages.arch] archived_keyring"),
            )
        })?;
    Ok(pm::Invocation::resolved(
        name,
        &path,
        args,
        pm::noninteractive_env(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured signature of `tree` 2.3.2-1 (an old-format packet) and of `bc` 1.08.2-1.
    #[test]
    fn signing_key_reads_real_arch_signatures() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/host/pin/archive/archive.archlinux.org/",
            "repos/2026/09/01/extra/os/x86_64/"
        );
        let tree = std::fs::read(format!("{dir}tree-2.3.2-1-x86_64.pkg.tar.zst.sig")).unwrap();
        assert_eq!(
            signing_key(&tree).as_deref(),
            Some("E499C79F53C96A54E572FEE1C06086337C50773E")
        );
        let bc = std::fs::read(format!("{dir}bc-1.08.2-1-x86_64.pkg.tar.zst.sig")).unwrap();
        assert_eq!(signing_key(&bc).map(|key| key.len()), Some(40));
        assert_eq!(signing_key(b""), None);
        assert_eq!(signing_key(&tree[..20]), None);
    }
}
