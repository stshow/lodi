//! `[services]` of `home.toml` (hc-1, #494): the user's own systemd units, enabled and started or
//! disabled and stopped through `systemctl --user` as the home's user, lingering only on request,
//! removal restoring only the units Lodi managed, and an inline unit only when one is written.
//!
//! The built binary runs in the throwaway roots of `support::home_env`, with test-owned
//! `systemctl` and `loginctl` first on `PATH` and a scratch `XDG_RUNTIME_DIR`: they keep the
//! units' states in files beside them and log every call with the uid that made it, so no test
//! here ever reaches a real user manager or logind. Offline and deterministic.

mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Output;

use support::{HomeEnv, home_env};

/// The fake `systemctl`: shell builtins only, since the fixed `PATH` it runs with may hold no
/// other tool, and `@ID@`, the absolute `id` the test found, for the uid of every call.
const SYSTEMCTL: &str = r#"#!/bin/sh
sd="${0%/bin/systemctl}/sd"
echo "uid=$(@ID@ -u) runtime=${XDG_RUNTIME_DIR-} bus=${DBUS_SESSION_BUS_ADDRESS-} systemctl $*" \
  >> "$sd/calls"
[ "$1" = --user ] || { echo "not a user manager call" >&2; exit 64; }
shift
verb=$1
shift
unit=
for a in "$@"; do case $a in --now|--) ;; *) unit=$a ;; esac; done
read -r home < "$sd/home"
known() { [ -e "$sd/known/$1" ] || [ -e "$home/.config/systemd/user/$1" ]; }
s=disabled
[ -e "$sd/state/$unit" ] && read -r s < "$sd/state/$unit"
a=0
[ -e "$sd/active/$unit" ] && read -r a < "$sd/active/$unit"
case $verb in
  daemon-reload) ;;
  is-enabled)
    known "$unit" || { echo "Failed to get unit file state for $unit" >&2; exit 1; }
    echo "$s"
    [ "$s" = enabled ] ;;
  is-active) if [ "$a" = 1 ]; then echo active; else echo inactive; exit 3; fi ;;
  enable) known "$unit" || exit 1; echo enabled > "$sd/state/$unit"; echo 1 > "$sd/active/$unit" ;;
  disable) known "$unit" || exit 1; echo disabled > "$sd/state/$unit"; echo 0 > "$sd/active/$unit" ;;
  *) echo "unexpected $verb" >&2; exit 64 ;;
esac
"#;

const LOGINCTL: &str = r#"#!/bin/sh
sd="${0%/bin/loginctl}/sd"
echo "uid=$(@ID@ -u) loginctl $*" >> "$sd/calls"
l=no
[ -e "$sd/linger" ] && read -r l < "$sd/linger"
case $1 in
  show-user) echo "$l" ;;
  enable-linger) echo yes > "$sd/linger" ;;
  disable-linger) echo no > "$sd/linger" ;;
  *) exit 64 ;;
esac
"#;

/// The absolute path of `id` on this test's own `PATH`.
fn id_program() -> String {
    let path = std::env::var_os("PATH").expect("the test runs with a PATH");
    std::env::split_paths(&path)
        .map(|dir| dir.join("id"))
        .find(|candidate| candidate.is_file())
        .expect("`id` is on the test's PATH")
        .display()
        .to_string()
}

/// A home with a fake user manager beside it.
struct Case {
    env: HomeEnv,
    bin: PathBuf,
    sd: PathBuf,
    run: PathBuf,
}

impl Case {
    fn new(name: &str) -> Case {
        let env = home_env(name);
        let bin = env.root().join("bin");
        let sd = env.root().join("sd");
        let run = env.root().join("run");
        for dir in [
            &bin,
            &run,
            &sd.join("known"),
            &sd.join("state"),
            &sd.join("active"),
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        // The user manager's own socket, and no session bus: Debian 12 ships none by default.
        fs::create_dir_all(run.join("systemd")).unwrap();
        fs::write(run.join("systemd/private"), "").unwrap();
        fs::write(sd.join("home"), env.home().display().to_string()).unwrap();
        fs::write(sd.join("calls"), "").unwrap();
        for (name, body) in [("systemctl", SYSTEMCTL), ("loginctl", LOGINCTL)] {
            let path = bin.join(name);
            fs::write(&path, body.replace("@ID@", &id_program())).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Case { env, bin, sd, run }
    }

    /// A unit the user manager has, in `state`, running or not.
    fn unit(&self, unit: &str, state: &str, active: bool) {
        fs::write(self.sd.join("known").join(unit), "").unwrap();
        fs::write(self.sd.join("state").join(unit), format!("{state}\n")).unwrap();
        fs::write(
            self.sd.join("active").join(unit),
            if active { "1\n" } else { "0\n" },
        )
        .unwrap();
    }

    /// The unit's state and whether it runs, as the fake manager holds them.
    fn state(&self, unit: &str) -> (String, bool) {
        let state = fs::read_to_string(self.sd.join("state").join(unit))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "disabled".into());
        let active =
            fs::read_to_string(self.sd.join("active").join(unit)).is_ok_and(|a| a.trim() == "1");
        (state, active)
    }

    fn manifest(&self, text: &str) {
        let dir = self.env.config().join("lodi");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("home.toml"), text).unwrap();
    }

    fn lodi(&self, args: &[&str]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.bin.display());
        self.env
            .command()
            .env("PATH", path)
            .env("XDG_RUNTIME_DIR", &self.run)
            .args(args)
            .output()
            .expect("the lodi binary runs")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.lodi(args);
        assert!(
            out.status.success(),
            "lodi {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.sd.join("calls"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The calls that change something, from the `n`th on.
    fn changes_since(&self, n: usize) -> Vec<String> {
        self.calls()[n..]
            .iter()
            .filter(|call| {
                ["enable", "disable", "daemon-reload"]
                    .iter()
                    .any(|verb| call.contains(&format!(" {verb}")))
            })
            .cloned()
            .collect()
    }

    fn uid(&self) -> u32 {
        fs::metadata(self.env.home()).unwrap().uid()
    }

    fn lingering(&self) -> bool {
        fs::read_to_string(self.sd.join("linger")).is_ok_and(|l| l.trim() == "yes")
    }
}

const TWO: &str = r#"
[services."a.service"]
enable = true

[services."b.service"]
enable = false
"#;

#[test]
fn service_runs_as_user() {
    let case = Case::new("services-as-user");
    case.unit("a.service", "disabled", false);
    case.unit("b.service", "enabled", true);
    case.manifest(TWO);

    let plan = case.ok(&["home", "plan"]);
    assert!(
        plan.contains("+ service a.service (systemctl --user enable --now -- a.service)"),
        "{plan}"
    );
    assert!(
        plan.contains("- service b.service (systemctl --user disable --now -- b.service)"),
        "{plan}"
    );
    assert!(case.changes_since(0).is_empty(), "plan changed nothing");
    assert_eq!(case.state("a.service"), ("disabled".into(), false));

    case.ok(&["home", "apply"]);
    assert_eq!(case.state("a.service"), ("enabled".into(), true));
    assert_eq!(case.state("b.service"), ("disabled".into(), false));
    let who = format!(
        "uid={} runtime={} bus= systemctl --user ",
        case.uid(),
        case.run.display()
    );
    let calls = case.calls();
    assert!(!calls.is_empty());
    for call in &calls {
        assert!(call.starts_with(&who), "not as the user: {call}");
    }

    let before = case.calls().len();
    assert_eq!(case.ok(&["home", "apply"]), "nothing to do\n");
    assert!(case.changes_since(before).is_empty(), "{:?}", case.calls());
}

#[test]
fn linger_only_when_opted_in() {
    let case = Case::new("services-linger");
    case.unit("a.service", "disabled", false);
    case.manifest("[services.\"a.service\"]\nenable = true\n");
    case.ok(&["home", "apply"]);
    assert!(!case.lingering());
    assert!(!case.calls().iter().any(|call| call.contains(" loginctl ")));

    case.manifest("[services.\"a.service\"]\nenable = true\nlinger = true\n");
    let plan = case.ok(&["home", "plan"]);
    let enable = format!("+ linger (loginctl enable-linger {})", case.uid());
    assert!(plan.contains(&enable), "{plan}");
    assert!(!case.lingering(), "plan changed nothing");
    case.ok(&["home", "apply"]);
    assert!(case.lingering());
    assert_eq!(case.ok(&["home", "apply"]), "nothing to do\n");

    // Lodi turned it on, so taking the request out turns it off.
    case.manifest("[services.\"a.service\"]\nenable = true\n");
    case.ok(&["home", "apply"]);
    assert!(!case.lingering());

    // Lingering the user turned on is theirs: never turned off.
    fs::write(case.sd.join("linger"), "yes\n").unwrap();
    case.ok(&["home", "apply"]);
    assert!(case.lingering());
}

#[test]
fn removal_restores_only_managed_units() {
    let case = Case::new("services-removal");
    case.unit("a.service", "disabled", false);
    case.unit("c.service", "enabled", true);
    case.manifest("[services.\"a.service\"]\nenable = true\n");
    case.ok(&["home", "apply"]);
    assert_eq!(case.state("a.service"), ("enabled".into(), true));

    case.manifest("[home]\nversion = \"1\"\n");
    let before = case.calls().len();
    let out = case.ok(&["home", "apply"]);
    assert!(
        out.contains("~ service a.service (systemctl --user disable --now -- a.service)"),
        "{out}"
    );
    assert_eq!(case.state("a.service"), ("disabled".into(), false));
    assert_eq!(case.state("c.service"), ("enabled".into(), true));
    assert!(
        !case.calls().iter().any(|call| call.contains("c.service")),
        "a unit lodi never managed is never read or touched"
    );
    assert_eq!(case.changes_since(before).len(), 1);
    assert_eq!(case.ok(&["home", "apply"]), "nothing to do\n");
}

const UNIT: &str = "[Unit]\nDescription=hello\n\n[Service]\nExecStart=/bin/true\n\n\
                    [Install]\nWantedBy=default.target\n";

#[test]
fn inline_unit_only_when_opted_in() {
    let case = Case::new("services-inline");
    case.unit("a.service", "disabled", false);
    let units = case.env.home().join(".config/systemd/user");
    case.manifest(&format!(
        "[services.\"a.service\"]\nenable = true\n\n\
         [services.\"hello.service\"]\nenable = true\nunit = '''\n{UNIT}'''\n"
    ));
    case.ok(&["home", "apply"]);
    assert_eq!(
        fs::read_to_string(units.join("hello.service")).unwrap(),
        UNIT
    );
    assert!(
        !units.join("a.service").exists(),
        "no unit is written unless one is declared"
    );
    assert_eq!(case.state("hello.service"), ("enabled".into(), true));
    assert!(
        case.calls()
            .iter()
            .any(|call| call.ends_with("daemon-reload"))
    );

    // Taken out, the inline unit goes back to not being there at all.
    case.manifest("[services.\"a.service\"]\nenable = true\n");
    case.ok(&["home", "apply"]);
    assert!(!units.join("hello.service").exists());
    assert_eq!(case.state("hello.service"), ("disabled".into(), false));

    // The import never copies a unit file, and never replaces what the user wrote.
    fs::write(units.join("mine.service"), UNIT).unwrap();
    let emitted = case.ok(&["home", "import", "--stdout"]);
    assert!(
        !emitted.contains("[services") && !emitted.contains("ExecStart"),
        "{emitted}"
    );
    let listing: Vec<_> = fs::read_dir(&units).unwrap().flatten().collect();
    assert_eq!(listing.len(), 1, "{listing:?}");
    let manifest = case.env.config().join("lodi/home.toml");
    let written = fs::read(&manifest).unwrap();
    assert!(!case.lodi(&["home", "import"]).status.success());
    assert_eq!(fs::read(&manifest).unwrap(), written);
}
