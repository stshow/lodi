//! The home tool set's shared schema parsing, one pinned `home.lock`, project realization
//! pipeline, and persistent GC root. T-4 built these mechanics behind the command surface;
//! T-5 connects them to the generated home profile (LD-106, LD-107).

use std::collections::BTreeMap;

use serde_json::json;

use crate::fetch::Fetcher;
use crate::home::fsops::{self, RelPath, Root};
use crate::home::{lock as home_lock, manifest};
use crate::host;
use crate::lock::{self, Failure, LockFile, LockProblem};
use crate::manifest::{ManifestErrors, Project, ProjectManifest};
use crate::roots::Roots;
use crate::store::{Report, Store};
use crate::tools::ToolSet;

/// The persistent root of the realized home environment, relative to the data/store root.
pub const HOME_GC_ROOT: &str = "gcroots/home";

/// The registry row that root is registered as (`src/schema.rs`, M-1.0 T-1).
pub const HOME_GC_ROOT_ARTIFACT: &str = "home-gc-root";

/// A resolved home tool set and the exact host-mode realization plan it uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub lock: LockFile,
    pub plan: host::Plan,
    /// Labels whose pins were freshly resolved. Empty means the existing lock was reused.
    pub resolved: Vec<String>,
    /// Whether this call changed `home.lock`.
    pub wrote_lock: bool,
}

/// What realizing a home tool set produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Realization {
    /// The environment entry first, then its artifact entries in label order.
    pub entries: Vec<String>,
    pub warnings: Vec<String>,
    pub report: Report,
}

/// Parse a home manifest's tools through the shared `src/tools.rs` schema. This remains public
/// for the cross-scope diagnostic regression.
pub fn parse_manifest_bytes(
    bytes: &[u8],
    file: &str,
    base: &Root,
) -> Result<ToolSet, ManifestErrors> {
    manifest::parse_home_manifest_bytes_with_tools(bytes, file, base).map(|m| m.tools)
}

fn failure(errors: ManifestErrors) -> Failure {
    Failure {
        diagnostics: errors.diagnostics,
        exit_status: ManifestErrors::EXIT_STATUS,
    }
}

fn project(tools: ToolSet) -> ProjectManifest {
    ProjectManifest {
        project: Project {
            schema_version: "1".into(),
            name: None,
        },
        container: None,
        packages: Vec::new(),
        tools,
        env: Vec::new(),
        tasks: BTreeMap::new(),
    }
}

fn stale(tools: &ToolSet, lock: &LockFile) -> Vec<String> {
    let mut stale = home_lock::staleness(tools, lock);
    if lock.manifest_hash != lock::tool_set_hash(tools) {
        stale.push("tools: declaration hash changed".into());
    }
    stale
}

/// Resolve the tool section once and pin it beside `home.toml`.
///
/// With `locked`, a missing or changed lock is
/// `E_LOCK_STALE` at exit 10 and nothing is written. A manifest without `[tools]` needs no lock;
/// an unlocked call removes a valid obsolete one, while a newer or invalid file is preserved and
/// refused rather than overwritten.
pub fn resolve(
    roots: &Roots,
    fetcher: &dyn Fetcher,
    now: i64,
    locked: bool,
) -> Result<Option<Resolution>, Failure> {
    let manifest = manifest::load_home_manifest_with_tools(roots).map_err(failure)?;
    resolve_set(roots, manifest.tools, fetcher, now, locked)
}

/// [`resolve`] for a tool set already loaded: what `lodi home apply` calls with the `[tools]` of
/// the manifest its pre-flight read, so the manifest is read once per plan.
pub fn resolve_set(
    roots: &Roots,
    tools: ToolSet,
    fetcher: &dyn Fetcher,
    now: i64,
    locked: bool,
) -> Result<Option<Resolution>, Failure> {
    let previous = match home_lock::read(roots) {
        Ok(lock) => Some(lock),
        Err(LockProblem::Missing) => None,
        Err(problem) => return Err(Failure::one(home_lock::diagnostic(&problem))),
    };
    resolve_set_with_lock(roots, tools, fetcher, now, locked, previous)
}

/// What `--locked` refuses of `previous` for `tools`: a lock where no tool is declared, a lock the
/// declarations no longer match, or no lock where a tool is declared. `None` uses it as it is.
pub fn locked_refusal(tools: &ToolSet, previous: Option<&LockFile>) -> Option<LockProblem> {
    match previous {
        Some(lock) if tools.is_empty() => {
            let mut entries = home_lock::staleness(tools, lock);
            if entries.is_empty() {
                entries.push("tools: the lock exists, but the manifest has no [tools]".into());
            }
            Some(LockProblem::Stale(entries))
        }
        Some(lock) => {
            let stale = stale(tools, lock);
            (!stale.is_empty()).then_some(LockProblem::Stale(stale))
        }
        None if tools.is_empty() => None,
        None => Some(LockProblem::Missing),
    }
}

pub fn resolve_set_with_lock(
    roots: &Roots,
    tools: ToolSet,
    fetcher: &dyn Fetcher,
    now: i64,
    locked: bool,
    previous: Option<LockFile>,
) -> Result<Option<Resolution>, Failure> {
    resolve_set_with_lock_to(
        roots,
        tools,
        fetcher,
        now,
        locked,
        previous,
        None,
        &mut Vec::new(),
    )
}

/// [`resolve_set_with_lock`] writing the lock to `target`, the home's section of its
/// repository's root lock, when there is one (LD-416), instead of `home.lock` beside the
/// manifest. The root lock's lines — each 1.4 lock it moved in, and the lock itself — are added
/// to `notes`.
#[allow(clippy::too_many_arguments)]
pub fn resolve_set_with_lock_to(
    roots: &Roots,
    tools: ToolSet,
    fetcher: &dyn Fetcher,
    now: i64,
    locked: bool,
    previous: Option<LockFile>,
    target: Option<&crate::flakelock::HomeTarget>,
    notes: &mut Vec<String>,
) -> Result<Option<Resolution>, Failure> {
    let write = |lock: Option<&LockFile>, notes: &mut Vec<String>| -> Result<(), Failure> {
        match (target, lock) {
            (Some(target), lock) => {
                notes.extend(target.write(lock).map_err(Failure::one)?);
                Ok(())
            }
            (None, Some(lock)) => home_lock::write(roots, lock).map_err(Failure::one),
            (None, None) => home_lock::remove(roots).map_err(Failure::one),
        }
    };
    if locked && let Some(problem) = locked_refusal(&tools, previous.as_ref()) {
        return Err(Failure::one(home_lock::diagnostic(&problem)));
    }
    if tools.is_empty() {
        if previous.is_some() {
            write(None, notes)?;
        }
        return Ok(None);
    }

    if let Some(lock) = &previous
        && stale(&tools, lock).is_empty()
    {
        let plan = host::plan_for(&project(tools), lock, "host", None).map_err(Failure::one)?;
        return Ok(Some(Resolution {
            lock: lock.clone(),
            plan,
            resolved: Vec::new(),
            wrote_lock: false,
        }));
    }

    let input = project(tools.clone());
    let (lock, resolved) = lock::resolve_manifest(
        &input,
        lock::tool_set_hash(&tools),
        previous.as_ref(),
        fetcher,
        now,
    )?;
    let plan = host::plan_for(&input, &lock, "host", None).map_err(Failure::one)?;
    write(Some(&lock), notes)?;
    Ok(Some(Resolution {
        lock,
        plan,
        resolved,
        wrote_lock: true,
    }))
}

/// The tool lock `tools` needs, written nowhere: `previous` when it is fresh for them, else a
/// fresh resolution that starts from it; `None` when there are no tools. `lodi update` commits
/// every home's lock with the host's in one write (LD-416).
pub fn locked_set(
    tools: ToolSet,
    previous: Option<&LockFile>,
    fetcher: &dyn Fetcher,
    now: i64,
) -> Result<Option<LockFile>, Failure> {
    if tools.is_empty() {
        return Ok(None);
    }
    if let Some(lock) = previous
        && stale(&tools, lock).is_empty()
    {
        return Ok(Some(lock.clone()));
    }
    let input = project(tools.clone());
    let (lock, _) =
        lock::resolve_manifest(&input, lock::tool_set_hash(&tools), previous, fetcher, now)?;
    Ok(Some(lock))
}

/// Realize the pinned set through the project host pipeline, then replace the persistent root
/// while the shared store lock is still held. A collector therefore sees either the prior whole
/// set or the new whole set, never a half-realized one.
pub fn realize(
    roots: &Roots,
    fetcher: &dyn Fetcher,
    resolution: &Resolution,
) -> Result<Realization, Failure> {
    let store = Store::open_for_write(roots.data()).map_err(Failure::one)?;
    store.prune_sessions();
    store.prune_staging();
    let shared = store.shared_lock().map_err(Failure::one)?;
    let mut report = Report::default();
    let warnings = host::realize(
        &store,
        fetcher,
        &resolution.lock,
        &resolution.plan,
        &mut report,
    )
    .map_err(Failure::one)?;
    let mut entries = vec![resolution.plan.env_name()];
    entries.extend(resolution.plan.arts.iter().map(|(_, name)| name.clone()));
    let rel = RelPath::new(HOME_GC_ROOT).expect("the home GC root is inside the data root");
    fsops::write(
        &roots.data_root(),
        &rel,
        crate::schema::stamped_json(HOME_GC_ROOT_ARTIFACT, &json!({ "entries": entries }))
            .as_bytes(),
        0o644,
    )
    .map_err(Failure::one)?;
    drop(shared);
    Ok(Realization {
        entries,
        warnings,
        report,
    })
}
