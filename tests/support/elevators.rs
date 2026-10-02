//! Stub elevators for every test that may elevate (#709, #715): recording `sudo`, `doas` and
//! `run0` stubs in one folder, and the `PATH` such a test runs lodi with, which holds that folder,
//! the folders the test names, and a folder of links to the tools lodi runs (`git`), never a
//! folder of the machine's own. So no stub left out or misnamed reaches a real elevator; and
//! should one be reached anyway, lodi refuses an elevator root owns under
//! `LODI_HOST_REQUIRE_ROOT=1` (LD-513).

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The elevators lodi knows, in its order.
pub const ELEVATORS: [&str; 3] = ["sudo", "doas", "run0"];

/// The tools a lodi under test runs by name: linked into the tools folder from the test's own
/// `PATH`, so that `PATH` holds no folder of the machine's own.
const TOOLS: [&str; 1] = ["git"];

/// The stubs in `<at>/elevators`, their record in `<at>/elevator-calls`, the tools in
/// `<at>/tools`.
pub struct Elevators {
    dir: PathBuf,
    record: PathBuf,
    tools: PathBuf,
}

impl Elevators {
    /// All three stubs, each running the rest of its command line.
    pub fn new(at: &Path) -> Elevators {
        let elevators = Elevators {
            dir: at.join("elevators"),
            record: at.join("elevator-calls"),
            tools: at.join("tools"),
        };
        fs::create_dir_all(&elevators.tools).unwrap();
        for tool in TOOLS {
            if let Some(found) = which(tool) {
                let _ = std::os::unix::fs::symlink(found, elevators.tools.join(tool));
            }
        }
        elevators.install(&ELEVATORS, false, "");
        elevators
    }

    /// Stubs named `names`, replacing those there. Each appends its name and arguments to the
    /// record, runs `before` (a shell line, if any), then, as an elevator with a secure path
    /// does, runs the rest unprivileged with an empty environment but `PATH`; a `refuse`d one
    /// exits 1 at once, as a refused password does.
    pub fn install(&self, names: &[&str], refuse: bool, before: &str) {
        let env = which("env").expect("env");
        let run = if refuse {
            "exit 1".to_string()
        } else {
            format!(
                "{before}\n[ \"$1\" = -- ] && shift\nexec '{}' -i PATH=\"$PATH\" \"$@\"",
                env.display()
            )
        };
        self.write(names, &run);
    }

    /// A stub `sudo` whose child reads what only root may read: the same uid in a user namespace
    /// of its own, keeping that namespace's capabilities (so a 0000 file of the person's is
    /// readable to it, and to nothing the person runs by hand).
    pub fn reading(&self, uid: u32, gid: u32) {
        let (env, unshare) = (
            which("env").expect("env"),
            which("unshare").expect("unshare"),
        );
        let run = format!(
            "[ \"$1\" = -- ] && shift\nexec '{}' --user --map-user={uid} --map-group={gid} \
             --keep-caps \\\n  '{}' -i PATH=\"$PATH\" \"$@\"",
            unshare.display(),
            env.display(),
        );
        self.write(&["sudo"], &run);
    }

    fn write(&self, names: &[&str], run: &str) {
        let _writing = crate::fakehost::writing();
        let _ = fs::remove_dir_all(&self.dir);
        fs::create_dir_all(&self.dir).unwrap();
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o755)).unwrap();
        for name in names {
            assert!(ELEVATORS.contains(name), "{name} is no elevator");
            let body = format!(
                "#!/bin/sh\nprintf '%s %s\\n' {name} \"$*\" >> '{}'\n{run}\n",
                self.record.display()
            );
            fs::write(self.dir.join(name), body).unwrap();
            fs::set_permissions(self.dir.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Every call, one line each: the elevator's name, then its arguments.
    pub fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Forget the calls so far.
    pub fn forget(&self) {
        let _ = fs::remove_file(&self.record);
    }

    /// The record of the calls, which a run that elevates leaves behind.
    pub fn record(&self) -> &Path {
        &self.record
    }

    /// The `PATH` a lodi under test runs with: the stubs, `dirs`, then the tools. A folder in
    /// `dirs` holding an elevator is a broken test, never a run.
    pub fn path(&self, dirs: &[&Path]) -> String {
        let mut all = vec![self.dir.as_path()];
        for dir in dirs {
            for name in ELEVATORS {
                let at = dir.join(name);
                assert!(
                    fs::symlink_metadata(&at).is_err(),
                    "{} on PATH",
                    at.display()
                );
            }
            all.push(dir);
        }
        all.push(&self.tools);
        std::env::join_paths(all).unwrap().into_string().unwrap()
    }
}

/// `name` on the test's own `PATH`, for the stubs and the tools folder only.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|file| file.is_file())
    })
}
