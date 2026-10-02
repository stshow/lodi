//! The import core: a machine's own readings turned into a short, commented, deterministic
//! `host.toml` (M-Import T-1).
//!
//! `lodi import` reaches it through the config's import. What is here is the whole of the
//! transformation, in three pieces:
//!
//! - [`read`] takes the readings — [`pm::Backend::observe`], [`pm::Backend::origins`] and the
//!   read-only [`pm::Backend::survey`] this package added to the seam — and nothing else;
//! - [`baseline::select`] subtracts what the distribution's own installer put there, leaving
//!   what a person chose;
//! - [`files::capture`] names the configuration files the package manager itself reports as
//!   changed, and reads none of them (T-2, si-1);
//! - [`emit::manifest`] writes the bytes.
//!
//! # What this module cannot do
//!
//! - **It builds no [`pm::Invocation`].** Every backend method it calls returns data; the ones
//!   that return an invocation — `transaction`, `removal`, `marks`, `holds`, `refresh` — are not
//!   called from anywhere below this line. `tests/host_import_core.rs` asserts it from the other
//!   side of the seam, out of the argv log its own shims write: only the read commands appear.
//! - **It writes nothing.** No file below a root, no lock, no journal, no cache. [`read`] opens
//!   directories to count snaps and flatpaks, reads apt's source lists and the keyrings they
//!   name ([`sources::read`], which attributes a package to a repository by the same
//!   `apt-cache policy` reading as [`pm::Backend::origins`], LD-367), and opens nothing else.
//! - **It makes no network request of any kind** (design call D3, LD-271). The baseline is the
//!   machine's own package database: no image manifest is downloaded, and no baseline table is
//!   shipped in the binary, which OD-04 rules out.
//! - **It adds no `Serialize` type.** The emitter writes text, because an emitted manifest is the
//!   user's own TOML and `docs/SCHEMAS.md` excludes those from the closed registry of
//!   `src/schema.rs` by name.

pub mod baseline;
pub mod emit;
pub mod files;
pub mod sources;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::diag::Diagnostic;

use super::pm::{self, Observed, Survey};
use super::safety::{Distro, Gate};

/// Where snapd keeps the squashfs images of the snaps a machine has, inside the root.
pub const SNAPS: &str = "var/lib/snapd/snaps";
/// Where a system-wide flatpak installation keeps its applications, inside the root.
pub const FLATPAKS: &str = "var/lib/flatpak/app";

/// The instant a machine was read, hour-floored and in UTC.
///
/// It is a **value the caller supplies**, never a clock this module reads, which is what keeps
/// the emitter a pure function of its inputs: two imports of an unchanged machine at the same
/// instant are byte identical, and a fixture can pin one. [`Snapshot::now`] is how a command
/// takes the present one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot(String);

impl Snapshot {
    /// The snapshot for a number of seconds since the Unix epoch, floored to the hour — the same
    /// resolution and the same `YYYY-MM-DDTHH:MM:SSZ` spelling the project manifest's own
    /// `[container] snapshot` uses.
    pub fn at(secs: i64) -> Snapshot {
        Snapshot(crate::util::format_utc(secs - secs.rem_euclid(3600)))
    }

    /// The snapshot for now. It is the only place in this module that reads a clock, and no
    /// function below it does.
    pub fn now() -> Snapshot {
        Snapshot::at(crate::util::now_utc())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Everything one import read off one machine, and the only thing the emitter is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub distro: Distro,
    /// The release's own codename (`bookworm`, `noble`), empty where the distribution has none.
    /// The **number** is deliberately not carried: a generated file states what it applies to,
    /// and a digit-dotted release is indistinguishable from a version of something.
    pub codename: String,
    pub observed: Observed,
    pub survey: Survey,
    /// Where the **installed** version of each name of the candidate set came from. It is what
    /// decides whether a name is declared, replacing the candidate-existence test R-2 found
    /// pointing the wrong way on apt (D4 and LD-272 stand: nothing off-base is declared).
    pub origins: BTreeMap<String, pm::Origin>,
    pub snaps: usize,
    pub flatpaks: usize,
    /// The apt repositories the machine enabled, read off `/etc/apt`, with the names from
    /// outside the distribution attributed to them by `apt-cache policy` (LD-367).
    pub repositories: sources::Repositories,
    /// The machine's human accounts (su-1, S1), read from `etc/passwd` and `etc/group` and
    /// never from a shadow file.
    pub users: Vec<super::users::Human>,
    /// The basics `[system]` declares, as the machine has them (si-1): read from files, never
    /// by running a program.
    pub system: Vec<(&'static str, String)>,
}

/// Read the machine the gate opened: the installed set and the marks, the survey the baseline
/// subtracts by, the origin of the installed version of each name a person chose, and the
/// counts of the two packaging systems Lodi does not manage.
///
/// Every backend call here is a read. Nothing below this function builds an invocation.
pub fn read(gate: &Gate) -> Result<Machine, Diagnostic> {
    let backend = pm::backend_for(gate.distro, &gate.root, gate.operation);
    read_with(gate, backend.as_ref())
}

/// The same, over a backend the caller already has. It is what the tests drive, and what keeps
/// this function honest about the reads it makes.
pub fn read_with(gate: &Gate, backend: &dyn pm::Backend) -> Result<Machine, Diagnostic> {
    let observed = backend.observe()?;
    let survey = backend.survey()?;
    // Origin is asked only about the names a person chose. Asking about the whole installed
    // set would be a longer command that answers a question nobody has.
    let chosen = baseline::chosen(&observed);
    let origins = backend.origins(&chosen)?;
    let repositories = sources::read(gate, backend, &origins)?;
    Ok(Machine {
        distro: gate.distro,
        codename: gate.os.codename.clone(),
        observed,
        survey,
        origins,
        snaps: count(&gate.root.join(SNAPS), Some("snap")),
        flatpaks: count(&gate.root.join(FLATPAKS), None),
        repositories,
        users: super::users::humans(&gate.root),
        system: super::basics::readings(gate),
    })
}

/// How many entries a directory holds, optionally only those with one extension. A directory
/// that is not there holds none, which is the honest answer for a machine with no snapd and no
/// flatpak. Nothing is opened, read or followed: the names are counted.
fn count(dir: &Path, extension: Option<&str>) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| match extension {
            Some(wanted) => entry.path().extension().is_some_and(|ext| ext == wanted),
            None => true,
        })
        .count()
}
