use std::path::{Component, Path, PathBuf};

use crate::diag::Diagnostic;
use crate::home::{apply::Failure, lock, manifest};
use crate::hostscope::{self, source};
use crate::roots::Roots;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub source: PathBuf,
    pub host: Option<String>,
}

pub struct Selected {
    pub roots: Roots,
    pub manifest: manifest::HomeManifest,
    pub lock: Option<crate::lock::LockFile>,
    /// The home's section of its repository's root lock, when it lives in one (LD-416).
    pub target: Option<crate::flakelock::HomeTarget>,
    pub source: String,
}

fn absolute(path: &Path) -> Result<PathBuf, Diagnostic> {
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(Diagnostic::new(
            "E_PATH_ESCAPE",
            "SOURCE contains parent traversal",
        ));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|dir| dir.join(path))
            .map_err(|e| Diagnostic::new("E_CONFIG", format!("cannot resolve SOURCE: {e}")))
    }
}

/// Whether `path` names anything, a link included: a link is refused by the walk that reads it.
fn present(path: &Path) -> bool {
    path.exists() || std::fs::read_link(path).is_ok()
}

fn choose_dir(selection: &Selection) -> Result<(PathBuf, PathBuf, u32), Diagnostic> {
    let named = absolute(&selection.source)?;
    if !present(&named) {
        return Err(Diagnostic::new(
            "E_NO_MANIFEST",
            format!("{} does not exist", named.display()),
        )
        .hint(
            "SOURCE names a home directory (it holds home.toml), a host directory or a \
               directory of hosts",
        ));
    }
    // SAFETY: geteuid cannot fail and takes no arguments.
    let euid = unsafe { libc::geteuid() };
    let direct = present(&named.join("home.toml"));
    let dir = if direct {
        if selection.host.is_some() {
            return Err(Diagnostic::new(
                "E_UNSUPPORTED",
                "--host cannot select from a home directory",
            ));
        }
        named.clone()
    } else {
        let host = if present(&named.join("host.toml")) {
            if selection.host.is_some() {
                return Err(Diagnostic::new(
                    "E_UNSUPPORTED",
                    "--host cannot select from one host directory",
                ));
            }
            named.clone()
        } else {
            let name = match &selection.host {
                Some(name) => name.clone(),
                None => source::hostname(Path::new("/"), true)?,
            };
            if !source::safe_component(&name) {
                return Err(Diagnostic::new("E_PATH_ESCAPE", "unsafe host name"));
            }
            named.join(name)
        };
        let user = crate::passwd::by_uid(Path::new("/"), euid).ok_or_else(|| {
            Diagnostic::new("E_CONFIG", format!("uid {euid} has no passwd entry"))
        })?;
        if !source::safe_component(&user.name) {
            return Err(Diagnostic::new("E_PATH_ESCAPE", "unsafe passwd login"));
        }
        host.join("home").join(user.name)
    };
    Ok((named, dir, euid))
}

pub fn target(roots: Roots, selection: &Selection) -> Result<Roots, Diagnostic> {
    let (named, dir, euid) = choose_dir(selection)?;
    let direct = dir == named;
    let host_dir = if direct {
        &dir
    } else {
        dir.parent()
            .and_then(Path::parent)
            .ok_or_else(|| Diagnostic::new("E_PATH_ESCAPE", "invalid home source"))?
    };
    let host = source::Host {
        anchor: named,
        dir: host_dir.to_path_buf(),
        source: host_dir.display().to_string(),
        in_place: false,
        owners: source::owners_for(euid, None),
        named: None,
    };
    if !direct
        && source::read_file(&host, "host.toml", crate::manifest::MAX_MANIFEST_BYTES)?.is_none()
    {
        return Err(Diagnostic::new(
            "E_NO_MANIFEST",
            "selected host has no host.toml",
        ));
    }
    let relative = dir
        .strip_prefix(host_dir)
        .expect("the home is below its selected host")
        .join("home.toml");
    source::inspect(&host, &relative.display().to_string())?;
    Ok(roots.with_config(dir))
}

pub fn load(roots: Roots, selection: &Selection) -> Result<Selected, Failure> {
    let (named, dir, euid) = choose_dir(selection)?;
    let host = source::Host {
        anchor: named.clone(),
        dir: dir.clone(),
        source: dir.display().to_string(),
        in_place: false,
        owners: source::owners_for(euid, None),
        named: Some(named),
    };
    let bytes = source::read_file(&host, "home.toml", manifest::MAX_HOME_MANIFEST_BYTES)?
        .ok_or_else(|| {
            Diagnostic::new(
                "E_NO_MANIFEST",
                format!("no home manifest at {}", dir.join("home.toml").display()),
            )
        })?;
    let roots = roots.with_config(dir.clone());
    let parsed = manifest::parse_home_manifest_host_bytes(
        &bytes,
        &dir.join("home.toml").display().to_string(),
        &roots.config_root(),
        manifest::HostSource {
            host: &host,
            prefix: "",
        },
    )?;
    let lock = source::read_file(&host, "home.lock", hostscope::MAX_SOURCE_BYTES)?
        .map(|bytes| {
            if bytes.len() > hostscope::MAX_SOURCE_BYTES {
                return Err(crate::lock::LockProblem::Invalid(format!(
                    "home.lock exceeds {} bytes",
                    hostscope::MAX_SOURCE_BYTES
                )));
            }
            crate::lock::parse_lock(&bytes)
        })
        .transpose()
        .map_err(|problem| lock::diagnostic(&problem))?;
    let lock = crate::flakelock::read_home(&host, &dir, lock)?;
    let target = crate::flakelock::HomeTarget::of(&host, &dir);
    // A home in a repository resolves through its root's `recipes/` too; a home directory
    // named on its own is no repository (LD-409).
    crate::catalogue::user::install(match &target {
        Some(_) => crate::catalogue::user::for_repository(&host),
        None => Ok(crate::catalogue::user::UserRecipes::none()),
    });
    Ok(Selected {
        roots,
        manifest: parsed,
        lock,
        target,
        source: dir.display().to_string(),
    })
}
