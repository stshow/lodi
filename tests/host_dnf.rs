//! LD-432: the host scope on Fedora 44, with dnf5 and rpm behind the one backend seam.
//!
//! Every case is the real binary against a scratch `--root` with a **fake machine** behind it
//! (`tests/support/fakehost.rs`, `tests/support/fakepm.py`), whose dnf5 and rpm answer in the
//! layouts a Fedora 44 guest printed (`tests/fixtures/host/dnf/`). Nothing reaches `/` and no
//! real package manager runs (`AGENTS.md` §8). The label case runs the binary in a user and
//! mount namespace of its own, on a tmpfs where an unprivileged test may set a
//! `security.selinux` attribute, because this machine has no SELinux to label with.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::process::{Command, Stdio};

use fakehost::{Case, Machine, Pkg, err, nothing, out, story};

/// A package at a Fedora 44 build.
fn fc44(name: &str, version: &str) -> Pkg {
    let mut pkg = Pkg::new(name);
    pkg.version = format!("0:{version}.fc44");
    pkg
}

/// A Fedora 44 machine a person chose `htop` on, which pulled `hwloc-libs` in; `tree` and the
/// weak dependency it recommends are offered and not installed.
fn chosen() -> Machine {
    Machine::fedora()
        .with(fc44("htop", "3.4.1-3").depends(&["hwloc-libs", "glibc"]))
        .with(fc44("hwloc-libs", "2.14.0-1").dep().depends(&["glibc"]))
        .offering(
            fc44("tree", "2.2.1-4")
                .depends(&["glibc"])
                .recommends(&["tree-doc"]),
        )
        .offering(fc44("tree-doc", "2.2.1-4"))
}

/// The argv the fake logged, without the root options every invocation under `--root` carries.
fn log(case: &Case) -> Vec<String> {
    let root = case.root.dir.display().to_string();
    case.log()
        .into_iter()
        .map(|line| {
            line.replace(&format!(" --installroot={root}"), "")
                .replace(&format!(" --root {root}"), "")
        })
        .collect()
}

/// Whether one logged argv changes the machine rather than reading or simulating it.
fn mutates(line: &str) -> bool {
    line.starts_with("dnf5 ") && line.contains(" -y ") && !line.contains(" --downloadonly")
        || line.starts_with("dnf5 ") && line.contains("makecache")
}

/// The imported manifest with `htop` deleted and `tree` declared.
fn edited(case: &Case) {
    case.delete_line("htop");
    let manifest = case.manifest();
    assert!(manifest.contains("add = [\n"), "{manifest}");
    case.set_manifest(&manifest.replacen("add = [\n", "add = [\n  \"tree\",\n", 1));
}

#[test]
fn import_writes_packages_fedora_and_matches_id_fedora_only() {
    let case = Case::new("dnf-import", chosen());
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let manifest = case.manifest();
    assert!(manifest.contains("distro = \"fedora\""), "{manifest}");
    assert!(manifest.contains("\n[packages.fedora]\n"), "{manifest}");
    assert!(!manifest.contains("\n[packages]\n"), "{manifest}");
    assert!(
        !manifest.contains("snapshot = "),
        "no dated archive on fedora\n{manifest}"
    );
    let declared: Vec<&str> = manifest
        .lines()
        .skip_while(|line| *line != "add = [")
        .skip(1)
        .take_while(|line| *line != "]")
        .map(|line| line.trim().trim_end_matches(',').trim_matches('"'))
        .collect();
    // What the person chose, the kernel a machine does not boot without, and what dnf protects;
    // never what was pulled in as a dependency.
    assert_eq!(declared, ["bash", "dnf5", "htop", "kernel"], "{manifest}");
    for line in log(&case) {
        assert!(!mutates(&line), "an import changed the machine: {line}");
    }

    // A distribution that is only like Fedora is not Fedora.
    let like = "NAME=\"Nobara\"\nID=nobara\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=44\n";
    case.root.write("etc/os-release", like);
    let refused = case.verb("import", &["--dry-run"]);
    assert_eq!(refused.status.code(), Some(3), "{}", story(&refused));
    assert!(
        err(&refused).contains("E_UNSUPPORTED") && err(&refused).contains("ID=nobara"),
        "{}",
        story(&refused)
    );
    assert!(case.log().is_empty(), "nothing was read: {:?}", case.log());
}

#[test]
fn exact_apply_removes_the_undeclared_package_then_changes_nothing() {
    let case = Case::new("dnf-exact", chosen());
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    edited(&case);

    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    let installed = case.installed();
    assert!(!installed.contains("htop"), "{}", story(&apply));
    assert!(
        !installed.contains("hwloc-libs"),
        "the dependency nothing needs left with it\n{}",
        story(&apply)
    );
    assert!(installed.contains("tree"), "{}", story(&apply));
    assert!(
        !installed.contains("tree-doc"),
        "a weak dependency was installed\n{}",
        story(&apply)
    );
    let changed: Vec<String> = log(&case).into_iter().filter(|l| mutates(l)).collect();
    assert_eq!(
        changed,
        [
            "dnf5 -y --setopt=install_weak_deps=False install tree",
            "dnf5 -y remove htop",
        ],
        "{}",
        story(&apply)
    );
    assert!(case.recorded().contains("tree"), "{}", case.lock());
    assert!(!case.recorded().contains("htop"), "{}", case.lock());

    let again = case.apply(&[]);
    assert!(again.status.success(), "{}", story(&again));
    assert!(nothing(&again), "{}", story(&again));
    let changed: Vec<String> = log(&case).into_iter().filter(|l| mutates(l)).collect();
    assert!(changed.is_empty(), "{changed:?}");
}

/// A refresh lodi ran is recorded beside its journals, so the index counts from it even where
/// dnf5 left an unchanged repository's cached `repomd.xml` as it was (#421).
#[test]
fn a_refresh_is_recorded_and_the_next_apply_changes_nothing() {
    let case = Case::new("dnf-refreshed", chosen());
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    fs::remove_dir_all(case.root.path("var/cache/libdnf5")).unwrap();
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(
        log(&case).iter().any(|l| l.contains(" makecache")),
        "{}",
        story(&apply)
    );
    assert!(
        case.root.path("var/lib/lodi/host/dnf-refreshed").is_file(),
        "{}",
        story(&apply)
    );
    let again = case.apply(&[]);
    assert!(nothing(&again), "{}", story(&again));
}

#[test]
fn plan_shows_the_dnf_transaction_and_changes_nothing() {
    let case = Case::new("dnf-plan", chosen());
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    edited(&case);
    let before = case.machine();
    let manifest = case.manifest();
    let lock = case.root.read("etc/lodi/host.lock");

    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    let text = err(&plan);
    assert!(
        text.contains("+ package tree 0:2.2.1-4.fc44.x86_64\n"),
        "{}",
        story(&plan)
    );
    assert!(
        text.contains("- package htop 0:3.4.1-3.fc44.x86_64 (not in the manifest)\n"),
        "{}",
        story(&plan)
    );
    assert!(
        text.contains("- package hwloc-libs 0:2.14.0-1.fc44.x86_64 ("),
        "{}",
        story(&plan)
    );
    assert_eq!(case.machine(), before, "a plan changed the machine");
    assert_eq!(case.manifest(), manifest);
    assert_eq!(
        case.root.read("etc/lodi/host.lock"),
        lock,
        "a plan wrote the lock"
    );
    for line in log(&case) {
        assert!(!mutates(&line), "a plan ran {line}");
    }
    assert!(
        log(&case)
            .iter()
            .any(|l| l == "dnf5 -C remove --assumeno htop"),
        "the removal is dnf's own answer: {:?}",
        log(&case)
    );
}

#[test]
fn an_rpm_ostree_system_is_rejected_before_any_change() {
    let case = Case::new("dnf-ostree", chosen());
    case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"fedora\"\n\n[system]\nhostname = \"box\"\n\n\
         [packages.fedora]\nadd = [\"tree\"]\n",
    );
    case.root.write("run/ostree-booted", "");
    let before = case.machine();
    for verb in ["apply", "plan", "import"] {
        let refused = case.verb(verb, &[]);
        assert_eq!(
            refused.status.code(),
            Some(3),
            "{verb}: {}",
            story(&refused)
        );
        let text = err(&refused);
        assert!(
            text.contains("E_HOST_OSTREE") && text.contains("rpm-ostree"),
            "{verb}: {}",
            story(&refused)
        );
        assert!(case.log().is_empty(), "{verb} ran {:?}", case.log());
    }
    assert_eq!(case.machine(), before);
    assert!(!case.has_lock());
    assert!(
        !case.root.exists("var/lib/lodi/host"),
        "no journal, no state"
    );
}

/// An ostree root is refused for what it is before lodi asks whether it may manage the machine
/// or what the machine is called: neither the marker nor the hostname is there (LD-528).
#[test]
fn an_rpm_ostree_system_is_named_before_the_marker_or_the_hostname() {
    let case = Case::new("dnf-ostree-first", chosen());
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"fedora\"\n");
    case.root.write("run/ostree-booted", "");
    for extra in [&[][..], &["--dry-run"][..]] {
        let mut command = case.command("switch", extra, &[]);
        fs::remove_file(case.root.path("etc/lodi/may-manage")).unwrap();
        let _ = fs::remove_file(case.root.path("etc/hostname"));
        let refused = {
            let _spawning = fakehost::spawning();
            command
                .stdin(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .expect("lodi runs")
        };
        assert_eq!(
            refused.status.code(),
            Some(3),
            "{extra:?}: {}",
            story(&refused)
        );
        assert!(
            err(&refused).contains("E_HOST_OSTREE"),
            "{}",
            story(&refused)
        );
        assert!(case.log().is_empty(), "{extra:?} ran {:?}", case.log());
    }
}

/// The destination below the scratch root, and the label it carries before and after.
const MANAGED: &str = "etc/ssh/sshd_config.d/50-lodi.conf";
const LABEL: &str = "system_u:object_r:sshd_config_t:s0";

/// Run inside a user and mount namespace: mount a tmpfs, copy the prepared root onto it, label
/// the destination, apply, and report what the destination is afterwards as one JSON line.
const IN_NAMESPACE: &str = r#"
import json, os, subprocess, sys
root, source, lodi, rel, label = sys.argv[1:6]
subprocess.run(["mount", "-t", "tmpfs", "lodi-label", root], check=True)
subprocess.run(["cp", "-a", source + "/.", root], check=True)
dest = os.path.join(root, rel)
os.setxattr(dest, "security.selinux", label.encode() + b"\0")
before = os.stat(dest).st_ino
run = subprocess.run([lodi, "switch", "--host", "--root", root], capture_output=True, text=True,
                     stdin=subprocess.DEVNULL)
after = os.stat(dest)
print(json.dumps({
    "status": run.returncode, "stdout": run.stdout, "stderr": run.stderr,
    "label": os.getxattr(dest, "security.selinux").rstrip(b"\0").decode(),
    "renamed": after.st_ino != before, "bytes": open(dest).read(),
    "beside": sorted(os.listdir(os.path.dirname(dest))),
}))
"#;

#[test]
fn a_managed_file_is_renamed_in_place_and_keeps_its_label() {
    let case = Case::new("dnf-label", Machine::fedora());
    case.set_manifest(&format!(
        "[host]\nversion = \"1\"\ndistro = \"fedora\"\n\n[system]\nhostname = \"box\"\n\n\
         [files.\"/{MANAGED}\"]\ncontent = \"PasswordAuthentication no\\n\"\nbackup = false\n"
    ));
    // The owner agreed once: the "may manage" marker `lodi import` leaves (#705).
    case.root
        .write("etc/lodi/may-manage", "")
        .chmod("etc/lodi/may-manage", 0o644);
    case.root.write(MANAGED, "PasswordAuthentication yes\n");
    case.root.write("etc/group", "root:x:0:\n");
    let mount = support::scratch("dnf-label-mount");
    let _spawning = fakehost::spawning();
    let run = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "python3",
            "-B",
            "-c",
        ])
        .arg(IN_NAMESPACE)
        .arg(&mount)
        .arg(&case.root.dir)
        .arg(env!("CARGO_BIN_EXE_lodi"))
        .args([MANAGED, LABEL])
        .env("PATH", case.fake_path())
        // The root run reads the config the copy carries, below the root it changes.
        .env("HOME", mount.join("home"))
        .env("XDG_CONFIG_HOME", mount.join("home/config"))
        .env("XDG_DATA_HOME", mount.join("home/data"))
        .env("LODI_HOME", mount.join("lodi-home"))
        .env_remove("XDG_STATE_HOME")
        .env_remove("LODI_REPO")
        .env_remove("SUDO_UID")
        .env_remove("SUDO_GID")
        .env_remove("DOAS_USER")
        .env("LODI_HOST_REQUIRE_ROOT", "1")
        .stdin(Stdio::null())
        .output()
        .expect("unshare runs");
    let _ = fs::remove_dir_all(&mount);
    assert!(run.status.success(), "{}", story(&run));
    let report: serde_json::Value =
        serde_json::from_str(out(&run).trim()).expect("one JSON report");
    assert_eq!(report["status"], 0, "{report:#}");
    assert_eq!(report["bytes"], "PasswordAuthentication no\n", "{report:#}");
    assert_eq!(
        report["renamed"], true,
        "written in place, not renamed\n{report:#}"
    );
    assert_eq!(report["label"], LABEL, "the label was lost\n{report:#}");
    assert_eq!(
        report["beside"],
        serde_json::json!(["50-lodi.conf"]),
        "a temporary was left beside it\n{report:#}"
    );
    for line in log(&case) {
        for never in ["setenforce", "restorecon", "chcon"] {
            assert!(!line.starts_with(never), "lodi ran {line}");
        }
    }
}

#[test]
fn fedora_settings_are_never_declared_or_captured() {
    let case = Case::new("dnf-settings", Machine::fedora());
    for path in [
        "/etc/yum.repos.d/lodi.repo",
        "/etc/pki/rpm-gpg/KEY",
        "/etc/dnf/vars/releasever",
        "/etc/selinux/config",
    ] {
        case.set_manifest(&format!(
            "[host]\nversion = \"1\"\ndistro = \"fedora\"\n\n[files.\"{path}\"]\ncontent = \"x\"\n"
        ));
        let plan = case.plan();
        assert_eq!(plan.status.code(), Some(3), "{path}: {}", story(&plan));
        assert!(err(&plan).contains("E_PROTECTED_PATH"), "{}", story(&plan));
    }

    // A repository file the package manager reports as changed is named, never captured.
    let case = Case::new(
        "dnf-settings-import",
        Machine::fedora()
            .with(fc44("fedora-repos", "44-3").conffile("/etc/yum.repos.d/fedora.repo")),
    );
    case.root
        .write("etc/yum.repos.d/fedora.repo", "[fedora]\nenabled=1\n");
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    assert!(
        !case.manifest().contains("yum.repos.d/fedora.repo\"]"),
        "{}",
        case.manifest()
    );
    assert!(
        !case
            .beside("files/host/etc/yum.repos.d/fedora.repo")
            .exists()
    );
    assert!(
        err(&import).contains("/etc/yum.repos.d/fedora.repo"),
        "{}",
        story(&import)
    );
}
