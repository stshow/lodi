//! The package-manager **family** seam (M-Arch-Base T-1, design call D1).
//!
//! M-0.3 T-4 parameterized the container path by *distribution*: the distro, the release and its
//! aliases, the repositories, their suites and components and the rootfs strategy all come from
//! the base definition (LD-74). That is exactly one family deep, and it is the right seam for a
//! fourth apt base and the wrong seam for a first non-apt one: the index format, the resolver,
//! the in-image configuration and the closure read-back all change together and none of them is
//! expressible as a different value in an apt-shaped definition.
//!
//! So "which package manager" is asked **once**, here, and answered by one implementation of
//! [`FamilyOps`]. The five operations are the ones the container path actually needs:
//!
//! 1. [`FamilyOps::repositories`] — materialize the repositories of a release at a snapshot;
//! 2. [`FamilyOps::fetch_index`] — fetch and verify one repository's index, returning the locked
//!    index records and the parsed packages;
//! 3. [`FamilyOps::resolve`] — resolve a requested set against those packages and the rootfs's
//!    own package list, to a closure;
//! 4. [`ImageOps::build_files`] — render the in-image package configuration and the install
//!    command, as a pure function of the [`BaseEntry`];
//! 5. [`ImageOps::closure_query_arguments`], [`ImageOps::recorded_base_arguments`] and
//!    [`ImageOps::closure_differences`] — name the read-back commands and parse the installed
//!    closure back out of them.
//!
//! [`apt`] is one implementation; it is the code M-0.3 T-4 landed, moved here unchanged, and the
//! digests `tests/distro_seam.rs` pins are the proof that it is unchanged. The apt index,
//! control, version and resolver modules keep their `src/debian/` home and their name (LD-74);
//! they are reached through this seam. `crate::arch::base::Pacman` is the other implementation,
//! joined to the seam by the convergence package (M-Arch-Base T-6); both families now answer the
//! whole of [`FamilyOps`], so [`Family::ops`] refuses neither.
//!
//! Operations 4 and 5 are also the **whole** of what an image build asks, and they were the half
//! the second family acquired first (M-Arch-Base T-5), so they stay their own trait,
//! [`ImageOps`]: `src/container.rs` asks for nothing else, and a build needs no lock-time code.
//!
//! What the seam carries between operations 2 and 3 is each family's **own** index record type,
//! in [`IndexPackages`]: an apt `Packages` stanza and a pacman `desc` stanza have neither the
//! same fields nor the same version semantics, and neither is expressible as the other. The type
//! is a struct with one field per family rather than an enum, so the family that fills a field
//! is the only one that has to read it and a mixture is not representable as an error state: a
//! base definition names exactly one family, so exactly one field is ever non-empty.

pub mod apt;
pub mod dnf;
pub mod zstd;

use std::collections::BTreeMap;

use crate::arch::db::Package as PacmanPackage;
use crate::catalogue::{BaseDefinition, ReleaseDefinition, RepositoryDefinition};
use crate::debian::index::BinaryPackage;
use crate::debian::resolve::ClosurePackage;
use crate::debian::version::DebVersion;
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::lock::{BaseEntry, ClosureEntry};

pub use crate::debian::base::LockedIndex;

/// A materialized repository of one release at one snapshot: what the lock records, before its
/// indexes are fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub name: String,
    pub url: String,
    pub suites: Vec<String>,
    pub components: Vec<String>,
}

/// The packages the indexes of one base carry, accumulated across its repositories.
///
/// One field per family, each that family's own record type. A base definition names exactly one
/// family (design call D1), so exactly one field is ever non-empty; each implementation of
/// [`FamilyOps`] fills and reads its own field and never looks at the other.
#[derive(Debug, Default)]
pub struct IndexPackages {
    /// What an apt `Packages` index carries.
    pub apt: Vec<BinaryPackage>,
    /// What a pacman `<repo>.db` carries.
    pub pacman: Vec<PacmanPackage>,
    /// What a dnf repository's `primary` index carries.
    pub rpm: Vec<dnf::RpmPackage>,
    /// The builds the dnf family's base image holds, `NAME-EPOCH:VERSION-RELEASE.ARCH`, as its
    /// base definition pins them; they are resolved against `rpm` like any other.
    pub rpm_base: Vec<String>,
}

/// What an image's read-back reported, as the family's own commands printed it.
#[derive(Debug, Clone, Copy)]
pub struct ReadBack<'a> {
    /// The output of [`ImageOps::closure_query_arguments`]: every package installed in the image.
    pub installed: &'a str,
    /// The output of [`ImageOps::recorded_base_arguments`] — what the base rootfs brought, as
    /// the build recorded it inside the image before installing anything. `None` when the family
    /// asks for no such query, and also when the query failed; a family that asks for one must
    /// treat its absence as a difference rather than assume an empty base.
    pub recorded_base: Option<&'a str>,
}

/// The package-manager families this project knows about. The set is closed at compile time and
/// is three; all are implemented in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Apt,
    Pacman,
    Dnf,
}

impl Family {
    /// The family's name, as a base definition spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Apt => "apt",
            Family::Pacman => "pacman",
            Family::Dnf => "dnf",
        }
    }

    /// The package architecture in this family's vocabulary. This stays available before every
    /// family has all five runtime operations, so an unreachable Arch lock can still be checked.
    pub fn package_arch(self, arch: &str) -> Option<&'static str> {
        match (self, arch) {
            (Family::Apt, "x86_64") => Some("amd64"),
            (Family::Pacman, "x86_64") => Some("x86_64"),
            (Family::Dnf, "x86_64") => Some("x86_64"),
            _ => None,
        }
    }

    /// The family a base definition spells out, if the name is one this project knows.
    pub fn parse(name: &str) -> Option<Family> {
        match name {
            "apt" => Some(Family::Apt),
            "pacman" => Some(Family::Pacman),
            "dnf" => Some(Family::Dnf),
            _ => None,
        }
    }

    /// The family a base definition that names none implies, read from the shape of its
    /// repositories: an apt repository is addressed by suite and component, a pacman repository
    /// by neither. A definition whose repositories do not agree, or that has none, implies no
    /// family and is a recipe error — there is no default (M-Arch-Base T-1, LD record).
    pub fn derive(repositories: &BTreeMap<String, RepositoryDefinition>) -> Option<Family> {
        if repositories.is_empty() {
            return None;
        }
        let addressed = |r: &RepositoryDefinition| !r.suites.is_empty() && !r.components.is_empty();
        let bare = |r: &RepositoryDefinition| r.suites.is_empty() && r.components.is_empty();
        if repositories.values().all(addressed) {
            Some(Family::Apt)
        } else if repositories.values().all(bare) {
            Some(Family::Pacman)
        } else {
            None
        }
    }

    /// The operations of this family. The result stays a `Result` because the family set is a
    /// compile-time property of the build and a definition may name a family a smaller build
    /// does not carry; this build carries both.
    pub fn ops(self) -> Result<&'static dyn FamilyOps, Diagnostic> {
        match self {
            Family::Apt => Ok(&apt::Apt),
            Family::Pacman => Ok(&crate::arch::base::Pacman),
            Family::Dnf => Ok(&dnf::Dnf),
        }
    }

    /// The image-build operations of this family, which every family has and an image build
    /// needs on their own: realization reads a lock and never resolves anything.
    pub fn image_ops(self) -> &'static dyn ImageOps {
        match self {
            Family::Apt => &apt::Apt,
            Family::Pacman => &crate::arch::base::Pacman,
            Family::Dnf => &dnf::Dnf,
        }
    }
}

/// What an **image build** asks of a package manager, and nothing else: operations 4 and 5.
/// Everything here is a pure function of the lock, so the same lock gives the same image
/// identity on every machine.
pub trait ImageOps: Sync {
    /// **4.** The files the image build generates, by name in the build context: the in-image
    /// package configuration and the `Containerfile` whose install command installs exactly the
    /// verified package files under [`ImageOps::package_mount`]. A pure function of the lock.
    fn build_files(&self, base: &BaseEntry, has_packages: bool) -> BTreeMap<&'static str, String>;

    /// **5.** The `podman run` arguments that read the installed packages of image `tag` back.
    fn closure_query_arguments<'a>(&self, tag: &'a str) -> Vec<&'a str>;

    /// **5.** The `podman run` arguments that read back what the base rootfs brought, for a
    /// family whose rootfs publishes no package list and whose generated build therefore records
    /// its own installed set inside the image before installing anything (LD record of
    /// M-Arch-Base T-6). `None` — the default — when the lock already names every package the
    /// image must hold, which is the case whenever the base definition publishes a package list.
    fn recorded_base_arguments<'a>(&self, _tag: &'a str) -> Option<Vec<&'a str>> {
        None
    }

    /// **5.** The differences between what the read-back reported and what the lock says the
    /// image must hold, one line each; empty when they agree.
    fn closure_differences(&self, read_back: &ReadBack, closure: &[ClosureEntry]) -> Vec<String>;

    /// Where the verified package files are mounted inside the build.
    fn package_mount(&self) -> &'static str;

    /// The name a verified package file is staged under in that mount. Only the family reads
    /// it back, so the name only has to be unique and to carry the suffix the family expects.
    fn staged_package_name(&self, name: &str, version: &str) -> String;
}

/// Everything the container path asks of a package manager, and nothing else.
pub trait FamilyOps: ImageOps {
    /// The package architecture in the distribution's own vocabulary, for a Lodi architecture
    /// (`x86_64`); `None` when this family has no name for it. This is what the lock's
    /// `base.debArch` carries (design call D11).
    fn package_arch(&self, arch: &str) -> Option<&'static str>;

    /// **1.** The repositories of `release` at `snapshot`, with every template rendered.
    /// `package_arch` is this family's name for the request's architecture, which a repository
    /// URL may be addressed by.
    fn repositories(
        &self,
        def: &BaseDefinition,
        release: &ReleaseDefinition,
        snapshot: i64,
        package_arch: &str,
    ) -> Result<Vec<Repository>, Diagnostic>;

    /// **2.** Fetch and verify `repository`'s index: the locked index records it is pinned by,
    /// with the packages it carries appended to this family's field of `into`. Nothing but
    /// metadata is fetched.
    fn fetch_index(
        &self,
        fetcher: &dyn Fetcher,
        repository: &Repository,
        package_arch: &str,
        into: &mut IndexPackages,
    ) -> Result<Vec<LockedIndex>, Diagnostic>;

    /// **3.** Resolve `requested` against the pinned `index` and the rootfs's own `installed`
    /// package list, to the complete closure.
    ///
    /// `installed` is the `name<TAB>version` list a base definition may publish beside its
    /// rootfs. It is parsed with apt version semantics because that is the only form anyone
    /// publishes; a family with no such list is handed an empty one and refuses a non-empty one
    /// rather than reading it in the wrong vocabulary.
    fn resolve(
        &self,
        index: &IndexPackages,
        installed: &[(String, DebVersion)],
        requested: &[String],
        distro: &str,
    ) -> Result<Vec<ClosurePackage>, Diagnostic>;
}
