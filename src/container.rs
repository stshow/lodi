//! Container environments through rootless Podman (M-Spike S-4; design ADR-012, `spec/08` §4,
//! §5, §7 and §8, `spec/01` §3.6).
//!
//! `lodi develop -- …` and `lodi run …` for a `[container]` manifest continue here after the
//! frozen lock and the trust gate of [`crate::host::enter`]:
//!
//! 1. nesting: a container environment is entered only from outside every Lodi environment
//!    (`E_NESTED`, exit 7, otherwise);
//! 2. the runtime: `podman` must be on `PATH`; Lodi never installs it (ADR-012) and names the
//!    command that would (`E_NO_RUNTIME`, exit 7), before anything is downloaded;
//! 3. realization under the shared store lock: the tools' `art-` entries and the `env-` entry
//!    as in host mode, then the environment image `localhost/lodi-env:<h32>`:
//!    - warm: `store/.meta/img-<h32>.json` says `complete` and Podman's image under that tag
//!      has the recorded image ID (a content digest). Nothing is downloaded or rebuilt.
//!    - otherwise it is rebuilt from verified inputs only: the locked rootfs layer and every
//!      `install` package are downloaded into `cache/dl/` and checked against the lock's size
//!      and SHA-256 (a cached file is re-hashed first); the rootfs is imported under a
//!      temporary tag; the generated `Containerfile` and package configuration of `spec/08`
//!      §4 come from the base's package-manager family (`src/distro/`), which installs exactly
//!      those verified package files while the build has **no network** (`--network=none`), so
//!      nothing unverified can enter; the installed package
//!      database is read back with the family's own query and must equal the lock's closure (name,
//!      version and architecture of every package, `E_CLOSURE_DRIFT`, exit 6, otherwise, and
//!      the image is removed); only then is the sidecar with the image ID written, last.
//! 4. a live-session root names the entries and the image while the container runs;
//! 5. `podman run --rm` with the runtime settings of `spec/01` §3.6 this build supports (see
//!    [`run_arguments`]); Lodi waits, forwards `SIGTERM`/`SIGHUP`, returns the status, and
//!    removes the container if it still exists (for example when Podman itself was killed).
//!
//! Root inside the build and in the image is root of the rootless user namespace only; no host
//! privilege, package source, security setting, global environment or shell file is changed.
//! Podman records no events and stores no container output (`--events-backend=none`,
//! `--log-driver=none`): its defaults would write both into the system journal (LD-24). Its
//! temporary image copies stay inside `LODI_HOME`: every Podman command of an entry runs with
//! `TMPDIR` = `store/tmp/<pid>-podman`, which Podman documents as the override of
//! `image_copy_tmp_dir` (containers.conf(5), default `/var/tmp`), removed when the entry ends
//! (LD-25).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::catalogue::RepositoryDefinition;
use crate::diag::Diagnostic;
use crate::distro::{Family, ImageOps, ReadBack};
use crate::fetch::Fetcher;
use crate::host::{self, Activation, Options, PROFILE, fail};
use crate::lock::{BaseEntry, ClosureEntry, Failure, LockFile, canonical_json};
use crate::manifest::ProjectManifest;
use crate::store::{Report, Store};
use crate::util::{format_utc, now_utc, sha256_tagged, untag_sha256};

/// The runtime of this module's environments (`LODI_MODE`, activation identity).
pub const RUNTIME: &str = "container";
/// The versioned protocol of an environment image's identity.
pub const IMAGE_FORMAT: &str = "lodi-spike-image/1";
/// Repository of environment images in Podman's storage.
pub const IMAGE_REPO: &str = "localhost/lodi-env";
/// Repository of the temporary base images a build starts from.
pub const BASE_REPO: &str = "localhost/lodi-tmp-base";
/// `PATH` of the container before the environment's `tree/bin` (Debian's default).
pub const CONTAINER_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Lines of the build log shown with `E_LAYER_BUILD`.
const LOG_TAIL_LINES: usize = 40;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The `podman` program Lodi drives.
#[derive(Debug, Clone)]
pub struct Podman {
    program: PathBuf,
    /// `TMPDIR` of every Podman command: where Podman keeps its temporary image copies.
    tmpdir: Option<PathBuf>,
}

impl Podman {
    /// Find `podman` on `path` (the caller's `PATH`). Missing: `E_NO_RUNTIME` with the command
    /// that installs it on this distribution (ADR-012: Lodi never installs it).
    pub fn find(path: Option<&OsStr>) -> Result<Podman, Diagnostic> {
        let found = path.and_then(|p| {
            std::env::split_paths(p)
                .map(|dir| dir.join("podman"))
                .find(|c| fs::metadata(c).is_ok_and(|m| m.is_file() && m.mode_exec()))
        });
        found
            .map(|program| Podman {
                program,
                tmpdir: None,
            })
            .ok_or_else(|| {
                Diagnostic::new(
                    "E_NO_RUNTIME",
                    "this manifest selects a container environment ([container]), which needs \
                 rootless Podman, and `podman` is not on PATH; nothing was downloaded or run",
                )
                .hint(format!(
                    "install it once with root privileges, for example `{}`; Lodi does not install \
                 it and changes no host package or security setting (docs: README, Container \
                 environments)",
                    install_command()
                ))
            })
    }

    /// Keep Podman's temporary image copies in `store/tmp/<pid>-podman` of `store` (created
    /// here, private to the user): Podman takes `TMPDIR` over `image_copy_tmp_dir`, whose
    /// default `/var/tmp` is outside `LODI_HOME` (LD-25). A directory left by a process that no
    /// longer exists is removed by [`Store::prune_staging`].
    pub fn keep_temporary_files_in(&mut self, store: &Store) -> Result<PathBuf, Diagnostic> {
        let dir = store
            .home()
            .join("store/tmp")
            .join(format!("{}-podman", std::process::id()));
        fs::create_dir_all(&dir)
            .and_then(|()| fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)))
            .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", dir.display())))?;
        self.tmpdir = Some(dir.clone());
        Ok(dir)
    }

    /// Every Podman command of Lodi: `--events-backend=none` before the subcommand, and the
    /// temporary directory in `TMPDIR`.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.arg(EVENTS);
        if let Some(dir) = &self.tmpdir {
            command.env("TMPDIR", dir);
        }
        command
    }

    /// The full argv of a Podman command that Lodi spawns itself (the `podman run` of an
    /// entry): the program, `--events-backend=none`, then `args`.
    pub fn argv(&self, args: Vec<OsString>) -> Vec<OsString> {
        let mut full = vec![self.program.clone().into_os_string(), EVENTS.into()];
        full.extend(args);
        full
    }

    /// The environment of such a command: `parent` with the temporary directory in `TMPDIR`
    /// (the container's own environment is given with `--env` only, so it does not see it).
    pub fn environment(
        &self,
        parent: &BTreeMap<OsString, OsString>,
    ) -> BTreeMap<OsString, OsString> {
        let mut env = parent.clone();
        if let Some(dir) = &self.tmpdir {
            env.insert("TMPDIR".into(), dir.clone().into_os_string());
        }
        env
    }

    /// Run podman with `args` and capture its output; a podman that cannot be started is
    /// `E_ENTER`.
    fn output<I, S>(&self, args: I) -> Result<Output, Diagnostic>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| {
                Diagnostic::new(
                    "E_ENTER",
                    format!("cannot run {}: {e}", self.program.display()),
                )
            })
    }

    /// Run podman with `args`; a non-zero exit is `E_ENTER` naming `what` and podman's stderr.
    fn checked<I, S>(&self, what: &str, args: I) -> Result<Vec<u8>, Diagnostic>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.output(args)?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(podman_failed(what, &out))
        }
    }

    /// The image ID under `tag`, `None` when there is no such image. Any other failure of
    /// Podman (a broken storage, a missing runtime) is `E_ENTER`.
    pub fn image_id(&self, tag: &str) -> Result<Option<String>, Diagnostic> {
        let out = self.output(["image", "exists", tag])?;
        match out.status.code() {
            Some(0) => {}
            Some(1) => return Ok(None),
            _ => return Err(podman_failed("checking the environment image", &out)),
        }
        let id = self.checked(
            "inspecting the environment image",
            ["image", "inspect", "--format", "{{.Id}}", tag],
        )?;
        Ok(Some(String::from_utf8_lossy(&id).trim().to_string()))
    }

    /// Every **tagged** image in Podman's storage, as `<repository>:<tag>` with its image ID.
    /// Untagged images are left out: an image with no name at all can never be proven Lodi's
    /// by the exact tag its own code writes (LD-31 with LD-32), so `lodi gc` never names one.
    pub fn list_images(&self) -> Result<Vec<ImageEntry>, Diagnostic> {
        let out = self.checked(
            "listing the images",
            ["images", "--format", "{{.Repository}}:{{.Tag}}\t{{.ID}}"],
        )?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .filter(|(tag, _)| !tag.contains("<none>"))
            .map(|(tag, id)| ImageEntry {
                tag: tag.trim().to_string(),
                id: id.trim().to_string(),
            })
            .collect())
    }

    /// Every container, running or not, with the image it uses.
    pub fn list_containers(&self) -> Result<Vec<ContainerEntry>, Diagnostic> {
        let out = self.checked(
            "listing the containers",
            [
                "ps",
                "--all",
                "--format",
                "{{.Names}}\t{{.ImageID}}\t{{.Image}}",
            ],
        )?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                Some(ContainerEntry {
                    name: fields.next()?.trim().to_string(),
                    image_id: fields.next()?.trim().to_string(),
                    image: fields.next()?.trim().to_string(),
                })
            })
            .collect())
    }

    /// Untag exactly `tag`. Never `--force`: an image a container still uses is kept, which is
    /// the ownership rule of LD-32 rather than a failure to work around.
    pub fn remove_image(&self, tag: &str) -> Result<(), Diagnostic> {
        self.checked("removing an environment image", ["rmi", tag])?;
        Ok(())
    }

    /// Whether a container called `name` exists (running or not).
    pub fn container_exists(&self, name: &str) -> bool {
        self.output(["container", "exists", name])
            .is_ok_and(|o| o.status.success())
    }

    /// Stop and remove container `name` if it exists.
    pub fn remove_container(&self, name: &str) {
        if self.container_exists(name) {
            let _ = self.output(["rm", "--force", "--ignore", "--time", "0", name]);
        }
    }
}

/// One tagged image of Podman's storage, as [`Podman::list_images`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageEntry {
    /// `<repository>:<tag>`, exactly as Podman prints it.
    pub tag: String,
    pub id: String,
}

/// One container of Podman's storage, as [`Podman::list_containers`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerEntry {
    pub name: String,
    pub image_id: String,
    /// The image reference the container was created from, as Podman prints it.
    pub image: String,
}

/// Every Podman command of Lodi records no events: Podman's default event logger is the system
/// journal, a write outside the ADR-018 boundary (LD-24). Given before the subcommand.
const EVENTS: &str = "--events-backend=none";

trait ModeExec {
    fn mode_exec(&self) -> bool;
}

impl ModeExec for fs::Metadata {
    fn mode_exec(&self) -> bool {
        self.permissions().mode() & 0o111 != 0
    }
}

fn podman_failed(what: &str, out: &Output) -> Diagnostic {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let last = stderr.lines().rev().find(|l| !l.trim().is_empty());
    Diagnostic::new(
        "E_ENTER",
        format!(
            "podman failed while {what} ({}){}",
            describe_status(out.status),
            last.map_or(String::new(), |l| format!(": {}", l.trim()))
        ),
    )
    .hint("check that rootless Podman works for this user: `podman info`")
}

fn describe_status(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(c), _) => format!("exit status {c}"),
        (None, Some(s)) => format!("signal {s}"),
        _ => "unknown status".into(),
    }
}

/// The command that installs Podman here, from `/etc/os-release` (informational only).
pub fn install_command() -> String {
    let text = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .map(|v| v.trim_matches('"').to_string())
            .unwrap_or_default()
    };
    let ids = format!("{} {}", field("ID"), field("ID_LIKE"));
    let has = |id: &str| ids.split_whitespace().any(|w| w == id);
    if has("arch") {
        "sudo pacman -S podman".into()
    } else if has("debian") || has("ubuntu") {
        "sudo apt-get install podman".into()
    } else {
        "your distribution's podman package".into()
    }
}

/// The package-manager family that wrote this lock's base, read from the shape of its
/// repositories (`src/distro/`): an apt repository is addressed by suite and component.
/// `lock::validate` refuses a base whose distribution this build cannot write before anything
/// here reads it, and every family this project knows has an image half, so a base that reaches
/// this module and whose repositories name a family it knows can be built. A shape that names
/// no family at all is `None` and is refused where it matters.
fn family(base: &BaseEntry) -> Option<&'static dyn ImageOps> {
    // A distribution this build defines is built by its definition's own family: two families'
    // repositories have the same bare shape, so the shape alone cannot tell them apart.
    if let Some(Ok(definition)) = crate::catalogue::builtin_base(&base.distro) {
        return Some(definition.family.image_ops());
    }
    let shape: BTreeMap<String, RepositoryDefinition> = base
        .repositories
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                RepositoryDefinition {
                    name: r.name.clone(),
                    snapshot_url: r.url.clone(),
                    suites: r.suites.clone(),
                    components: r.components.clone(),
                },
            )
        })
        .collect();
    Some(Family::derive(&shape)?.image_ops())
}

/// The family, or `E_UNSUPPORTED` naming the base — the fallible paths of an image build.
fn family_or_refuse(base: &BaseEntry) -> Result<&'static dyn ImageOps, Diagnostic> {
    family(base).ok_or_else(|| {
        Diagnostic::new(
            "E_UNSUPPORTED",
            format!(
                "this build cannot build a {} {} image",
                base.distro, base.release
            ),
        )
    })
}

/// The configuration the image build writes (`spec/08` §4 step 2) and the `Containerfile` that
/// installs the verified packages, rendered by the base's family. Pure functions of the lock;
/// an unimplemented family renders nothing, which the fallible paths refuse before they get here.
fn build_files(base: &BaseEntry, has_packages: bool) -> BTreeMap<&'static str, String> {
    family(base).map_or_else(BTreeMap::new, |ops| ops.build_files(base, has_packages))
}

/// The SHA-256 of every generated build file, by name: the surface `image_identity` hashes and
/// the surface `tests/distro_seam.rs` pins, so that a change to what an image build writes
/// cannot happen silently (M-Arch-Base T-1).
pub fn build_file_digests(base: &BaseEntry, has_packages: bool) -> BTreeMap<String, String> {
    build_files(base, has_packages)
        .iter()
        .map(|(k, v)| ((*k).to_string(), sha256_tagged(v.as_bytes())))
        .collect()
}

/// The identity of an environment image: the canonical hash of the image protocol, the locked
/// rootfs, the closure (its hash) and every generated build file. Same lock, same identity.
pub fn image_identity(base: &BaseEntry) -> String {
    let files = build_files(base, has_install(base));
    let digests: BTreeMap<&str, String> = files
        .iter()
        .map(|(k, v)| (*k, sha256_tagged(v.as_bytes())))
        .collect();
    sha256_tagged(
        canonical_json(&json!({
            "format": IMAGE_FORMAT,
            "distro": base.distro,
            "release": base.release,
            "arch": base.arch,
            "rootfs": base.rootfs.sha256,
            "closure": base.closure.hash,
            "files": digests,
        }))
        .as_bytes(),
    )
}

fn has_install(base: &BaseEntry) -> bool {
    base.closure.packages.iter().any(|p| p.origin == "install")
}

fn h32(tagged: &str) -> &str {
    let hex = untag_sha256(tagged).unwrap_or(tagged);
    &hex[..32.min(hex.len())]
}

/// The environment image of `base`: its tag and its `img-` record name.
pub fn image_names(base: &BaseEntry) -> (String, String) {
    let identity = image_identity(base);
    let h = h32(&identity).to_string();
    (format!("{IMAGE_REPO}:{h}"), format!("img-{h}"))
}

/// The record of an image Lodi built and verified (`store/.meta/img-<h32>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageMeta {
    /// The record's schema version. `#[serde(default)]` because releases through 0.3.0 wrote
    /// this record without it, in exactly this shape (M-1.0 T-1, design call D5).
    #[serde(default = "image_record_version")]
    pub version: u64,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub identity: String,
    pub tag: String,
    pub image_id: String,
    pub closure_hash: String,
    pub packages: usize,
    pub created: String,
    pub complete: bool,
    pub lodi_version: String,
}

/// The registry row the image record is registered as.
pub const IMAGE_RECORD_ARTIFACT: &str = "image-record";

fn image_record_version() -> u64 {
    crate::schema::artifact(IMAGE_RECORD_ARTIFACT).writes
}

/// What realizing the image did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageOutcome {
    pub meta: ImageMeta,
    /// The image was built in this invocation (not warm).
    pub built: bool,
}

/// Read the complete record `name`, if any.
pub fn image_record(store: &Store, name: &str) -> Option<ImageMeta> {
    // A record whose schema version this build does not read is not a warm image: the image is
    // rebuilt from verified inputs rather than trusted (M-1.0 T-1, design call D18).
    let meta: ImageMeta = crate::schema::parse_registered(
        IMAGE_RECORD_ARTIFACT,
        &fs::read(store.meta_path(name)).ok()?,
    )?;
    (meta.complete && meta.name == name).then_some(meta)
}

/// The URL of an `install` package: its repository's URL and its `filename`.
fn package_url(base: &BaseEntry, p: &ClosureEntry) -> Result<String, Diagnostic> {
    let repo_name = p.repository.as_deref().unwrap_or_default();
    let repo = base
        .repositories
        .iter()
        .find(|r| r.name == repo_name)
        .ok_or_else(|| {
            Diagnostic::new(
                "E_LOCK_STALE",
                format!(
                    "package {} names repository `{repo_name}`, which the lock does not list",
                    p.name
                ),
            )
        })?;
    let filename = p.filename.as_deref().unwrap_or_default();
    Ok(format!("{}{filename}", repo.url))
}

/// Read the installed closure of image `tag` and compare it with the lock.
fn verify_closure(podman: &Podman, tag: &str, base: &BaseEntry) -> Result<(), Diagnostic> {
    let ops = family_or_refuse(base)?;
    let out = podman.checked(
        "reading the installed packages",
        ops.closure_query_arguments(tag),
    )?;
    let installed = String::from_utf8_lossy(&out).into_owned();
    // A family whose rootfs publishes no package list has its build record what the rootfs
    // brought, inside the image. A failed or empty read of that record is not an error here: the
    // family reports it as a difference, which discards the image like any other drift.
    let recorded = match ops.recorded_base_arguments(tag) {
        None => None,
        Some(arguments) => podman
            .output(arguments)
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned()),
    };
    let read_back = ReadBack {
        installed: &installed,
        recorded_base: recorded.as_deref(),
    };
    let differences = ops.closure_differences(&read_back, &base.closure.packages);
    if differences.is_empty() {
        return Ok(());
    }
    let shown: Vec<&String> = differences.iter().take(20).collect();
    let mut d = Diagnostic::new(
        "E_CLOSURE_DRIFT",
        format!(
            "the installed packages differ from the lock's closure ({} differences); the image \
             was discarded",
            differences.len()
        ),
    );
    for line in shown {
        d.notes.push(format!("  {line}"));
    }
    Err(d.hint("entering never changes the lock; run `lodi update` to resolve it again"))
}

/// The rootfs format of a released OCI image archive (`release_image`, LD-435).
const OCI_ARCHIVE: &str = "oci-archive.tar.xz";
/// The largest decompressed OCI image archive accepted.
const MAX_OCI_ARCHIVE_BYTES: usize = 1 << 30;

/// For a rootfs that is a released OCI image archive, the one layer of the image manifest the
/// lock pins, unpacked from the verified archive into `work` and held to the digest that
/// manifest states; `None` for a rootfs that is already a layer. The archive's own SHA-256 was
/// checked on download; this checks that it is the image the lock names, not only the bytes.
fn oci_archive_layer(
    base: &BaseEntry,
    archive: &Path,
    work: &Path,
) -> Result<Option<PathBuf>, Diagnostic> {
    if base.rootfs.format != OCI_ARCHIVE {
        return Ok(None);
    }
    let bad =
        |why: String| Diagnostic::new("E_ARCHIVE_FORMAT", format!("{}: {why}", base.rootfs.url));
    let manifest = base
        .rootfs
        .oci_manifest
        .as_deref()
        .and_then(untag_sha256)
        .ok_or_else(|| bad("the lock pins no OCI image manifest".into()))?;
    let compressed = fs::read(archive)
        .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", archive.display())))?;
    let mut plain = Vec::new();
    lzma_rs::xz_decompress(
        &mut compressed.as_slice(),
        &mut Capped {
            out: &mut plain,
            limit: MAX_OCI_ARCHIVE_BYTES,
        },
    )
    .map_err(|e| bad(format!("not an xz archive: {e}")))?;
    let mut members: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut tar = tar::Archive::new(plain.as_slice());
    for entry in tar.entries().map_err(|e| bad(e.to_string()))? {
        let mut entry = entry.map_err(|e| bad(e.to_string()))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| bad(e.to_string()))?
            .to_string_lossy()
            .into_owned();
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| bad(e.to_string()))?;
        members.insert(path.trim_start_matches("./").to_string(), bytes);
    }
    let blob = |digest: &str| -> Result<&Vec<u8>, Diagnostic> {
        let bytes = members
            .get(&format!("blobs/sha256/{digest}"))
            .ok_or_else(|| bad(format!("the archive holds no blob sha256:{digest}")))?;
        let actual = crate::util::sha256_hex(bytes);
        if actual != digest {
            return Err(Diagnostic::new(
                "E_HASH_MISMATCH",
                format!(
                    "{}: blob sha256:{digest} holds sha256:{actual}",
                    base.rootfs.url
                ),
            ));
        }
        Ok(bytes)
    };
    let index: serde_json::Value = serde_json::from_slice(
        members
            .get("index.json")
            .ok_or_else(|| bad("the archive has no index.json".into()))?,
    )
    .map_err(|e| bad(format!("index.json: {e}")))?;
    let named: Vec<&str> = index["manifests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["digest"].as_str().and_then(untag_sha256))
        .collect();
    if named != [manifest] {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!(
                "{}: the archive names image {named:?}, the lock pins sha256:{manifest}",
                base.rootfs.url
            ),
        ));
    }
    let manifest: serde_json::Value = serde_json::from_slice(blob(manifest)?)
        .map_err(|e| bad(format!("the image manifest: {e}")))?;
    let layers: Vec<&str> = manifest["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["digest"].as_str().and_then(untag_sha256))
        .collect();
    let [layer] = layers[..] else {
        return Err(bad(format!(
            "the image has {} layers; a base image has exactly one",
            layers.len()
        )));
    };
    let bytes = blob(layer)?;
    fs::create_dir_all(work)
        .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", work.display())))?;
    let path = work.join("rootfs-layer.tar");
    fs::write(&path, bytes)
        .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", path.display())))?;
    Ok(Some(path))
}

/// A writer that refuses to grow past `limit` bytes.
struct Capped<'a> {
    out: &'a mut Vec<u8>,
    limit: usize,
}

impl io::Write for Capped<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.out.len() + buf.len() > self.limit {
            return Err(io::Error::other(
                "the archive decompresses past its size limit",
            ));
        }
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A locked rpm build that is no longer served where the lock says (`E_BUILD_UNAVAILABLE`,
/// LD-435): nothing else is substituted for it. Any other failure, and every other family's,
/// is returned as it was.
fn unavailable_build(p: &ClosureEntry, url: &str, d: Diagnostic) -> Diagnostic {
    let gone = d.code == "E_FETCH"
        && ["HTTP status 404", "HTTP status 410"]
            .iter()
            .any(|status| d.message.contains(status));
    if !gone || p.release.is_none() {
        return d;
    }
    let mut d = Diagnostic::new(
        "E_BUILD_UNAVAILABLE",
        format!(
            "{}-{}:{}-{}.{} is not served any more",
            p.name,
            p.epoch.unwrap_or(0),
            p.version,
            p.release.as_deref().unwrap_or(""),
            p.arch.as_deref().unwrap_or("")
        ),
    );
    d.notes.push(format!("locked at {url}"));
    d.hint("no other build is used: run `lodi update` to pin one it serves")
}

/// Remove temporary base images left by builds whose process no longer exists.
fn prune_temporary_bases(podman: &Podman) {
    let Ok(out) = podman.output([
        "images",
        "--noheading",
        "--format",
        "{{.Repository}}:{{.Tag}}",
        "--filter",
        &format!("reference={BASE_REPO}"),
    ]) else {
        return;
    };
    for tag in String::from_utf8_lossy(&out.stdout).lines() {
        let pid = tag
            .rsplit(':')
            .next()
            .and_then(|t| t.split('-').next())
            .and_then(|p| p.parse::<i32>().ok());
        if pid.is_some_and(|p| !crate::store::pid_alive(p)) {
            let _ = podman.output(["untag", tag]);
        }
    }
}

/// Ensure the verified environment image of `base` exists and return its record.
pub fn realize_image(
    store: &Store,
    podman: &Podman,
    fetcher: &dyn Fetcher,
    base: &BaseEntry,
    report: &mut Report,
) -> Result<ImageOutcome, Diagnostic> {
    let identity = image_identity(base);
    let (tag, name) = image_names(base);
    let _key = store.key_lock(&name)?;
    let current = podman.image_id(&tag)?;
    if let (Some(meta), Some(id)) = (image_record(store, &name), &current)
        && meta.image_id == *id
        && meta.identity == identity
    {
        return Ok(ImageOutcome { meta, built: false });
    }
    // Not warm: whatever is under the tag was not verified by this store, or differs from the
    // image that was. It is never used; the image is rebuilt from verified inputs.
    let _ = fs::remove_file(store.meta_path(&name));
    if current.is_some() {
        report.incomplete_replaced.push(name.clone());
    }
    prune_temporary_bases(podman);

    // Verified inputs: the rootfs layer and every package to install.
    let rootfs = store.fetch_verified(
        fetcher,
        &base.rootfs.url,
        &base.rootfs.sha256,
        base.rootfs.size,
        report,
    )?;
    let install: Vec<&ClosureEntry> = base
        .closure
        .packages
        .iter()
        .filter(|p| p.origin == "install")
        .collect();
    let mut staged = Vec::new();
    for p in &install {
        let url = package_url(base, p)?;
        let sha = p.sha256.as_deref().unwrap_or_default();
        let file = store
            .fetch_verified(fetcher, &url, sha, p.size, report)
            .map_err(|d| unavailable_build(p, &url, d))?;
        staged.push((p, file));
    }

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let work = store
        .home()
        .join("store/tmp")
        .join(format!("{}-{n}-{name}", std::process::id()));
    let base_tag = format!("{BASE_REPO}:{}-{n}", std::process::id());
    let log_path = store.home().join("logs").join(format!("build-{name}.log"));
    let result = oci_archive_layer(base, &rootfs, &work).and_then(|layer| {
        build_image(
            podman,
            base,
            layer.as_deref().unwrap_or(&rootfs),
            &staged,
            &work,
            &base_tag,
            &tag,
            &identity,
            &log_path,
        )
    });
    let _ = podman.output(["untag", &base_tag]);
    let _ = crate::store::remove_tree(&work);
    let image_id = match result {
        Ok(()) => podman
            .image_id(&tag)?
            .ok_or_else(|| Diagnostic::new("E_LAYER_BUILD", "the built image has no tag"))?,
        Err(d) => return Err(d),
    };
    if let Err(d) = verify_closure(podman, &tag, base) {
        let _ = podman.output(["rmi", "--force", &tag]);
        return Err(d);
    }
    let meta = ImageMeta {
        version: image_record_version(),
        name: name.clone(),
        kind: "img".into(),
        identity,
        tag,
        image_id,
        closure_hash: base.closure.hash.clone(),
        packages: base.closure.packages.len(),
        created: format_utc(now_utc()),
        complete: true,
        lodi_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    crate::lock::write_atomic(&store.meta_path(&name), canonical_json(&meta).as_bytes()).map_err(
        |e| {
            Diagnostic::new(
                "E_STORE_IO",
                format!("{}: {e}", store.meta_path(&name).display()),
            )
        },
    )?;
    report.created.push(name);
    Ok(ImageOutcome { meta, built: true })
}

#[allow(clippy::too_many_arguments)]
fn build_image(
    podman: &Podman,
    base: &BaseEntry,
    rootfs: &Path,
    packages: &[(&&ClosureEntry, PathBuf)],
    work: &Path,
    base_tag: &str,
    tag: &str,
    identity: &str,
    log_path: &Path,
) -> Result<(), Diagnostic> {
    let io =
        |p: &Path, e: io::Error| Diagnostic::new("E_STORE_IO", format!("{}: {e}", p.display()));
    let ops = family_or_refuse(base)?;
    let context = work.join("context");
    let package_dir = work.join("packages");
    for dir in [&context, &package_dir] {
        fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    for (p, file) in packages {
        // A hard link where possible (same file system), a copy otherwise; the family names
        // the file, because only the family reads it back.
        let target = package_dir.join(ops.staged_package_name(&p.name, &p.version));
        if fs::hard_link(file, &target).is_err() {
            fs::copy(file, &target).map_err(|e| io(&target, e))?;
        }
    }
    let files = build_files(base, !packages.is_empty());
    for (name, text) in &files {
        let text = text.replace("{BASE}", base_tag);
        let path = context.join(name);
        fs::write(&path, text).map_err(|e| io(&path, e))?;
    }
    for p in [&context, &package_dir, &context.join("Containerfile")] {
        refuse_separators(p)?;
    }

    eprintln!(
        "lodi: building the container image ({} locked packages, {} to install; no network)",
        base.closure.packages.len(),
        packages.len()
    );
    podman.checked(
        "importing the locked rootfs",
        [
            OsStr::new("import"),
            OsStr::new("--quiet"),
            rootfs.as_os_str(),
            OsStr::new(base_tag),
        ],
    )?;
    let log = fs::File::create(log_path).map_err(|e| io(log_path, e))?;
    let log_err = log.try_clone().map_err(|e| io(log_path, e))?;
    let mut volume = OsString::from(package_dir.as_os_str());
    volume.push(format!(":{}:ro", ops.package_mount()));
    let status = podman
        .command()
        .args([
            "build",
            "--network=none",
            "--pull=never",
            "--layers=false",
            "--security-opt",
            "label=disable",
        ])
        .arg("--volume")
        .arg(&volume)
        .args(["--label", &format!("lodi.identity={identity}")])
        .args(["--label", &format!("lodi.closure={}", base.closure.hash)])
        .args(["--label", &format!("lodi.format={IMAGE_FORMAT}")])
        .args(["--tag", tag])
        .arg("--file")
        .arg(context.join("Containerfile"))
        .arg(&context)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .status()
        .map_err(|e| Diagnostic::new("E_ENTER", format!("cannot run podman: {e}")))?;
    if status.success() {
        return Ok(());
    }
    let mut d = Diagnostic::new(
        "E_LAYER_BUILD",
        format!(
            "the container image build failed ({}); nothing was recorded",
            describe_status(status)
        ),
    );
    for line in tail(log_path, LOG_TAIL_LINES) {
        d.notes.push(format!("  | {line}"));
    }
    Err(d.hint(format!("the full log is {}", log_path.display())))
}

/// The last `n` lines of a file (reading at most its last 64 KiB).
fn tail(path: &Path, n: usize) -> Vec<String> {
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)));
    let mut text = String::new();
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    text.push_str(&String::from_utf8_lossy(&bytes));
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Paths given to Podman's `--volume`/`--mount` syntax cannot contain `:` or `,`.
fn refuse_separators(path: &Path) -> Result<(), Diagnostic> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&b':') || bytes.contains(&b',') {
        return Err(Diagnostic::new(
            "E_ENTER",
            format!(
                "{} contains `:` or `,`, which Podman's bind syntax cannot express",
                path.display()
            ),
        )
        .hint("move the project, or set LODI_HOME, to a path without them"));
    }
    Ok(())
}

/// The container's environment (`spec/07` §3 with `LODI_MODE=container`): `HOME`, `USER`,
/// `LOGNAME` and `TERM` from the caller, `LANG` from the locked locale, `PATH` =
/// `<env>/tree/bin` before Debian's default, the `LODI_*` values, the tools' `spec.env` and the
/// manifest's `[env]`. Nothing else of the caller's environment enters the container.
#[allow(clippy::too_many_arguments)]
pub fn container_environment(
    parent: &BTreeMap<OsString, OsString>,
    store: &Store,
    plan: &host::Plan,
    lock: &LockFile,
    manifest: &ProjectManifest,
    activation: &Activation,
    name: &str,
    locale: &str,
) -> Result<BTreeMap<OsString, OsString>, Diagnostic> {
    let mut seed: BTreeMap<OsString, OsString> = BTreeMap::new();
    for key in ["HOME", "USER", "LOGNAME", "TERM"] {
        if let Some(v) = parent.get(OsStr::new(key)) {
            seed.insert(key.into(), v.clone());
        }
    }
    seed.insert("LANG".into(), locale.into());
    seed.insert("PATH".into(), CONTAINER_PATH.into());
    host::child_environment(&seed, store, plan, lock, manifest, activation, name)
}

/// The settings of one `podman run`.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub name: String,
    pub image: String,
    pub hostname: String,
    pub project_root: PathBuf,
    /// `home = "shared"`: the caller's `$HOME`, bound read-write at the same path.
    pub home: Option<PathBuf>,
    /// The store, bound read-only at the same path (tools and the `env-` entry).
    pub store: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub tty: bool,
}

/// The `podman run` argv for `argv` (`spec/08` §8 with this build's runtime settings):
/// `--rm` (writable = ephemeral: the container's writes outside the binds are discarded),
/// `--log-driver=none` (the command's output reaches the caller and is stored nowhere; Podman's
/// default driver copies it into the system journal, outside the ADR-018 boundary, LD-24),
/// `--userns=keep-id` (user = same: the caller's uid and gid, name in `/etc/passwd`),
/// `--network=host`, `--pid=private`, `--hostname` = the environment name (the `spec/01`
/// defaults of the runtime keys the manifest cannot set), `--init` (signals reach the command
/// and orphans are reaped), the project root and a shared home bound read-write at their own
/// paths, the store read-only, the working directory the project root, and the argv run as
/// `/bin/sh -c 'exec "$@"' sh <argv…>`. `gui` and `devices` are not provided in this build
/// (LD-21).
pub fn run_arguments(spec: &RunSpec, argv: &[OsString]) -> Result<Vec<OsString>, Diagnostic> {
    let mut args: Vec<OsString> = [
        "run",
        "--rm",
        "--log-driver=none",
        "--init",
        "--interactive",
        "--pull=never",
        "--userns=keep-id",
        "--network=host",
        "--pid=private",
        "--security-opt",
        "label=disable",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    if spec.tty {
        args.push("--tty".into());
    }
    args.push("--name".into());
    args.push(spec.name.clone().into());
    args.push("--hostname".into());
    args.push(spec.hostname.clone().into());
    let mut bind = |path: &Path, mode: &str| -> Result<(), Diagnostic> {
        refuse_separators(path)?;
        let mut v = OsString::from(path.as_os_str());
        v.push(":");
        v.push(path.as_os_str());
        v.push(format!(":{mode}"));
        args.push("--volume".into());
        args.push(v);
        Ok(())
    };
    bind(&spec.project_root, "rw")?;
    if let Some(home) = &spec.home {
        bind(home, "rw")?;
    }
    bind(&spec.store, "ro")?;
    args.push("--workdir".into());
    args.push(spec.project_root.clone().into_os_string());
    for (k, v) in &spec.env {
        let mut pair = k.clone();
        pair.push("=");
        pair.push(v);
        args.push("--env".into());
        args.push(pair);
    }
    args.push(spec.image.clone().into());
    // `spec/08` §7 step 4, without an activate script (the environment is passed above):
    // `exec "$@"` hands the argv over unchanged and gives a missing program 127 and one that
    // cannot run 126, as in host mode (the init process would report 1).
    for a in ["/bin/sh", "-c", "exec \"$@\"", "sh"] {
        args.push(a.into());
    }
    args.extend(argv.iter().cloned());
    Ok(args)
}

/// `lodi develop` / `lodi run` for a `[container]` manifest (after the frozen lock and trust).
pub fn enter(
    root: &Path,
    manifest: &ProjectManifest,
    lock: &LockFile,
    argv: &[OsString],
    _options: &Options,
    fetcher: &dyn Fetcher,
) -> Result<u8, Failure> {
    let parent: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    let outer = match (
        parent.get(OsStr::new("LODI_MODE")),
        parent.get(OsStr::new("LODI_ACTIVATION")),
    ) {
        (Some(mode), _) => Some(mode.to_string_lossy().into_owned()),
        (None, Some(_)) => Some("unknown".to_string()),
        (None, None) => None,
    };
    if let Some(mode) = outer {
        return Err(fail(
            Diagnostic::new(
                "E_NESTED",
                format!(
                    "cannot enter a container environment from inside a {mode} environment; \
                     this build does not nest container environments"
                ),
            )
            .hint("leave the outer environment first"),
        ));
    }
    let base = lock.base.as_ref().ok_or_else(|| {
        fail(Diagnostic::new(
            "E_LOCK_STALE",
            "the lock has no base for this container manifest",
        ))
    })?;
    let podman =
        Podman::find(parent.get(OsStr::new("PATH")).map(OsString::as_os_str)).map_err(fail)?;

    let store = Store::open_for_write(&Store::home_from_env().map_err(fail)?).map_err(fail)?;
    store.prune_sessions();
    store.prune_staging();
    let mut podman = podman;
    let tmpdir = podman.keep_temporary_files_in(&store).map_err(fail)?;
    let result = enter_with(
        root, manifest, lock, argv, fetcher, &parent, base, &podman, &store,
    );
    let _ = crate::store::remove_tree(&tmpdir);
    result
}

/// [`enter`] once the runtime and the store are known.
#[allow(clippy::too_many_arguments)]
fn enter_with(
    root: &Path,
    manifest: &ProjectManifest,
    lock: &LockFile,
    argv: &[OsString],
    fetcher: &dyn Fetcher,
    parent: &BTreeMap<OsString, OsString>,
    base: &BaseEntry,
    podman: &Podman,
    store: &Store,
) -> Result<u8, Failure> {
    let identity = image_identity(base);
    let (tag, _) = image_names(base);
    let plan = host::plan_for(
        manifest,
        lock,
        RUNTIME,
        Some(json!({
            "image": identity,
            "closure": base.closure.hash,
            "rootfs": base.rootfs.sha256,
        })),
    )
    .map_err(fail)?;
    let activation = Activation {
        envhash: plan.envhash.clone(),
        project_root: root.to_path_buf(),
        profile: PROFILE.into(),
        runtime: RUNTIME.into(),
    };
    let shared = store.shared_lock().map_err(fail)?;
    let mut report = Report::default();
    let warnings = host::realize(store, fetcher, lock, &plan, &mut report).map_err(fail)?;
    for w in warnings {
        eprintln!("{w}");
    }
    let image = realize_image(store, podman, fetcher, base, &mut report).map_err(fail)?;
    if !report.created.is_empty() {
        eprintln!(
            "lodi: realized {} ({} bytes downloaded)",
            report.created.join(", "),
            report.downloaded
        );
    }
    let name = manifest
        .environment_name(root)
        .unwrap_or_else(|| "project".into());
    let env = container_environment(
        parent,
        store,
        &plan,
        lock,
        manifest,
        &activation,
        &name,
        &base.options.locale,
    )
    .map_err(fail)?;
    let container = format!(
        "lodi-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let home = parent
        .get(OsStr::new("HOME"))
        .map(PathBuf::from)
        .filter(|h| h.is_absolute() && h.is_dir());
    let spec = RunSpec {
        name: container.clone(),
        image: tag,
        hostname: name.chars().take(63).collect(),
        project_root: root.to_path_buf(),
        home,
        store: store.home().join("store"),
        env,
        tty: io::stdin().is_terminal() && io::stdout().is_terminal(),
    };
    let args = run_arguments(&spec, argv).map_err(fail)?;
    let mut entries = vec![plan.env_name()];
    entries.extend(plan.arts.iter().map(|(_, a)| a.clone()));
    let pid = std::process::id();
    let root_file = store
        .add_session_root(
            pid,
            &json!({
                "pid": pid,
                "activation": activation.record(),
                "entries": entries,
                "image": { "name": image.meta.name, "tag": image.meta.tag, "id": image.meta.image_id },
                "container": container,
            }),
        )
        .map_err(fail)?;
    drop(shared);
    let status = host::spawn_and_wait(&podman.argv(args), &podman.environment(parent), root);
    // `--rm` removes the container when Podman exits normally; if Podman itself was killed the
    // container may still exist (conmon keeps it), so it is stopped and removed here.
    podman.remove_container(&container);
    let _ = fs::remove_file(&root_file);
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_arguments_bind_same_paths_and_refuse_separators() {
        let spec = RunSpec {
            name: "lodi-1-0-abc".into(),
            image: "localhost/lodi-env:0".into(),
            hostname: "demo".into(),
            project_root: "/p/demo".into(),
            home: Some("/h/me".into()),
            store: "/h/me/lodi/store".into(),
            env: [("PATH".into(), "/x:/bin".into())].into_iter().collect(),
            tty: false,
        };
        let args = run_arguments(&spec, &["make".into(), "-j 2".into()]).unwrap();
        let text: Vec<String> = args.iter().map(|a| a.to_string_lossy().into()).collect();
        for want in [
            "--rm",
            "--log-driver=none",
            "--init",
            "--userns=keep-id",
            "--network=host",
            "--pid=private",
            "/p/demo:/p/demo:rw",
            "/h/me:/h/me:rw",
            "/h/me/lodi/store:/h/me/lodi/store:ro",
            "PATH=/x:/bin",
        ] {
            assert!(
                text.iter().any(|a| a == want),
                "{want} missing from {text:?}"
            );
        }
        assert!(!text.iter().any(|a| a == "--tty"));
        assert_eq!(
            &text[text.len() - 7..],
            [
                "localhost/lodi-env:0",
                "/bin/sh",
                "-c",
                "exec \"$@\"",
                "sh",
                "make",
                "-j 2"
            ]
        );
        let bad = RunSpec {
            project_root: "/p/a:b".into(),
            ..spec
        };
        assert_eq!(run_arguments(&bad, &[]).unwrap_err().code, "E_ENTER");
    }

    #[test]
    fn the_project_scope_document_names_what_an_entry_shares_with_the_machine() {
        // Read from the arguments an entry is started with, not listed here, so that a changed
        // or added setting fails until `docs/scopes/project.md` discloses it (M-1.0 u-4,
        // LD-363): every `podman run` option below that decides what the container shares with
        // the machine, with its value, and the mode the caller's home is bound with.
        const SHARING: [&str; 10] = [
            "--security-opt",
            "--network",
            "--net",
            "--userns",
            "--pid",
            "--ipc",
            "--uts",
            "--cgroupns",
            "--cap-add",
            "--device",
        ];
        let heading = "## What a container entry shares with your machine\n";
        let section = include_str!("../docs/scopes/project.md")
            .split_once(heading)
            .map(|(_, rest)| rest.split("\n## ").next().unwrap_or(rest))
            .expect("docs/scopes/project.md has the section this test reads");
        let spec = RunSpec {
            name: "lodi-1-0-abc".into(),
            image: "localhost/lodi-env:0".into(),
            hostname: "demo".into(),
            project_root: "/p/demo".into(),
            home: Some("/h/me".into()),
            store: "/s/store".into(),
            env: BTreeMap::new(),
            tty: false,
        };
        let args: Vec<String> = run_arguments(&spec, &[])
            .unwrap()
            .iter()
            .map(|a| a.to_string_lossy().into())
            .collect();
        let mut settings = Vec::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            if arg == "--privileged" {
                settings.push(arg.clone());
            } else if SHARING.contains(&arg.split('=').next().unwrap_or(arg)) {
                let value = if arg.contains('=') {
                    String::new()
                } else {
                    format!(" {}", rest.next().unwrap())
                };
                settings.push(format!("{arg}{value}"));
            }
        }
        assert!(!settings.is_empty(), "{args:?}");
        for setting in &settings {
            assert!(
                section.contains(&format!("`{setting}`")),
                "docs/scopes/project.md does not disclose `{setting}` under {:?}",
                heading.trim_end()
            );
        }
        let home = args
            .iter()
            .find_map(|a| a.strip_prefix("/h/me:/h/me:"))
            .expect("the home is bound");
        let mode = match home {
            "rw" => "read-write",
            "ro" => "read-only",
            other => panic!("the home is bound with the mode {other}"),
        };
        assert!(
            section
                .lines()
                .any(|line| line.contains("`$HOME`") && line.contains(mode)),
            "docs/scopes/project.md does not say the home is bound {mode}"
        );
    }

    /// A stand-in `podman` that logs its `TMPDIR` and argv, one line per call, and answers
    /// like a Podman with no image and no container.
    fn recording_podman(dir: &Path) -> (Podman, PathBuf) {
        let log = dir.join("calls");
        let script = dir.join("podman");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s' \"${{TMPDIR-unset}}\" >> '{log}'\nfor a in \"$@\"; do \
                 printf ' %s' \"$a\" >> '{log}'; done\necho >> '{log}'\ncase \"$*\" in \
                 *' exists '*) exit 1;; esac\nexit 0\n",
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (
            Podman {
                program: script,
                tmpdir: None,
            },
            log,
        )
    }

    #[test]
    fn every_podman_command_records_no_events_and_keeps_temporary_files_in_lodi_home() {
        let dir = std::env::temp_dir().join(format!("lodi-unit-podman-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("lodi-home")).unwrap();
        let (mut podman, log) = recording_podman(&dir);
        let tmpdir = podman.keep_temporary_files_in(&store).unwrap();
        assert!(tmpdir.starts_with(store.home()), "{}", tmpdir.display());
        assert_eq!(
            fs::metadata(&tmpdir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        // Every method that runs Podman, and the argv and environment of the entry's own run.
        assert_eq!(podman.image_id("localhost/lodi-env:x").unwrap(), None);
        assert!(!podman.container_exists("c"));
        podman.remove_container("c");
        prune_temporary_bases(&podman);
        podman.checked("import", ["import", "r.tar", "t"]).unwrap();
        podman
            .checked(
                "reading",
                crate::distro::apt::Apt.closure_query_arguments("localhost/lodi-env:x"),
            )
            .unwrap();
        let calls = fs::read_to_string(&log).unwrap();
        // (The query's format ends in a newline: its last argument ends a line of its own.)
        let calls: Vec<&str> = calls.lines().filter(|l| !l.is_empty()).collect();
        // image exists; container exists (twice: remove_container asks first); images; import;
        // the closure query.
        assert_eq!(calls.len(), 6, "{calls:?}");
        for call in &calls {
            let want = format!("{} {EVENTS} ", tmpdir.display());
            assert!(
                call.starts_with(&want),
                "{call:?} does not start with {want:?}"
            );
        }
        let query: Vec<&str> = calls[5].split(' ').skip(2).collect();
        assert_eq!(query[0], "run");
        assert!(query.contains(&"--log-driver=none"), "{query:?}");

        let spec = RunSpec {
            name: "lodi-1-0".into(),
            image: "localhost/lodi-env:0".into(),
            hostname: "demo".into(),
            project_root: "/p/demo".into(),
            home: None,
            store: "/h/store".into(),
            env: BTreeMap::new(),
            tty: false,
        };
        let full = podman.argv(run_arguments(&spec, &["true".into()]).unwrap());
        assert_eq!(full[0], podman.program.as_os_str());
        assert_eq!(full[1], EVENTS, "{full:?}");
        assert_eq!(full[2], "run", "{full:?}");
        assert!(full.iter().any(|a| a == "--log-driver=none"), "{full:?}");
        let env = podman.environment(&[("PATH".into(), "/bin".into())].into_iter().collect());
        assert_eq!(
            env.get(OsStr::new("TMPDIR")),
            Some(&tmpdir.clone().into_os_string())
        );
        assert_eq!(env.get(OsStr::new("PATH")), Some(&OsString::from("/bin")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn podman_is_started_only_through_the_checked_paths() {
        // Every Podman process of this module comes from `Podman::command` (the flag before the
        // subcommand, TMPDIR) or from `Podman::argv` + `Podman::environment` (the entry's
        // `podman run`); nothing else here builds a command line or spawns a process.
        let before_tests = |source: &'static str| &source[..source.find("#[cfg(test)]").unwrap()];
        let code = before_tests(include_str!("container.rs"));
        // The family renders the read-back command (M-Arch-Base T-1), so it is held to the same
        // rule: it builds argument lists and never starts a process of its own.
        let family = before_tests(include_str!("distro/apt.rs"));
        assert_eq!(code.matches("Command::new(").count(), 1);
        assert_eq!(code.matches("spawn_and_wait(").count(), 1);
        assert_eq!(family.matches("Command::new(").count(), 0);
        assert_eq!(family.matches("spawn_and_wait(").count(), 0);
        assert!(code.contains(
            "host::spawn_and_wait(&podman.argv(args), &podman.environment(parent), root)"
        ));
        // Every `podman run` of this module and of the family stores no output.
        let runs = |t: &str| code.matches(t).count() + family.matches(t).count();
        assert_eq!(runs("\"run\","), 2);
        assert_eq!(runs("\"--log-driver=none\","), 2);
    }

    #[test]
    fn a_missing_podman_names_the_install_command() {
        let d = Podman::find(Some(OsStr::new("/nonexistent-dir"))).unwrap_err();
        assert_eq!(d.code, "E_NO_RUNTIME");
        assert!(d.to_string().contains("podman"), "{d}");
        assert_eq!(Podman::find(None).unwrap_err().code, "E_NO_RUNTIME");
    }
}
