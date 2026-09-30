//! The apt backend: Debian 12 and Ubuntu 24.04 (`spec/09` §5.1).
//!
//! Everything a host apply does to packages on these distributions is here, and it is a small
//! set of commands deliberately:
//!
//! | what | the command |
//! | --- | --- |
//! | the installed set | `dpkg-query -W -f=…`, whose status says installed |
//! | the marks | `apt-mark showauto`, `apt-mark showhold` |
//! | the candidates | `apt-cache policy <names…>` |
//! | the index | `apt-get update`, when `/var/lib/apt/lists` is older than six hours |
//! | the transaction | **one** `apt-get install -y --no-install-recommends …`, and under `exact` **one** `apt-get install -y …` |
//! | the marks again | `apt-mark auto` / `manual` / `hold` / `unhold` `<names…>` |
//! | the state of dpkg | `dpkg --audit` |
//! | the conffiles (import only) | `dpkg-query -W -f=${Conffiles}` |
//!
//! Three things are **not** here, and their absence is the design:
//!
//! - no `apt-get upgrade`, `dist-upgrade` or `autoremove`. An apply changes what the manifest
//!   names and nothing else; `auto_remove` is bounded by the lock's record of what Lodi itself
//!   installed (OD-15), and apt's own idea of what is no longer needed is not Lodi's business.
//! - no version constraint, no pin and no snapshot repository. Host packages track the
//!   distribution; a name with a version in it is `E_UNSUPPORTED` at the manifest (OD-15, D9).
//! - no `dpkg --configure -a`. When dpkg is left mid-configuration, the apply stops with
//!   `E_JOURNAL_AMBIGUOUS` and names that command as the **operator's** next step. Running it
//!   for them would be Lodi deciding what happened to a machine it cannot see.
//!
//! Every argv here that carries package names puts `--` before them (LD-358), so that no name
//! can be read as an option, whatever let it through.
//!
//! Removals travel in the install transaction with apt's own `name-` suffix, so that apt solves
//! the whole change at once rather than in two solvable-apart halves.
//!
//! Under `--root DIR` every invocation also carries the root options of design call D2 (LD-114):
//! `-o RootDir=DIR` for the apt programs, `--admindir=DIR/var/lib/dpkg` for `dpkg-query` and
//! `dpkg`. On the root `/` they add nothing, so the argv above is what a machine sees. See
//! [`Apt::root_options`] for what that is claimed to do, and what it is not.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::diag::Diagnostic;

use super::super::manifest::normalize_absolute;
use super::super::safety::{Distro, Operation, resolve_program, searched, untrusted_runtime};
use super::{
    Backend, Conffile, ConffileStatus, Invocation, Observed, Origin, Pinning, Priority, Simulation,
    Survey, capture, noninteractive_env,
};

/// Every program this backend can reach for. The gate requires `apt-get` and `dpkg-query` to be
/// on `PATH` before it opens anything; the other three are resolved here and their absence is
/// `E_NO_RUNTIME` at the moment one is needed, naming the command that is missing.
pub const PROGRAMS: &[&str] = &["apt-get", "apt-cache", "apt-mark", "dpkg-query", "dpkg"];

/// How long before `now` apt last refreshed the lists directory `dir`: the newest time any of its
/// regular files was touched. `None` when it holds none.
///
/// A file's mtime is not that time. apt sets it to the archive's own `Last-Modified`, which can be
/// hours or days before the `apt-get update` that fetched it, and an update in which every line
/// is `Hit:` rewrites nothing. What every update does move is each list file's change time, and
/// nothing else does in the ordinary life of a machine, so a file counts from the later of the two.
fn age_at(dir: &Path, now: SystemTime) -> Option<Duration> {
    let newest = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .filter_map(|meta| {
            let changed = u64::try_from(meta.ctime())
                .ok()
                .zip(u32::try_from(meta.ctime_nsec()).ok())
                .map(|(secs, nanos)| UNIX_EPOCH + Duration::new(secs, nanos));
            meta.modified().ok().max(changed)
        })
        .max()?;
    now.duration_since(newest).ok()
}

/// The end of the options. Every argv that carries package names puts it before them, so that
/// no name can ever be read as an option; a manifest or host-lock name that begins with `-` is
/// also refused long before it reaches here.
const END_OF_OPTIONS: &str = "--";

/// Append `names` to `args` after [`END_OF_OPTIONS`].
fn names_after_options(args: &mut Vec<String>, names: &[String]) {
    args.push(END_OF_OPTIONS.to_string());
    args.extend(names.iter().cloned());
}

/// The format string `dpkg-query` is given, exactly. Tabs separate the fields, because a
/// version never holds one and a package name cannot; the status is the abbreviated one, read by
/// [`is_installed`].
///
/// The name field is `${Package}` and **not** `${binary:Package}` (LD-305, fixed by LD-312).
/// `${binary:Package}` prints an architecture-qualified name — `libc6:amd64` — for every
/// Multi-Arch package, while `apt-mark showauto`, `apt-mark showhold`, `apt-cache policy` and
/// every name a manifest declares are plain. Reading the installed set under the one identity
/// all of those already use is what makes the comparisons meet: a qualified name matched
/// nothing, so on a Debian 12 machine 143 libraries of 333 never met the automatic set and were
/// listed in the emitted `NOT CAPTURED` block as names the repositories offer no candidate for.
///
/// A package installed for two architectures at once prints its plain name twice; the later
/// line wins, which is the same package at the same version under either reading.
pub const QUERY_FORMAT: &str = "-f=${Package}\t${Version}\t${db:Status-Abbrev}\n";

/// The second `dpkg-query` format: the fields the **import** baseline subtracts by, and that no
/// apply has ever needed. It reads dpkg's own status data and nothing else — no network, no
/// image manifest, no shipped table (design call D3, LD-271).
///
/// Its name field is `${Package}` for the reason [`QUERY_FORMAT`]'s is (LD-305, fixed by
/// LD-312): the survey feeds the import, whose priority, metapackage and `Depends` lookups are
/// all made under the installed set's own names, so a qualified name here would find no row.
pub const SURVEY_FORMAT: &str = "-f=${Package}\t${db:Status-Abbrev}\t${Priority}\t\
                                 ${Section}\t${Depends}\t${Recommends}\n";

/// The third `dpkg-query` format: dpkg's own record of the configuration files it shipped.
///
/// `${Conffiles}` is a multi-line field — one line per conffile, ` <path> <md5> [flag]` — so it
/// is asked for on its own rather than inside a tab-separated row, and a package with no
/// conffile contributes an empty line. It is the list of files **dpkg itself** calls
/// configuration; nothing else under `/etc` is a candidate for import (design call D9).
pub const CONFFILES_FORMAT: &str = "-f=${Conffiles}\n";

/// How large a conffile may be before this backend declines to digest it. Nothing is captured
/// anywhere near this size (the capture's own cap is 64 KiB), so the only effect of the bound is
/// that a pathological `/etc` cannot make a read command read gigabytes.
pub const MAX_DIGESTED_BYTES: u64 = 8 * 1024 * 1024;

/// MD5, because that is the digest **dpkg** recorded for its conffiles and the comparison has to
/// be made in dpkg's own terms.
///
/// It is not used as a security primitive anywhere in this tree and nothing is trusted because
/// it matches: the answer decides whether a file is offered to the capture, which then applies
/// its own refusal policy to it. RFC 1321, implemented here rather than taken as a dependency —
/// one small function against a published test vector is cheaper than a crate in the audited
/// closure of `docs/licenses/DEPENDENCIES.md`.
pub fn md5_hex(bytes: &[u8]) -> String {
    /// The per-round left rotations of RFC 1321 §3.4.
    const SHIFT: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    /// The sine table of RFC 1321 §3.4, written out rather than computed, so that the constants
    /// do not depend on this machine's floating point.
    const K: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];

    let mut message = bytes.to_vec();
    let bit_length = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_le_bytes());

    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for chunk in message.chunks_exact(64) {
        let mut word = [0u32; 16];
        for (index, slot) in word.iter_mut().enumerate() {
            let at = index * 4;
            *slot = u32::from_le_bytes([chunk[at], chunk[at + 1], chunk[at + 2], chunk[at + 3]]);
        }
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (mixed, index) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let turned = a
                .wrapping_add(mixed)
                .wrapping_add(K[i])
                .wrapping_add(word[index]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(turned.rotate_left(SHIFT[i]));
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut out = String::with_capacity(32);
    for value in state {
        for byte in value.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

/// The kernel and firmware metapackages of this family: a package that carries no kernel itself
/// and exists only to depend on whichever versioned one is current. They are this distribution's
/// own metapackages as surely as a `task-` package is, and a machine restored without one does
/// not boot, so the baseline treats them as seeds rather than subtracting them.
///
/// It is a committed list of exact names, not a pattern: there are no regular expressions
/// anywhere in this tree (LD-80). A kernel metapackage this build does not know is simply not
/// recognised as one, which subtracts nothing and declares nothing wrongly.
pub const KERNEL_METAPACKAGES: &[&str] = &[
    "linux-generic",
    "linux-image-amd64",
    "linux-image-arm64",
    "linux-image-generic",
    "linux-image-virtual",
    "linux-virtual",
];

/// The section a distribution gives its own task and system metapackages. Ubuntu qualifies it
/// with the component it lives in (`universe/metapackages`), so the last path element is what is
/// compared.
fn is_metapackage_section(section: &str) -> bool {
    section
        .rsplit('/')
        .next()
        .is_some_and(|last| last.trim() == "metapackages")
}

/// The package names a `Depends` or `Recommends` field lists.
///
/// The field is a comma-separated list of alternatives separated by `|`, each name optionally
/// carrying an architecture qualifier (`libfoo:any`) and a version relation
/// (`libc6 (>= 2.34)`). Every alternative counts: a metapackage that reaches a package one way
/// or another still reaches it. There are no regular expressions anywhere in this tree (LD-80).
pub fn dependency_names(field: &str) -> Vec<String> {
    let mut out = Vec::new();
    for clause in field.split(',') {
        for alternative in clause.split('|') {
            let name = alternative
                .trim()
                .split(['(', '[', '<'])
                .next()
                .unwrap_or("")
                .trim()
                .split(':')
                .next()
                .unwrap_or("")
                .trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Where apt keeps what it fetched from the index, inside the root.
pub const LISTS: &str = "var/lib/apt/lists";

/// Is a package with this `${db:Status-Abbrev}` installed and usable?
///
/// The field is three characters — the wanted state, the current state, then dpkg's error flag —
/// and only the middle one says whether the package is there. It is easy to read the first two
/// as one word and test for `ii`, and that is wrong in exactly the case this backend creates
/// itself: a held package reads `hi`, because holding changes the *wanted* state. A package
/// that is merely unpacked (`iU`) or half-configured (`iF`) is not installed, and one dpkg wants
/// reinstalled (error flag `R`) is not usable, so neither counts.
fn is_installed(abbrev: &str) -> bool {
    let mut chars = abbrev.chars();
    let (Some(_want), Some(state)) = (chars.next(), chars.next()) else {
        return false;
    };
    state == 'i' && chars.next().is_none_or(|eflag| eflag != 'R')
}

pub struct Apt {
    distro: Distro,
    root: PathBuf,
    programs: BTreeMap<String, PathBuf>,
    /// Programs that were found and are not trusted, with the reason (LD-357).
    refused: BTreeMap<String, String>,
    /// The command being run, which a missing program's hint names.
    operation: Operation,
    /// `[host] packages = "exact"`: the transaction and its simulation keep apt's own default and
    /// install what a new package recommends (LD-375 validator repair). `managed` and a manifest
    /// without the key keep 1.1.0's `--no-install-recommends`.
    recommends: bool,
}

impl Apt {
    /// Resolve every program this backend might use, by bare name (D3, LD-115), from a trusted
    /// place ([`resolve_program`], LD-357). A program that is there and is refused is kept with
    /// its refusal, which is what a use of it reports. Nothing is run here: a backend is built by
    /// `plan`, which mutates nothing.
    pub fn new(distro: Distro, root: &Path, operation: Operation) -> Apt {
        let system_root = root == Path::new("/");
        let euid = super::super::safety::current_euid();
        let mut programs = BTreeMap::new();
        let mut refused = BTreeMap::new();
        for name in PROGRAMS {
            match resolve_program(name, system_root, euid) {
                Ok(Some(found)) => {
                    programs.insert((*name).to_string(), found);
                }
                Ok(None) => {}
                Err(why) => {
                    refused.insert((*name).to_string(), why);
                }
            }
        }
        Apt {
            distro,
            root: root.to_path_buf(),
            programs,
            refused,
            operation,
            recommends: false,
        }
    }

    /// The environment every apt invocation gets: the seam's own non-interactive environment
    /// plus apt's changelog frontend, so that an install never stops to show one.
    fn env() -> Vec<(String, String)> {
        let mut env = noninteractive_env();
        env.push(("APT_LISTCHANGES_FRONTEND".to_string(), "none".to_string()));
        env
    }

    /// What points a program at `--root DIR` instead of the running machine (design call D2,
    /// LD-114): apt's own `RootDir`, and for `dpkg-query` and `dpkg` the database below that
    /// root. The root `/` adds nothing, so the argv on a machine is the one `spec/09` §5.1 and
    /// T-4 fix, and the one the guest evidence shows.
    ///
    /// LD-114 says what this is and is not: Lodi writes these options and asserts them, and
    /// nothing here claims that a real apt and dpkg then change that tree's package database
    /// rather than the running machine's. `README.md` says the same in the operator's words.
    fn root_options(&self, program: &str) -> Vec<String> {
        if self.root == Path::new("/") {
            return Vec::new();
        }
        match program {
            "dpkg-query" | "dpkg" => {
                vec![format!(
                    "--admindir={}",
                    self.root.join("var/lib/dpkg").display()
                )]
            }
            _ => vec!["-o".to_string(), format!("RootDir={}", self.root.display())],
        }
    }

    /// One invocation, with the root options after the subcommand — where an option belongs for
    /// every one of these programs, and where it leaves the subcommand itself readable.
    fn invocation(&self, program: &str, args: &[String]) -> Result<Invocation, Diagnostic> {
        if let Some(why) = self.refused.get(program) {
            return Err(untrusted_runtime(program, why));
        }
        let path = self
            .programs
            .get(program)
            .ok_or_else(|| missing(program, &self.root, self.operation))?;
        let mut full: Vec<String> = Vec::with_capacity(args.len() + 2);
        if let Some((subcommand, rest)) = args.split_first() {
            full.push(subcommand.clone());
            full.extend(self.root_options(program));
            full.extend(rest.iter().cloned());
        } else {
            full.extend(self.root_options(program));
        }
        Ok(Invocation::resolved(program, path, &full, Apt::env()))
    }

    fn read(&self, program: &str, args: &[&str]) -> Result<String, Diagnostic> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        capture(&self.invocation(program, &args)?)
    }

    /// The names `apt-mark showauto`/`showhold` prints, one per line.
    fn marked(&self, what: &str) -> Result<BTreeSet<String>, Diagnostic> {
        let text = self.read("apt-mark", &[what])?;
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect())
    }
}

impl Apt {
    /// Compare one recorded conffile digest against the file as it is now, without opening
    /// anything the invoking user needs a privilege to read.
    fn conffile_status(&self, path: &str, recorded: &str) -> ConffileStatus {
        let full = self.root.join(path.trim_start_matches('/'));
        let Ok(meta) = fs::symlink_metadata(&full) else {
            // The file dpkg shipped is not there. There is nothing to capture and nothing to
            // warn about: a conffile the person removed is not a conffile they changed.
            return ConffileStatus::Unmodified;
        };
        let readable_by_other = meta.permissions().mode() & 0o004 != 0;
        if !meta.file_type().is_file() || !readable_by_other || meta.len() > MAX_DIGESTED_BYTES {
            return ConffileStatus::Undetermined;
        }
        // A path lodi never reads is not opened to be compared either (si-1).
        if super::super::import::files::never_captured(path) {
            return ConffileStatus::Undetermined;
        }
        // Opened as a managed path is: never blocking, and read only if it is a regular file.
        let Ok(mut handle) = super::super::files::open_regular(&full, path) else {
            return ConffileStatus::Undetermined;
        };
        let mut bytes = Vec::new();
        if handle.read_to_end(&mut bytes).is_err() {
            return ConffileStatus::Undetermined;
        }
        if md5_hex(&bytes) == recorded.trim().to_ascii_lowercase() {
            ConffileStatus::Unmodified
        } else {
            ConffileStatus::Modified
        }
    }
}

/// The refusal for a `Conffiles` entry whose path leaves the selected root once normalised.
fn conffile_escape(root: &Path, path: &str) -> Diagnostic {
    Diagnostic::new(
        "E_PATH_ESCAPE",
        format!(
            "{path}: dpkg lists this configuration file below {}, and it does not stay inside \
             that root",
            root.display()
        ),
    )
    .hint(
        "lodi reads a configuration file only inside the selected root; the package database \
         of that root names a path outside it, so check that database. Nothing was written",
    )
}

fn missing(program: &str, root: &Path, operation: Operation) -> Diagnostic {
    Diagnostic::new(
        "E_NO_RUNTIME",
        format!(
            "`{program}` is not on {}, and the apt backend needs it",
            searched(root == Path::new("/"))
        ),
    )
    .hint(format!(
        "install the distribution's `apt` and `dpkg` packages, or run `{}` on a machine that has \
         them",
        operation.command()
    ))
}

impl Backend for Apt {
    fn keep_default_recommends(&mut self) {
        self.recommends = true;
    }

    fn distro(&self) -> Distro {
        self.distro
    }

    /// `dpkg-query -W -f=…`, keeping only what is installed **and** configured.
    ///
    /// `dpkg-query` exits non-zero when it was asked about a name it does not know; asked about
    /// everything, as here, it does not, so a failure is a real failure and is reported.
    fn observe(&self) -> Result<Observed, Diagnostic> {
        let text = self.read("dpkg-query", &["-W", QUERY_FORMAT])?;
        let mut installed = BTreeMap::new();
        for line in text.lines() {
            let mut fields = line.split('\t');
            let (Some(name), Some(version), Some(status)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if !name.is_empty() && is_installed(status) {
                installed.insert(name.to_string(), version.to_string());
            }
        }
        Ok(Observed {
            installed,
            auto: self.marked("showauto")?,
            held: self.marked("showhold")?,
        })
    }

    /// `apt-cache policy <names…>`: one call, one block per name apt knows.
    ///
    /// A name apt does not know produces no block at all, and a name whose candidate is
    /// `(none)` — a package that exists in the index but is installable from no enabled source —
    /// is treated the same way, because neither can be installed.
    fn candidates(&self, names: &[String]) -> Result<BTreeMap<String, String>, Diagnostic> {
        if names.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut args = vec!["policy".to_string()];
        names_after_options(&mut args, names);
        let text = capture(&self.invocation("apt-cache", &args)?)?;
        Ok(parse_policy(&text))
    }

    /// Where the installed version of each named package came from: the source lines of that
    /// version from `apt-cache policy <names…>`, matched against the `release` fields the
    /// no-argument `apt-cache policy` states for each enabled source.
    ///
    /// Both are index-local reads that make no network request. The second one is the reading
    /// R-2 added to this backend, and it is the only way to tell apt's own repositories from an
    /// enabled third-party one: an **installed** package always has a candidate, because apt
    /// counts `/var/lib/dpkg/status` as a source, so candidate existence said "yes" for every
    /// Docker, PPA and hand-installed package there has ever been.
    fn origins(&self, names: &[String]) -> Result<BTreeMap<String, Origin>, Diagnostic> {
        if names.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut args = vec!["policy".to_string()];
        names_after_options(&mut args, names);
        let per_name = parse_policy_sources(&capture(&self.invocation("apt-cache", &args)?)?);
        let table = parse_release_table(&self.read("apt-cache", &["policy"])?);
        if !has_index(&table) {
            return Ok(names
                .iter()
                .map(|name| (name.clone(), Origin::Unchecked))
                .collect());
        }
        Ok(names
            .iter()
            .map(|name| {
                let sources = per_name.get(name).map(Vec::as_slice).unwrap_or_default();
                (name.clone(), classify(self.distro, sources, &table))
            })
            .collect())
    }

    /// The same two `apt-cache policy` readings [`Backend::origins`] makes, returned before any
    /// classification: the source lines of each name's installed version and the `o=` of every
    /// enabled source. No new command and no new parser: the argv is built by the same
    /// [`Apt::invocation`], and the text is read by [`parse_policy_sources`] and
    /// [`parse_release_table`].
    fn offers(&self, names: &[String]) -> Result<super::Offers, Diagnostic> {
        let mut offers = super::Offers::default();
        if !names.is_empty() {
            let mut args = vec!["policy".to_string()];
            names_after_options(&mut args, names);
            let per_name = parse_policy_sources(&capture(&self.invocation("apt-cache", &args)?)?);
            for name in names {
                let sources = per_name.get(name).map(Vec::as_slice).unwrap_or_default();
                offers.installed_from.insert(
                    name.clone(),
                    sources
                        .iter()
                        .filter(|source| *source != STATUS_SOURCE)
                        .cloned()
                        .collect(),
                );
            }
        }
        for (source, release) in parse_release_table(&self.read("apt-cache", &["policy"])?) {
            if source != STATUS_SOURCE {
                offers.codename_of.insert(source.clone(), release.codename);
                offers.origin_of.insert(source, release.origin);
            }
        }
        Ok(offers)
    }

    /// The nearest names among those the index knows that begin as this one does.
    ///
    /// `apt-cache pkgnames <prefix>` is asked rather than the whole index, which is tens of
    /// thousands of names: a misspelling that is nowhere near any real name deserves no
    /// suggestion, and one that is near a real name almost always starts the same way.
    fn nearest(&self, name: &str) -> Vec<String> {
        let prefix: String = name.chars().take(2).collect();
        if prefix.is_empty() {
            return Vec::new();
        }
        let Ok(text) = self.read("apt-cache", &["pkgnames", END_OF_OPTIONS, &prefix]) else {
            return Vec::new();
        };
        let known: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect();
        crate::tools::nearest(name, &known)
    }

    /// A second `dpkg-query -W`, with the format the import baseline needs: the priority the
    /// distribution gave each installed package, the section that names its own metapackages,
    /// and the `Depends`/`Recommends` those metapackages reach.
    ///
    /// `foreign` is empty here, and it is not a gap. apt keeps no mark for "this came from no
    /// repository"; what it keeps is the **source of the installed version**, which
    /// [`Backend::origins`] reads and the import classifies by. The claim that used to stand
    /// here — that a hand-installed `.deb` is recognised by the index offering no candidate for
    /// it — was false of every apt machine, because `apt-cache policy` counts
    /// `/var/lib/dpkg/status` as a source at priority 100 for anything installed. R-2 replaced
    /// it; the decision record supersedes it by name.
    ///
    /// `keep` is empty here: this family's kernels arrive as the metapackages of
    /// [`KERNEL_METAPACKAGES`], which are seeds of the subtraction rather than survivors of it,
    /// so there is nothing left to rescue from it.
    fn survey(&self) -> Result<Survey, Diagnostic> {
        let text = self.read("dpkg-query", &["-W", SURVEY_FORMAT])?;
        let mut survey = Survey::default();
        for line in text.lines() {
            let mut fields = line.split('\t');
            let (Some(name), Some(status), Some(priority), Some(section)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if name.is_empty() || !is_installed(status) {
                continue;
            }
            survey
                .priority
                .insert(name.to_string(), Priority::from_field(priority));
            if is_metapackage_section(section) || KERNEL_METAPACKAGES.contains(&name) {
                survey.metapackages.insert(name.to_string());
            }
            let mut reached = BTreeSet::new();
            for field in fields {
                reached.extend(dependency_names(field));
            }
            survey.depends.insert(name.to_string(), reached);
        }
        Ok(survey)
    }

    /// dpkg's own list of the configuration files it shipped, and what became of each.
    ///
    /// `${Conffiles}` records the path and the MD5 of the bytes the package installed, so the
    /// verdict is that digest against the file as it is now. An entry dpkg marks `obsolete` is
    /// no longer part of any installed package and is not offered; a path that is gone is not
    /// offered either, because there is nothing to import from a file the person deleted.
    ///
    /// **Nothing whose mode denies read to *other* is opened here.** That file is
    /// [`ConffileStatus::Undetermined`], the capture refuses it by its mode, and the import
    /// therefore never reads a file with a privilege the invoking user does not already have.
    fn conffiles(&self) -> Result<Vec<Conffile>, Diagnostic> {
        let text = self.read("dpkg-query", &["-W", CONFFILES_FORMAT])?;
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(path), Some(recorded)) = (fields.next(), fields.next()) else {
                continue;
            };
            // ` obsolete` and ` newconffile` are dpkg's own markers for an entry that is no
            // longer a live configuration file of an installed package.
            if fields.next().is_some() || !path.starts_with('/') {
                continue;
            }
            // The path is the package database's word, and it is resolved below the selected
            // root: normalised first, and refused when its `..` would leave that root.
            let path = normalize_absolute(path).ok_or_else(|| conffile_escape(&self.root, path))?;
            if !seen.insert(path.clone()) {
                continue;
            }
            let status = self.conffile_status(&path, recorded);
            out.push(Conffile { path, status });
        }
        Ok(out)
    }

    /// How long ago apt last refreshed its lists directory (`age_at`). A directory that is not
    /// there, or holds nothing, is `None`: no index at all, which an apply refreshes.
    fn index_age(&self) -> Option<Duration> {
        age_at(&self.root.join(LISTS), SystemTime::now())
    }

    fn refresh(&self) -> Result<Vec<Invocation>, Diagnostic> {
        Ok(vec![self.invocation("apt-get", &["update".to_string()])?])
    }

    /// **One** `apt-get install`, carrying the installs and, with apt's `name-` suffix, the
    /// removals, so that apt solves the whole change together.
    fn transaction(
        &self,
        install: &[String],
        remove: &[String],
    ) -> Result<Vec<Invocation>, Diagnostic> {
        if install.is_empty() && remove.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![self.invocation(
            "apt-get",
            &transaction_args(install, remove, false, self.recommends, &Pinning::default()),
        )?])
    }

    /// The transaction of a pinned apply (M-Pin, LD-395): the same `apt-get install`, with
    /// `NAME=VERSION` for a pinned name, the private source set's options when it reads one, and
    /// `--allow-downgrades` / `--allow-change-held-packages` only when the plan names a downgrade
    /// or moves a held name.
    fn transaction_pinned(
        &self,
        install: &[String],
        remove: &[String],
        pinning: &Pinning,
    ) -> Result<Vec<Invocation>, Diagnostic> {
        if install.is_empty() && remove.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![self.invocation(
            "apt-get",
            &transaction_args(install, remove, false, self.recommends, pinning),
        )?])
    }

    /// `apt-get update` of the private dated source set, into its own lists (P4): the machine's
    /// `/etc/apt` and `/var/lib/apt/lists` are neither read as a source nor written.
    fn private_refresh(&self) -> Result<Vec<Invocation>, Diagnostic> {
        let mut args = vec!["update".to_string()];
        args.extend(super::super::pin::private_options());
        Ok(vec![self.invocation("apt-get", &args)?])
    }

    fn simulate_pinned(
        &self,
        install: &[String],
        remove: &[String],
        pinning: &Pinning,
    ) -> Result<Simulation, Diagnostic> {
        let text = capture(&self.invocation(
            "apt-get",
            &transaction_args(install, remove, true, self.recommends, pinning),
        )?)?;
        Ok(parse_simulation(&text))
    }

    /// `apt-get install -s …`: the transaction's own argv with apt's simulation option, so what
    /// is read is what exactly that command would do (LD-375). The `Remv` lines are what it
    /// would remove, and the "no longer required" block what it would leave for autoremove.
    /// Nothing changes, and no `autoremove` of any kind is ever run.
    fn simulate(&self, install: &[String], remove: &[String]) -> Result<Simulation, Diagnostic> {
        let text = capture(&self.invocation(
            "apt-get",
            &transaction_args(install, remove, true, self.recommends, &Pinning::default()),
        )?)?;
        Ok(parse_simulation(&text))
    }

    /// The `Inst` lines of the transaction's own simulation, then `apt-cache show
    /// --no-all-versions` of those names: the candidate record apt would install of each, read
    /// for the fields [`Backend::survey`] reads of an installed package (LD-412). Both are reads.
    fn arriving(&self, install: &[String]) -> Result<Survey, Diagnostic> {
        if install.is_empty() {
            return Ok(Survey::default());
        }
        let names: Vec<String> = self.simulate(install, &[])?.installed.into_iter().collect();
        if names.is_empty() {
            return Ok(Survey::default());
        }
        let mut args = vec!["show".to_string(), "--no-all-versions".to_string()];
        names_after_options(&mut args, &names);
        Ok(parse_show(&capture(&self.invocation("apt-cache", &args)?)?))
    }

    fn marks(&self, auto: &[String], manual: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        let mut out = Vec::new();
        for (verb, names) in [("auto", auto), ("manual", manual)] {
            if names.is_empty() {
                continue;
            }
            let mut args = vec![verb.to_string()];
            names_after_options(&mut args, names);
            out.push(self.invocation("apt-mark", &args)?);
        }
        Ok(out)
    }

    fn holds(&self, hold: &[String], unhold: &[String]) -> Result<Vec<Invocation>, Diagnostic> {
        let mut out = Vec::new();
        for (verb, names) in [("hold", hold), ("unhold", unhold)] {
            if names.is_empty() {
                continue;
            }
            let mut args = vec![verb.to_string()];
            names_after_options(&mut args, names);
            out.push(self.invocation("apt-mark", &args)?);
        }
        Ok(out)
    }

    /// `dpkg --audit`: every package dpkg considers half-installed, half-configured or in any
    /// other state that is not finished. It prints nothing at all when the database is clean.
    fn unclean(&self) -> Result<Vec<String>, Diagnostic> {
        let text = self.read("dpkg", &["--audit"])?;
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect())
    }
}

/// The argv of the one `apt-get install`, or of its simulation: the installs, then each removal
/// with apt's `name-` suffix, after the end of the options. `recommends` keeps apt's own default
/// of installing what a new package recommends; without it the argv is 1.1.0's, byte for byte.
/// A default `pinning` adds nothing, so an unpinned transaction is what it always was (P3).
fn transaction_args(
    install: &[String],
    remove: &[String],
    simulate: bool,
    recommends: bool,
    pinning: &Pinning,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["install".to_string()];
    if simulate {
        args.push("-s".to_string());
    }
    args.push("-y".to_string());
    if !recommends {
        args.push("--no-install-recommends".to_string());
    }
    args.extend(
        [
            "-o",
            "Dpkg::Options::=--force-confdef",
            "-o",
            "Dpkg::Options::=--force-confold",
        ]
        .iter()
        .map(|a| (*a).to_string()),
    );
    if pinning.private {
        args.extend(super::super::pin::private_options());
    }
    if pinning.allow_downgrades {
        args.push("--allow-downgrades".to_string());
    }
    if pinning.allow_change_held {
        args.push("--allow-change-held-packages".to_string());
    }
    let removals: Vec<String> = remove.iter().map(|name| format!("{name}-")).collect();
    names_after_options(&mut args, &[install, &removals].concat());
    args
}

/// What `apt-get install -s` printed: its `Remv NAME [VERSION]` lines, its `Inst NAME (VERSION
/// …)` lines for a package that is not installed, and the names of the block that begins
/// `…automatically installed and (is|are) no longer required:`, whose names are indented on the
/// lines after it. An upgrade is `Inst NAME [INSTALLED] (VERSION …)`: the package is on the
/// machine already, and is not one the change brings (LD-412).
pub fn parse_simulation(text: &str) -> Simulation {
    let mut simulation = Simulation::default();
    let mut in_orphans = false;
    for line in text.lines() {
        if in_orphans {
            if line.starts_with(char::is_whitespace) {
                // A name apt qualifies with another architecture is not one lodi can name
                // exactly in a removal, so it is not offered as one.
                simulation.orphans.extend(
                    line.split_whitespace()
                        .filter(|name| !name.contains(':'))
                        .map(ToString::to_string),
                );
                continue;
            }
            in_orphans = false;
        }
        if line.trim_end().ends_with("no longer required:") {
            in_orphans = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("Remv ")
            && let Some(name) = rest.split_whitespace().next()
        {
            // `name:arch` is the same package under the one identity every other reading uses.
            let name = name.split(':').next().unwrap_or(name);
            simulation.removed.insert(name.to_string());
        }
        if let Some(rest) = line.strip_prefix("Inst ") {
            let mut words = rest.split_whitespace();
            if let Some(name) = words.next()
                && !words.next().is_some_and(|word| word.starts_with('['))
            {
                let name = name.split(':').next().unwrap_or(name);
                simulation.installed.insert(name.to_string());
            }
        }
    }
    simulation
}

/// `apt-cache show --no-all-versions` output: for each package's candidate record, the fields
/// the survey reads of an installed one ([`SURVEY_FORMAT`]) — `Priority`, `Section`, and the
/// names `Depends` and `Recommends` list — as a [`Survey`] whose `depends` has every record's
/// package as a key (LD-412).
///
/// A record is a block of `Field: value` lines ended by a blank line; a line that begins with
/// whitespace continues the field before it, which is how a long `Description` is laid out.
pub fn parse_show(text: &str) -> Survey {
    let mut survey = Survey::default();
    let mut record: BTreeMap<String, String> = BTreeMap::new();
    let mut last: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            show_record(&mut record, &mut survey);
            last = None;
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            if let Some(key) = &last
                && let Some(value) = record.get_mut(key)
            {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            record.insert(key.trim().to_string(), value.trim().to_string());
            last = Some(key.trim().to_string());
        }
    }
    show_record(&mut record, &mut survey);
    survey
}

/// One record of [`parse_show`], into the survey, and the record emptied for the next.
fn show_record(record: &mut BTreeMap<String, String>, survey: &mut Survey) {
    let field = |key: &str| record.get(key).map_or("", |value| value.as_str());
    let name = field("Package").trim().to_string();
    if !name.is_empty() {
        survey
            .priority
            .insert(name.clone(), Priority::from_field(field("Priority").trim()));
        if is_metapackage_section(field("Section")) || KERNEL_METAPACKAGES.contains(&name.as_str())
        {
            survey.metapackages.insert(name.clone());
        }
        let reached = survey.depends.entry(name).or_default();
        for key in ["Depends", "Recommends"] {
            reached.extend(dependency_names(field(key)));
        }
    }
    record.clear();
}

/// `apt-cache policy` output: one block per name, `Candidate:` inside it.
///
/// ```text
/// bc:
///   Installed: (none)
///   Candidate: 1.07.1-3+b1
///   Version table:
/// ```
pub fn parse_policy(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut name: Option<String> = None;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            let trimmed = line.trim();
            name = trimmed
                .strip_suffix(':')
                .filter(|n| !n.is_empty() && !n.contains(char::is_whitespace))
                .map(ToString::to_string);
            continue;
        }
        let Some(candidate) = line.trim().strip_prefix("Candidate:") else {
            continue;
        };
        let candidate = candidate.trim();
        if let Some(name) = &name
            && candidate != "(none)"
            && !candidate.is_empty()
        {
            out.insert(name.clone(), candidate.to_string());
        }
    }
    out
}

/// dpkg's own status file, which `apt-cache policy` counts as a source at priority 100 for
/// every installed package. A name whose installed version has **only** this source line came
/// from a package file installed by hand, or from a source the machine no longer has.
pub const STATUS_SOURCE: &str = "/var/lib/dpkg/status";

/// The suffixes of a suite that a fresh install of the same release does not enable by itself.
/// A name from one of them is still the distribution's own and is still declared; its installed
/// **version** is simply not the one a fresh machine would get, which the emitted file says.
///
/// It is a committed list of exact suffixes, not a pattern: there are no regular expressions
/// anywhere in this tree (LD-80).
pub const NON_DEFAULT_SUITES: &[&str] = &["-backports", "-proposed"];

/// What a source whose label could be an address, or which states none, is called in the
/// emitted file.
pub const UNLABELLED: &str = "unlabelled source";

/// The `release` fields `apt-cache policy` with no argument states for one source.
///
/// It is the machine's own reading of each enabled source's `Release` file — never a table
/// shipped in this binary (design call D3, LD-271) — and it is the only thing that tells a
/// Docker or a PPA source from the distribution's own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Release {
    /// `o=`: the `Origin:` of that source's `Release` file (`Debian`, `Docker`, `LP-PPA-…`).
    pub origin: String,
    /// `l=`: its `Label:`.
    pub label: String,
    /// `a=`: its suite (`stable`, `bookworm`, `bookworm-backports`).
    pub suite: String,
    /// `n=`: its `Codename:` — on Debian the suite's codename form (`oldstable-security` is
    /// `bookworm-security`), on Ubuntu the release's codename for every suite (`noble`).
    pub codename: String,
    /// `c=`: its component (`main`, `stable`, `universe`).
    pub component: String,
}

/// A field of a [`Release`] that may be written into the emitted file.
///
/// The emitted manifest carries **labels, never addresses** (`emit.rs`): a third-party source
/// is free to put its own host name in `Origin:`, and a host name is one of the things
/// `AGENTS.md` §1.2 keeps out of this repository, where a guest's emitted file is committed as
/// evidence. A field that could be an address — anything carrying a dot, a slash, an `@` or the
/// letters `http` — is therefore not used, and the name falls back to [`UNLABELLED`]. Nothing
/// is redacted: the whole field is either usable or replaced.
pub fn address_free(field: &str) -> Option<&str> {
    let field = field.trim();
    if field.is_empty() {
        return None;
    }
    let unusable = field.contains('.')
        || field.contains('/')
        || field.contains('@')
        || field.contains(':')
        || field.contains("http");
    (!unusable).then_some(field)
}

/// `apt-cache policy <names…>` output: for each name, the source lines of the version that is
/// **installed**, in the order apt printed them.
///
/// ```text
/// docker-ce:
///   Installed: 5:27.3.1-1~debian.12~bookworm
///   Candidate: 5:27.3.1-1~debian.12~bookworm
///   Version table:
///  *** 5:27.3.1-1~debian.12~bookworm 500
///         500 https://download.docker.com/linux/debian bookworm/stable amd64 Packages
///         100 /var/lib/dpkg/status
/// ```
///
/// The version table indents a version row five columns and a source row eight, and marks the
/// installed version with `***`. A source row's first word is its pin priority; what follows is
/// the key the no-argument listing states that source's `release` fields under, so the two
/// readings meet on that text and on nothing else.
pub fn parse_policy_sources(text: &str) -> BTreeMap<String, Vec<String>> {
    /// How deep `apt-cache policy` indents a source row of the version table.
    const SOURCE_INDENT: usize = 8;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut name: Option<String> = None;
    let mut installed = false;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            let trimmed = line.trim();
            name = trimmed
                .strip_suffix(':')
                .filter(|n| !n.is_empty() && !n.contains(char::is_whitespace))
                .map(ToString::to_string);
            installed = false;
            continue;
        }
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent < SOURCE_INDENT {
            // A version row. `***` is apt's own mark for the version that is installed.
            installed = trimmed.starts_with("***");
            continue;
        }
        let Some((_pin, source)) = trimmed.trim_end().split_once(' ') else {
            continue;
        };
        if let Some(name) = &name
            && installed
        {
            out.entry(name.clone())
                .or_default()
                .push(source.trim().to_string());
        }
    }
    out
}

/// `apt-cache policy` with **no argument**: the `release` fields of every enabled source.
///
/// ```text
/// Package files:
///  100 /var/lib/dpkg/status
///      release a=now
///  500 https://download.docker.com/linux/debian bookworm/stable amd64 Packages
///      release o=Docker,a=bookworm,n=bookworm,l=Docker CE,c=stable,b=amd64
///      origin download.docker.com
/// ```
///
/// The `origin` line is deliberately not read: it is a host name, and nothing here may carry
/// one. It is index-local and makes no network request, which is what lets an import stay a
/// read of the machine.
pub fn parse_release_table(text: &str) -> BTreeMap<String, Release> {
    let mut out: BTreeMap<String, Release> = BTreeMap::new();
    let mut key: Option<String> = None;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            key = None;
            continue;
        }
        let trimmed = line.trim();
        if let Some(fields) = trimmed.strip_prefix("release ") {
            let Some(key) = &key else { continue };
            let entry = out.entry(key.clone()).or_default();
            for field in fields.split(',') {
                let Some((name, value)) = field.split_once('=') else {
                    continue;
                };
                let value = value.trim().to_string();
                match name.trim() {
                    "o" => entry.origin = value,
                    "l" => entry.label = value,
                    "a" => entry.suite = value,
                    "n" => entry.codename = value,
                    "c" => entry.component = value,
                    _ => {}
                }
            }
            continue;
        }
        let Some((pin, source)) = trimmed.split_once(' ') else {
            continue;
        };
        if !pin.is_empty() && pin.chars().all(|c| c.is_ascii_digit()) {
            key = Some(source.trim().to_string());
        }
    }
    out
}

/// Whether this machine has a package index at all: an enabled source that is not its own
/// database.
///
/// A machine whose `/var/lib/apt/lists` has never been fetched — every fresh cloud image, until
/// `apt-get update` runs on it — lists `/var/lib/dpkg/status` and nothing else, and then
/// `apt-cache policy` reports that one source for everything installed. Classifying on that
/// reading would call every package on the machine hand-installed, which is false. There is no
/// second reading that could answer instead: an import runs no command that changes the machine
/// and makes no network request, and fetching an index is both.
pub fn has_index(table: &BTreeMap<String, Release>) -> bool {
    table.keys().any(|source| source != STATUS_SOURCE)
}

/// Where one installed version came from, given its source lines and the release table.
///
/// A version several sources offer takes the best answer any one of them gives — base before a
/// non-default suite before a third party — because an apply on a fresh machine only needs one
/// source that can install it.
pub fn classify(distro: Distro, sources: &[String], table: &BTreeMap<String, Release>) -> Origin {
    let mut best: Option<Origin> = None;
    for source in sources {
        if source == STATUS_SOURCE {
            continue;
        }
        let found = table.get(source).cloned().unwrap_or_default();
        let candidate = classify_one(distro, &found);
        best = Some(match best.take() {
            Some(current) if rank(&current) <= rank(&candidate) => current,
            _ => candidate,
        });
    }
    best.unwrap_or(Origin::LocalOnly)
}

/// How good an answer one source is: the lower, the better.
fn rank(origin: &Origin) -> u8 {
    match origin {
        Origin::Base => 0,
        Origin::Suite(_) => 1,
        Origin::ThirdParty(_) => 2,
        Origin::LocalOnly => 3,
        // Never ranked: `Unchecked` is decided for the whole machine before any source is
        // looked at, so no source ever yields it. It ranks worst so that adding it here can
        // never make an answer better than one a real source gave.
        Origin::Unchecked => 4,
    }
}

/// One source's verdict. **Base** is `o=` equal to the distribution this root runs, which is
/// mirror-independent and is the same repository set `catalogue/bases/{debian,ubuntu}.toml`
/// names, without this binary carrying a list of mirrors.
fn classify_one(distro: Distro, release: &Release) -> Origin {
    let origin = address_free(&release.origin);
    if origin.is_some_and(|o| o.eq_ignore_ascii_case(distro.name())) {
        return match address_free(&release.suite) {
            Some(suite) if NON_DEFAULT_SUITES.iter().any(|end| suite.ends_with(end)) => {
                Origin::Suite(suite.to_string())
            }
            _ => Origin::Base,
        };
    }
    Origin::ThirdParty(label(release))
}

/// How a third-party source is named in the emitted file and in its warning: its own label, and
/// the suite and component it served the package from. Never its address.
fn label(release: &Release) -> String {
    let name = address_free(&release.origin)
        .or_else(|| address_free(&release.label))
        .unwrap_or(UNLABELLED);
    match (
        address_free(&release.suite),
        address_free(&release.component),
    ) {
        (Some(suite), Some(component)) => format!("{name}, {suite}/{component}"),
        (Some(suite), None) => format!("{name}, {suite}"),
        (None, Some(component)) => format!("{name}, {component}"),
        (None, None) => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apt() -> Apt {
        let mut programs = BTreeMap::new();
        for name in PROGRAMS {
            programs.insert((*name).to_string(), PathBuf::from("/usr/bin").join(name));
        }
        Apt {
            distro: Distro::Debian,
            root: PathBuf::from("/"),
            programs,
            refused: BTreeMap::new(),
            operation: Operation::Apply,
            recommends: false,
        }
    }

    /// The same backend, pointed at a root that is not the machine.
    fn apt_rooted() -> Apt {
        Apt {
            root: PathBuf::from("/srv/elsewhere"),
            ..apt()
        }
    }

    /// A `Conffiles` path is resolved below the selected root: normalised lexically, and
    /// refused when its `..` would leave that root — before any file is looked at. The list is
    /// printed by a real program standing in for `dpkg-query`, run as the backend runs it.
    #[test]
    fn a_conffiles_path_that_leaves_the_root_is_refused() {
        let dir = Path::new(env!("OUT_DIR")).join("hostscope-apt-conffiles");
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("root");
        fs::create_dir_all(root.join("etc/sub")).unwrap();
        fs::write(root.join("etc/kept.conf"), "kept\n").unwrap();
        // Readable by *other*, whatever the umask: the only files the import reads.
        fs::set_permissions(
            root.join("etc/kept.conf"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        fs::write(dir.join("outside.conf"), "outside\n").unwrap();
        let listing = dir.join("conffiles.txt");
        let program = dir.join("dpkg-query");
        // It is started with a cleared environment and a fixed PATH (LD-357), so the PATH
        // its `cat` is found on is baked in.
        let path = std::env::var("PATH").unwrap_or_default();
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nPATH='{path}'; export PATH\ncat '{}'\n",
                listing.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let apt = Apt {
            root: root.clone(),
            programs: BTreeMap::from([("dpkg-query".to_string(), program)]),
            ..apt()
        };
        let digest = md5_hex(b"kept\n");
        fs::write(
            &listing,
            format!(" /etc/./kept.conf {digest}\n /etc/sub/../kept.conf {digest}\n"),
        )
        .unwrap();
        let found = apt.conffiles().unwrap();
        assert_eq!(found.len(), 1, "one file, under one normalised name");
        assert_eq!(found[0].path, "/etc/kept.conf");
        assert!(matches!(found[0].status, ConffileStatus::Unmodified));

        for escaping in [
            "/../outside.conf",
            "/etc/../../outside.conf",
            "/etc/../../../x",
        ] {
            fs::write(
                &listing,
                format!(" /etc/kept.conf {digest}\n {escaping} {digest}\n"),
            )
            .unwrap();
            let error = apt.conffiles().expect_err(escaping);
            assert_eq!(error.code, "E_PATH_ESCAPE", "{escaping}: {}", error.message);
            assert!(
                error.message.contains("does not stay inside that root"),
                "{}",
                error.message
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_transaction_carries_the_installs_and_the_removals() {
        let apt = apt();
        let invocations = apt
            .transaction(&["bc".to_string(), "ed".to_string()], &["zip".to_string()])
            .unwrap();
        assert_eq!(invocations.len(), 1, "one transaction, always");
        assert_eq!(
            invocations[0].args,
            [
                "install",
                "-y",
                "--no-install-recommends",
                "-o",
                "Dpkg::Options::=--force-confdef",
                "-o",
                "Dpkg::Options::=--force-confold",
                "--",
                "bc",
                "ed",
                "zip-",
            ]
        );
        assert!(
            invocations[0]
                .env
                .contains(&("APT_LISTCHANGES_FRONTEND".to_string(), "none".to_string()))
        );
        assert!(apt.transaction(&[], &[]).unwrap().is_empty());
    }

    #[test]
    fn the_backend_never_upgrades_autoremoves_or_configures() {
        let apt = apt();
        let mut every: Vec<String> = Vec::new();
        every.extend(apt.refresh().unwrap().iter().map(Invocation::command_line));
        every.extend(
            apt.transaction(&["bc".to_string()], &["ed".to_string()])
                .unwrap()
                .iter()
                .map(Invocation::command_line),
        );
        every.extend(
            apt.marks(&["bc".to_string()], &["ed".to_string()])
                .unwrap()
                .iter()
                .map(Invocation::command_line),
        );
        every.extend(
            apt.holds(&["bc".to_string()], &["ed".to_string()])
                .unwrap()
                .iter()
                .map(Invocation::command_line),
        );
        for line in &every {
            for forbidden in [
                "autoremove",
                "dist-upgrade",
                "upgrade",
                "--configure",
                "sudo",
            ] {
                assert!(!line.contains(forbidden), "{line} contains {forbidden}");
            }
        }
        assert_eq!(
            every,
            [
                "apt-get update",
                "apt-get install -y --no-install-recommends -o Dpkg::Options::=--force-confdef \
                 -o Dpkg::Options::=--force-confold -- bc ed-",
                "apt-mark auto -- bc",
                "apt-mark manual -- ed",
                "apt-mark hold -- bc",
                "apt-mark unhold -- ed",
            ]
        );
    }

    /// `--root DIR` puts the root options of LD-114 after the subcommand, and `/` puts none
    /// there — which is why the argv a guest sees is the one T-4 fixes.
    #[test]
    fn a_root_that_is_not_the_machine_carries_the_root_options() {
        let rooted = apt_rooted();
        assert_eq!(
            rooted.refresh().unwrap()[0].command_line(),
            "apt-get update -o RootDir=/srv/elsewhere"
        );
        assert_eq!(
            rooted.transaction(&["bc".to_string()], &[]).unwrap()[0].command_line(),
            "apt-get install -o RootDir=/srv/elsewhere -y --no-install-recommends \
             -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold -- bc"
        );
        assert_eq!(
            rooted.marks(&["bc".to_string()], &[]).unwrap()[0].command_line(),
            "apt-mark auto -o RootDir=/srv/elsewhere -- bc"
        );
        assert_eq!(
            rooted
                .invocation("dpkg-query", &["-W".to_string(), QUERY_FORMAT.to_string()])
                .unwrap()
                .args,
            ["-W", "--admindir=/srv/elsewhere/var/lib/dpkg", QUERY_FORMAT,]
        );
        assert_eq!(
            rooted
                .invocation("dpkg", &["--audit".to_string()])
                .unwrap()
                .command_line(),
            "dpkg --audit --admindir=/srv/elsewhere/var/lib/dpkg"
        );
        // The machine itself is asked plainly.
        let machine = apt();
        assert_eq!(
            machine.refresh().unwrap()[0].command_line(),
            "apt-get update"
        );
    }

    #[test]
    fn a_missing_program_is_a_runtime_error_that_names_it() {
        let apt = Apt {
            distro: Distro::Ubuntu,
            root: PathBuf::from("/"),
            programs: BTreeMap::new(),
            refused: BTreeMap::new(),
            operation: Operation::Apply,
            recommends: false,
        };
        let error = apt.refresh().unwrap_err();
        assert_eq!(error.code, "E_NO_RUNTIME");
        assert!(error.to_string().contains("apt-get"));
    }

    /// The hint of a missing program names the command that was run, whichever it is.
    #[test]
    fn a_missing_program_names_the_command_that_was_run() {
        for operation in [Operation::Plan, Operation::Apply, Operation::Import] {
            let apt = Apt {
                programs: BTreeMap::new(),
                operation,
                ..apt()
            };
            let text = apt.refresh().unwrap_err().to_string();
            assert!(
                text.contains(&format!("run `{}`", operation.command())),
                "{text}"
            );
            for other in [Operation::Plan, Operation::Apply, Operation::Import] {
                if other != operation {
                    assert!(!text.contains(other.command()), "{text}");
                }
            }
        }
    }

    /// `apt-get install -s`: the `Remv` lines are what goes, the "no longer required" block is
    /// the orphans, and a name qualified with another architecture is never offered as one.
    #[test]
    fn a_simulation_names_what_goes_and_what_it_orphans() {
        let text = "Reading package lists...\nBuilding dependency tree...\n\
                    Reading state information...\n\
                    The following packages were automatically installed and are no longer required:\n\
                    \x20 libfoo libbar libbaz:i386\n\
                    \x20 libqux\n\
                    Use 'apt autoremove' to remove them.\n\
                    The following packages will be REMOVED:\n  app tool\n\
                    0 upgraded, 0 newly installed, 2 to remove and 0 not upgraded.\n\
                    Remv app [1.0-1]\nRemv tool:amd64 [2.0-1]\nInst bc (1.07 Debian:12 [amd64])\n\
                    Inst libc6 [2.36-9] (2.36-9+deb12u1 Debian:12 [amd64])\n\
                    Inst libbc1:i386 (1.07 Debian:12 [i386])\n";
        let simulation = parse_simulation(text);
        // What it installs that is not installed: an upgrade names the version it replaces.
        assert_eq!(
            simulation.installed.iter().cloned().collect::<Vec<_>>(),
            ["bc", "libbc1"]
        );
        assert_eq!(
            simulation.removed.into_iter().collect::<Vec<_>>(),
            ["app", "tool"]
        );
        assert_eq!(
            simulation.orphans.into_iter().collect::<Vec<_>>(),
            ["libbar", "libfoo", "libqux"]
        );
        let bare =
            parse_simulation("0 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.\n");
        assert!(bare.removed.is_empty() && bare.orphans.is_empty());
    }

    /// `apt-cache show --no-all-versions`: each candidate record read for what the survey reads
    /// of an installed package (LD-412) — its priority, a `metapackages` section however
    /// qualified, and every name `Depends` and `Recommends` list — with a wrapped field and a
    /// record with no dependencies read as well.
    #[test]
    fn a_candidate_record_is_read_as_the_survey_reads_an_installed_one() {
        let text = "Package: ubuntu-server\nArchitecture: amd64\nVersion: 1.539\n\
                    Priority: optional\nSection: metapackages\n\
                    Depends: git, lvm2 (>= 2.03), mdadm | raidtools2\n\
                    Recommends: fwupd:any\nDescription: The Ubuntu Server system\n\
                    \x20This package depends on all of the packages in the Ubuntu Server system.\n\
                    \x20.\n\n\
                    Package: git\nVersion: 1:2.43.0-1\nPriority: optional\nSection: vcs\n\
                    Depends: libc6 (>= 2.34),\n liberror-perl\n\n\
                    Package: gpgv\nPriority: important\nSection: universe/metapackages\n";
        let survey = parse_show(text);
        assert_eq!(
            survey.depends["ubuntu-server"]
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            ["fwupd", "git", "lvm2", "mdadm", "raidtools2"]
        );
        assert_eq!(
            survey.depends["git"].iter().cloned().collect::<Vec<_>>(),
            ["libc6", "liberror-perl"]
        );
        assert!(survey.depends["gpgv"].is_empty());
        assert_eq!(
            survey.metapackages.iter().cloned().collect::<Vec<_>>(),
            ["gpgv", "ubuntu-server"]
        );
        assert_eq!(survey.priority["ubuntu-server"], Priority::Optional);
        assert!(survey.priority["gpgv"].is_baseline());
    }

    /// The simulation is the transaction's own argv, with `-s`, and never an autoremove.
    #[test]
    fn the_simulation_is_the_transaction_with_s() {
        let names = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        for recommends in [false, true] {
            let real = transaction_args(
                &names(&["bc"]),
                &names(&["zip"]),
                false,
                recommends,
                &Pinning::default(),
            );
            let simulated = transaction_args(
                &names(&["bc"]),
                &names(&["zip"]),
                true,
                recommends,
                &Pinning::default(),
            );
            assert_eq!(simulated[0], "install");
            assert_eq!(simulated[1], "-s");
            assert_eq!(&simulated[2..], &real[1..]);
            assert!(real.ends_with(&names(&["--", "bc", "zip-"])));
            assert!(!simulated.iter().any(|a| a.contains("autoremove")));
            assert_eq!(
                real.iter().any(|a| a == "--no-install-recommends"),
                !recommends,
                "{real:?}"
            );
        }
    }

    #[test]
    fn policy_output_gives_the_candidate_and_omits_what_has_none() {
        let text = "bc:\n  Installed: (none)\n  Candidate: 1.07.1-3+b1\n  Version table:\n     \
                    1.07.1-3+b1 500\n        500 http://deb.debian.org/debian bookworm/main \
                    amd64 Packages\nnope:\n  Installed: (none)\n  Candidate: (none)\n  Version \
                    table:\n";
        let parsed = parse_policy(text);
        assert_eq!(parsed.get("bc").map(String::as_str), Some("1.07.1-3+b1"));
        assert!(!parsed.contains_key("nope"));
    }

    #[test]
    fn an_index_that_is_not_there_has_no_age() {
        assert!(apt().index_age().is_none());
    }

    /// A lists directory in a scratch root, removed when the test ends, pass or fail.
    struct Lists(PathBuf);

    impl Lists {
        fn new(tag: &str) -> Lists {
            let root =
                std::env::temp_dir().join(format!("lodi-unit-apt-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join(LISTS)).unwrap();
            Lists(root)
        }

        /// A list file as apt leaves it: written now, its mtime set to the archive's own
        /// `Last-Modified`, `hours` ago.
        fn fetched(&self, name: &str, hours: u64) {
            let path = self.0.join(LISTS).join(name);
            fs::write(&path, "Origin: example\n").unwrap();
            let archive = SystemTime::now() - Duration::from_secs(hours * 3600);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(archive)
                .unwrap();
        }
    }

    impl Drop for Lists {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// apt stamps each list file with the archive's `Last-Modified`, so an index that
    /// `apt-get update` fetched a moment ago can carry an mtime hours old — and on a machine
    /// whose archive has not published for six hours, reading the mtime would plan a refresh
    /// after every refresh, and the machine would never be `nothing to do`.
    #[test]
    fn an_index_apt_just_fetched_is_fresh_whatever_date_the_archive_gave_it() {
        let lists = Lists::new("fetched");
        lists.fetched("deb.example_dists_stable_InRelease", 9);
        let apt = Apt {
            root: lists.0.clone(),
            ..apt()
        };
        let age = apt.index_age().expect("an index");
        assert!(age < super::super::REFRESH_AFTER, "{age:?}");
    }

    /// The window still closes: six hours after apt last touched its lists, whatever the
    /// archive's date, the index is old enough to refresh, and five hours after it is not.
    #[test]
    fn an_index_is_old_six_hours_after_apt_last_touched_it() {
        let lists = Lists::new("window");
        lists.fetched("deb.example_dists_stable_InRelease", 30);
        lists.fetched("deb.example_dists_stable_main_binary-amd64_Packages", 9);
        let dir = lists.0.join(LISTS);
        let hours = |n: u64| SystemTime::now() + Duration::from_secs(n * 3600);
        let window = super::super::REFRESH_AFTER;
        assert!(age_at(&dir, hours(5)).expect("an index") < window);
        assert!(age_at(&dir, hours(6)).expect("an index") >= window);
        assert!(age_at(&dir, hours(7)).expect("an index") >= window);
    }

    /// A held package is installed. Reading dpkg's three-character status as the word `ii`
    /// misses it, because a hold changes the *wanted* state to `h` and leaves the current state
    /// alone — and this backend puts packages on hold itself, so the mistake would make a
    /// second apply forget every held package it had just recorded.
    #[test]
    fn a_held_package_is_installed_and_a_half_unpacked_one_is_not() {
        for abbrev in ["ii ", "ii", "hi ", "hi", "ui "] {
            assert!(is_installed(abbrev), "{abbrev:?} is installed");
        }
        for abbrev in [
            "iU ", "iF ", "iH ", "un ", "rc ", "pn ", "iR", "iiR", "i", "",
        ] {
            assert!(!is_installed(abbrev), "{abbrev:?} is not installed");
        }
    }
}
