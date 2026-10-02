//! Lodi: an environment and system manager for Ubuntu, Debian, Arch and Fedora.
//!
//! The `lodi` binary (`src/main.rs`) exposes `lodi import`, `switch`, `update`, `pin` and
//! `unpin` for a machine and its home, `lodi init`, `develop`, `run`, `shell`, `search` and `gc`
//! for projects and tools, and `--version` and `--help`. The modules:
//!
//! - [`config`]: the config a host and home command reads, and `import`, `switch`, `update`,
//!   `pin` and `unpin` over it;
//! - [`elevate`]: the elevator helper that runs one request as root for a lodi started as the
//!   user;
//! - [`marker`]: the "may manage" marker `lodi import` writes and `lodi switch` reads;
//! - [`progress`]: the step list `import`, `switch` and `update` show while they work;
//! - [`manifest`]: the strict project-manifest subset of design `spec/01` (S-1);
//! - [`version`]: upstream-tool version constraints, design `spec/04` §4;
//! - [`diag`]: diagnostics in the design `spec/11` format and the exit statuses;
//! - [`init`]: `lodi init`, the starter manifest and the `.gitignore` line (M-0.3 T-1);
//! - [`lock`]: a project's `lodi.lock`, its staleness rules and frozen use (S-2);
//! - [`debian`]: Debian versions, indexes, the dependency closure and the pinned base (S-2);
//! - [`arch`]: bounded Arch repository parsing and package-version ordering (M-Arch-Base T-2);
//! - [`distro`]: the package-manager family seam, asked once (M-Arch-Base T-1);
//! - [`upstream`]: upstream tool discovery through [`catalogue`] recipes (S-2);
//! - [`store`]: verified downloads, the per-user store and live-session roots (S-3);
//! - [`archive`]: bounded, checked tar extraction; [`nar`]: the NAR-compatible tree hash (S-3);
//! - [`tools`]: the `[tools]` contract of `spec/01` §3.8, scope-independent (M-0.4 T-2);
//! - [`trust`]: the trust gate for manifest task text (S-3);
//! - [`roots`]: the one reader of `HOME`, `XDG_*` and `LODI_HOME` (M-0.5 T-1);
//! - [`schema`]: the registry of every on-disk artifact, its read-set and its writer field
//!   (M-1.0 T-1);
//! - [`layout`]: the store's layout marker and its once-only migration (M-1.0 T-2);
//! - [`home`]: the home scope — the one module that writes inside a root (M-0.5 T-1), the
//!   `home.toml` loader and the home plan (M-0.5 T-2);
//! - [`host`]: host-tool environments, `lodi develop` and `lodi run` (S-3);
//! - [`hostscope`]: the distribution's packages and root-owned files, as `lodi switch` applies
//!   them (M-0.6 T-3);
//! - [`passwd`]: the selected root's own `etc/passwd` and `etc/group`;
//! - [`shell`]: `lodi shell`, an ad-hoc environment resolved from its arguments (M-0.3 T-2);
//! - [`gc`]: `lodi gc`, the store collector and its image report (M-0.3 T-3);
//! - [`search`]: `lodi search`, offline browsing of the catalogue and of the project's own
//!   indexes (M-0.4 T-6);
//! - [`surface`]: the deprecation table of the published CLI surface and its resolver, the
//!   mechanism behind `docs/CLI.md`'s promise (M-1.0 T-8);
//! - [`container`]: container environments through rootless Podman (S-4);
//! - [`git`]: a verified single-commit smart-HTTP reader;
//! - [`fetch`]: HTTPS fetching; [`util`]: digests and timestamps.

pub mod arch;
pub mod archive;
pub mod catalogue;
pub mod config;
pub mod container;
pub mod debian;
pub mod diag;
pub mod distro;
pub mod elevate;
pub mod fetch;
pub mod gc;
pub mod git;
pub mod home;
pub mod host;
pub mod hostscope;
pub mod init;
pub mod layout;
pub mod lock;
pub mod manifest;
pub mod marker;
pub mod nar;
pub mod passwd;
pub mod progress;
pub mod roots;
pub mod schema;
pub mod search;
pub mod shell;
pub mod store;
pub mod surface;
pub mod tools;
pub mod trust;
pub mod upstream;
pub mod util;
pub mod version;
