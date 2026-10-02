//! LD-375: the machine follows `host.toml`. Deleting a package's line from an imported manifest
//! and applying it again removes that package — on both families, decided by the
//! package manager's own answers, with the record only as a fence against changes made by hand.
//!
//! Every case here is the real binary against a scratch `--root` with a **fake machine** behind
//! it (`tests/support/fakehost.rs`, `tests/support/fakepm.py`): a package graph that the fake
//! pacman and apt answer from and change when they are told to. Nothing reaches `/` and no real
//! package manager runs (`AGENTS.md` §8).

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use fakehost::{Case, Family, Machine, Pkg, err, nothing, story};

/// An Arch machine a person chose `tree` and `jq` on; `jq` pulled `oniguruma` in.
fn arch_chosen() -> Machine {
    Machine::arch()
        .with(Pkg::new("tree"))
        .with(Pkg::new("jq").depends(&["oniguruma"]))
        .with(Pkg::new("oniguruma").dep())
}

/// The same choices on a Debian machine.
fn debian_chosen() -> Machine {
    Machine::debian()
        .with(Pkg::new("tree").depends(&["libc6"]))
        .with(Pkg::new("jq").depends(&["libjq1"]))
        .with(Pkg::new("libjq1").dep().depends(&["libc6"]))
}

fn machine_for(family: Family) -> Machine {
    match family {
        Family::Pacman => arch_chosen(),
        Family::Apt => debian_chosen(),
    }
}

// ------------------------------------------------------------------ E1: the owner's case ---

/// The owner's scenario of 2026-09-23, exactly: import, delete one declared leaf package's
/// line, apply. The package leaves the machine, the plan said why, pacman removed it with `-Rs`,
/// the record forgot it, and a second apply has nothing to do. No flag was needed.
fn deleting_a_line_removes_the_package(family: Family, name: &str) {
    let case = Case::new(&format!("e1-{name}-{family:?}"), machine_for(family));
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    assert!(case.manifest().contains("\"tree\""), "{}", case.manifest());

    case.delete_line("tree");

    let plan = case.plan();
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(
        !case.installed().contains("tree"),
        "tree is still installed after the apply\n{}",
        story(&apply)
    );
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        err(&plan).contains("- package tree (not in the manifest)"),
        "the plan names the removal and its reason\n{}",
        story(&plan)
    );
    assert!(
        err(&apply).contains("- package tree (not in the manifest)"),
        "the apply names the removal and its reason\n{}",
        story(&apply)
    );
    // Exactly that package: everything else the machine had is still there.
    let mut expected = machine_for(family)
        .installed
        .iter()
        .map(|p| p.name.clone())
        .collect::<std::collections::BTreeSet<_>>();
    expected.remove("tree");
    assert_eq!(case.installed(), expected, "{}", story(&apply));
    if family == Family::Pacman {
        let removal: Vec<String> = case
            .log()
            .into_iter()
            .filter(|line| line.starts_with("pacman -R") && !line.starts_with("pacman -Rsp"))
            .collect();
        assert_eq!(removal.len(), 1, "{removal:?}");
        assert!(
            removal[0].starts_with("pacman -Rs ") && removal[0].ends_with("-- tree"),
            "{removal:?}"
        );
    }
    assert!(
        !case.recorded().contains("tree"),
        "the record still names tree: {}",
        case.lock()
    );
    assert!(case.recorded().contains("jq"), "{}", case.lock());

    let again = case.apply(&[]);
    assert!(again.status.success(), "{}", story(&again));
    assert!(nothing(&again), "{}", story(&again));
}

#[test]
fn e1_deleting_a_line_removes_the_package_on_pacman() {
    deleting_a_line_removes_the_package(Family::Pacman, "tree");
}

#[test]
fn e1_deleting_a_line_removes_the_package_on_apt() {
    deleting_a_line_removes_the_package(Family::Apt, "tree");
}

/// Whether one logged argv changes the machine, rather than reading or simulating it.
fn mutates(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["pacman", op, ..] => {
            matches!(*op, "-Syu" | "-S" | "-Rs" | "-Rns" | "-D")
        }
        ["apt-get", "install", ..] => !words.contains(&"-s"),
        ["apt-get", "update", ..] => true,
        ["apt-mark", verb, ..] => !verb.starts_with("show"),
        _ => false,
    }
}

// -------------------------------------------------------------- E2: exact | managed | unset ---

fn replace_in_manifest(case: &Case, from: &str, to: &str) {
    let text = case.manifest();
    assert!(text.contains(from), "the manifest has no {from:?}:\n{text}");
    case.set_manifest(&text.replacen(from, to, 1));
}

/// A manifest of ours, with `[host]` saying what it is told to.
fn manifest(host: &str, common: &[&str], extra: &str) -> String {
    let names: Vec<String> = common.iter().map(|n| format!("\"{n}\"")).collect();
    format!(
        "[host]\nversion = \"1\"\n{host}\n[packages]\ncommon = [{}]\n{extra}",
        names.join(", ")
    )
}

#[test]
fn e2_the_packages_key_is_exact_or_managed() {
    let case = Case::new("e2-key", arch_chosen());
    case.set_manifest(&manifest("packages = \"everything\"\n", &["tree"], ""));
    let plan = case.plan();
    assert_eq!(plan.status.code(), Some(3), "{}", story(&plan));
    assert!(err(&plan).contains("E_TYPE"), "{}", story(&plan));
    assert!(
        err(&plan).contains("`exact`") && err(&plan).contains("`managed`"),
        "the error names both values\n{}",
        story(&plan)
    );

    case.set_manifest(&manifest(
        "packages = \"exact\"\nauto_remove = false\n",
        &["tree"],
        "",
    ));
    let plan = case.plan();
    assert_eq!(plan.status.code(), Some(3), "{}", story(&plan));
    assert!(err(&plan).contains("E_ATTR_CONFLICT"), "{}", story(&plan));
}

#[test]
fn e2_import_writes_exact_and_says_what_it_means() {
    let case = Case::new("e2-import", arch_chosen());
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let text = case.manifest();
    let host = text
        .split("[host]\n")
        .nth(1)
        .and_then(|rest| rest.split("\n[").next())
        .expect("a [host] table");
    assert!(host.contains("\npackages = \"exact\"\n"), "{host}");
    // The 2.0 floor (LD-528): 2.0 reads no 1.x config and 1.x none of 2.0's, so the file names
    // `>=2.0.0` from 2.0.0 on, and no 1.x lodi applies it.
    let floor = format!("\nmin_lodi_version = \">={}\"\n", floor());
    assert!(host.contains(&floor), "the import names its floor\n{host}");
    // And the lodi that wrote it applies it: the round trip stays usable.
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        host.contains("# packages = \"exact\": a switch makes this machine's packages"),
        "the comment says what exact means:\n{host}"
    );
    // The comment that promised removal only of what lodi itself installed is gone.
    assert!(
        !text.contains("an apply removes what lodi\n# itself installed and this file no longer names, and nothing else"),
        "{text}"
    );
}

/// `managed` is 1.1.0's rule exactly: what the record names leaves with `-Rns`, and a package
/// installed by hand is never touched and never mentioned.
#[test]
fn e2_managed_keeps_the_recorded_removal() {
    let case = Case::new("e2-managed", arch_chosen());
    assert!(case.import().status.success());
    replace_in_manifest(
        &case,
        "\npackages = \"exact\"\n",
        "\npackages = \"managed\"\n",
    );
    case.install_by_hand(Pkg::new("htop"));
    case.delete_line("tree");
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(!case.installed().contains("tree"), "{}", story(&apply));
    assert!(case.installed().contains("htop"), "{}", story(&apply));
    assert!(
        case.log()
            .iter()
            .any(|line| line.starts_with("pacman -Rns ") && line.ends_with("-- tree")),
        "{:?}",
        case.log()
    );
    assert!(!err(&apply).contains("htop"), "{}", story(&apply));
}

/// Without the key: `managed`, plus one `W_UNDECLARED` line per explicitly installed
/// distribution package the manifest does not declare, with the repair.
#[test]
fn e2_without_the_key_undeclared_packages_are_named() {
    let case = Case::new("e2-unset", arch_chosen());
    assert!(case.import().status.success());
    replace_in_manifest(&case, "\npackages = \"exact\"\n", "\n");
    case.install_by_hand(Pkg::new("htop"));
    for output in [case.plan(), case.apply(&[])] {
        assert!(output.status.success(), "{}", story(&output));
        assert!(
            err(&output).contains(
                "W_UNDECLARED: htop is explicitly installed and host.toml does not declare it"
            ),
            "{}",
            story(&output)
        );
        assert!(
            err(&output).contains("packages = \"exact\""),
            "the one-line repair is named\n{}",
            story(&output)
        );
        assert!(
            !err(&output).contains("W_UNDECLARED: tree"),
            "{}",
            story(&output)
        );
    }
    assert!(case.installed().contains("htop"));
}

// ------------------------------------------------------------------ E3: what can leave ---

/// A `mark_auto` name and a third-party package the record names both leave when their lines do.
#[test]
fn e3_mark_auto_and_recorded_third_party_leave_with_their_lines() {
    for family in [Family::Pacman, Family::Apt] {
        let machine = machine_for(family)
            .offering(Pkg::new("vendortool").repo(Some("vendor")))
            .offering(Pkg::new("bc"));
        let case = Case::new(&format!("e3-leave-{family:?}"), machine);
        assert!(case.import().status.success());
        replace_in_manifest(
            &case,
            "common = [\n",
            "common = [\n  \"vendortool\",\n  \"bc\",\n",
        );
        replace_in_manifest(&case, "[packages]\n", "[packages]\nmark_auto = [\"bc\"]\n");
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{family:?}: {}", story(&apply));
        assert!(case.recorded().contains("vendortool"), "{}", case.lock());
        assert!(case.recorded().contains("bc"), "{}", case.lock());
        assert!(!case.is_explicit("bc"), "{family:?}");

        case.delete_line("vendortool");
        case.delete_line("bc");
        replace_in_manifest(&case, "mark_auto = [\"bc\"]\n", "");
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{family:?}: {}", story(&apply));
        let installed = case.installed();
        assert!(
            !installed.contains("vendortool"),
            "{family:?}: {}",
            story(&apply)
        );
        assert!(!installed.contains("bc"), "{family:?}: {}", story(&apply));
    }
}

/// A third-party, local-only or foreign package the record never named is never touched.
#[test]
fn e3_an_unrecorded_package_from_elsewhere_is_never_touched() {
    for family in [Family::Pacman, Family::Apt] {
        let machine = machine_for(family)
            .with(Pkg::new("vendortool").repo(Some("vendor")))
            .with(Pkg::new("handmade").repo(None));
        let case = Case::new(&format!("e3-foreign-{family:?}"), machine);
        assert!(case.import().status.success());
        let before = case.installed();
        for output in [case.plan(), case.apply(&[])] {
            assert!(output.status.success(), "{family:?}: {}", story(&output));
            assert!(
                !err(&output).contains("W_DRIFT"),
                "{family:?}: {}",
                story(&output)
            );
        }
        assert_eq!(case.installed(), before, "{family:?}");
        let changes: Vec<String> = case.log().into_iter().filter(|l| mutates(l)).collect();
        assert!(
            !changes
                .iter()
                .any(|l| l.contains("vendortool") || l.contains("handmade")),
            "{family:?}: {changes:?}"
        );
    }
}

/// A baseline or kernel name taken out of the manifest stays, and the plan says so.
#[test]
fn e3_a_deleted_baseline_or_kernel_line_is_kept_and_said() {
    for (family, kernel, meta) in [
        (Family::Pacman, "linux", Some("base")),
        (Family::Apt, "linux-image-amd64", None),
    ] {
        let case = Case::new(&format!("e3-baseline-{family:?}"), machine_for(family));
        assert!(case.import().status.success());
        let before = case.installed();
        case.delete_line(kernel);
        if let Some(meta) = meta {
            case.delete_line(meta);
        }
        let plan = case.plan();
        assert!(plan.status.success(), "{family:?}: {}", story(&plan));
        assert!(
            err(&plan).contains(&format!("= package {kernel} (baseline; not removed)")),
            "{family:?}: {}",
            story(&plan)
        );
        if let Some(meta) = meta {
            assert!(
                err(&plan).contains(&format!("= package {meta} (baseline; not removed)")),
                "{}",
                story(&plan)
            );
        }
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{family:?}: {}", story(&apply));
        assert!(
            err(&apply).contains(&format!("= package {kernel} (baseline; not removed)")),
            "{family:?}: {}",
            story(&apply)
        );
        assert_eq!(case.installed(), before, "{family:?}");
    }
}

/// A pacman group is not a package: declaring one is refused, by name, before anything moves.
#[test]
fn e3_a_pacman_group_is_refused_by_name() {
    let case = Case::new("e3-group", arch_chosen());
    case.edit(|m| m["groups"] = serde_json::json!({"xorg": ["xorg-server", "xorg-xinit"]}));
    assert!(case.import().status.success());
    replace_in_manifest(&case, "common = [\n", "common = [\n  \"xorg\",\n");
    let before = case.machine();
    for output in [case.plan(), case.apply(&[])] {
        assert_eq!(output.status.code(), Some(3), "{}", story(&output));
        assert!(
            err(&output).contains("`xorg` is a pacman group"),
            "{}",
            story(&output)
        );
    }
    assert_eq!(case.machine(), before);
}

// ------------------------------------------------- E4: the package manager decides the set ---

/// pacman: a deleted root something declared still requires (through what it provides) is
/// demoted and stays; once its last consumer leaves, `-Rs` takes it with it.
#[test]
fn e4_pacman_demotes_a_needed_root_and_removes_it_with_its_last_consumer() {
    let machine = arch_chosen()
        .with(Pkg::new("libfoo").provides(&["libfoo.so"]))
        .with(Pkg::new("app").depends(&["libfoo.so"]));
    let case = Case::new("e4-pacman", machine);
    assert!(case.import().status.success());
    case.delete_line("libfoo");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        err(&plan).contains("~ package libfoo (a dependency now: needed by app; stays)"),
        "{}",
        story(&plan)
    );
    assert!(
        case.log().iter().any(|l| l.starts_with("pacman -Qi")),
        "{:?}",
        case.log()
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(case.installed().contains("libfoo"));
    assert!(!case.is_explicit("libfoo"));
    assert!(
        case.log()
            .iter()
            .any(|l| l.starts_with("pacman -D ") && l.contains(" --asdeps -- libfoo")),
        "{:?}",
        case.log()
    );
    assert!(
        !case.log().iter().any(|l| l.starts_with("pacman -R")),
        "{:?}",
        case.log()
    );

    // The delayed orphan: its last consumer leaves later, and takes it along.
    case.delete_line("app");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        err(&plan).contains("- package app (not in the manifest)"),
        "{}",
        story(&plan)
    );
    assert!(
        err(&plan).contains("- package libfoo (needed by nothing once the rest is removed)"),
        "{}",
        story(&plan)
    );
    assert!(
        case.log()
            .iter()
            .any(|l| l.starts_with("pacman -Rsp") && l.ends_with("-- app")),
        "{:?}",
        case.log()
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    let installed = case.installed();
    assert!(
        !installed.contains("app") && !installed.contains("libfoo"),
        "{installed:?}"
    );
    for line in case.log() {
        assert!(!line.contains("-Qdt") && !line.contains("-Rns"), "{line}");
    }
}

/// apt: the same lifecycle, decided by `apt-get -s`: a root whose removal would take a declared
/// package with it is demoted; the orphans a change creates are named and removed; and a package
/// something installed by hand still recommends is not an orphan.
#[test]
fn e4_apt_demotes_a_needed_root_and_removes_the_orphans_it_causes() {
    let machine = debian_chosen()
        .with(Pkg::new("libfoo"))
        .with(Pkg::new("app").depends(&["libfoo"]))
        .with(Pkg::new("tool").depends(&["libbar"]))
        .with(Pkg::new("libbar").dep())
        .with(Pkg::new("viewer").recommends(&["libbar"]));
    let case = Case::new("e4-apt", machine);
    assert!(case.import().status.success());
    case.delete_line("libfoo");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        err(&plan).contains("~ package libfoo (a dependency now: needed by app; stays)"),
        "{}",
        story(&plan)
    );
    assert!(
        case.log()
            .iter()
            .any(|l| l.starts_with("apt-get install") && l.contains(" -s ")),
        "{:?}",
        case.log()
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(case.installed().contains("libfoo") && case.installed().contains("app"));
    assert!(!case.is_explicit("libfoo"));

    case.delete_line("app");
    case.delete_line("tool");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        err(&plan).contains("- package app (not in the manifest)"),
        "{}",
        story(&plan)
    );
    assert!(
        err(&plan).contains("- package tool (not in the manifest)"),
        "{}",
        story(&plan)
    );
    assert!(
        err(&plan).contains("- package libfoo (needed by nothing once the rest is removed)"),
        "{}",
        story(&plan)
    );
    // Recommended by `viewer`, which stays: apt keeps it, and so does lodi.
    assert!(!err(&plan).contains("libbar"), "{}", story(&plan));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    let installed = case.installed();
    for gone in ["app", "tool", "libfoo"] {
        assert!(!installed.contains(gone), "{gone}: {installed:?}");
    }
    assert!(installed.contains("libbar") && installed.contains("viewer"));
    for line in case.log() {
        assert!(!line.contains("autoremove"), "{line}");
    }
}

/// apt, exact: the transaction and the simulation that decides it are apt's own default, so a new
/// package's `Recommends` come with it, exactly as the simulation said. `managed` keeps 1.1.0's
/// `--no-install-recommends` byte for byte (LD-375 validator repair).
#[test]
fn e4_apt_exact_keeps_apts_default_recommends_in_the_simulation_and_the_transaction() {
    let machine = || {
        debian_chosen()
            .offering(Pkg::new("viewer").recommends(&["viewer-data"]))
            .offering(Pkg::new("viewer-data"))
    };
    let case = Case::new("e4-apt-recommends", machine());
    assert!(case.import().status.success());
    replace_in_manifest(&case, "common = [\n", "common = [\n  \"viewer\",\n");
    // A removal too, so that the transaction is simulated before it runs.
    case.delete_line("tree");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    let simulated: Vec<String> = case
        .log()
        .into_iter()
        .filter(|l| l.starts_with("apt-get install") && l.contains(" -s "))
        .collect();
    assert!(!simulated.is_empty(), "{:?}", case.log());
    for line in &simulated {
        assert!(
            !line.contains("--no-install-recommends"),
            "the exact simulation turns apt's default Recommends off: {line}"
        );
    }
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    let ran: Vec<String> = case
        .log()
        .into_iter()
        .filter(|l| l.starts_with("apt-get install") && !l.contains(" -s "))
        .filter(|l| !l.contains("--download-only"))
        .collect();
    assert_eq!(ran.len(), 1, "{:?}", case.log());
    assert!(
        !ran[0].contains("--no-install-recommends"),
        "the exact transaction turns apt's default Recommends off: {}",
        ran[0]
    );
    let installed = case.installed();
    assert!(
        installed.contains("viewer") && installed.contains("viewer-data"),
        "{installed:?}"
    );
    assert!(!case.is_explicit("viewer-data"));
    assert!(!installed.contains("tree"), "{installed:?}");
    let again = case.apply(&[]);
    assert!(nothing(&again), "{}", story(&again));

    // `managed` is 1.1.0's rule, and 1.1.0 installed without Recommends.
    let case = Case::new("e4-apt-recommends-managed", machine());
    case.set_manifest(&manifest(
        "packages = \"managed\"\n",
        &["tree", "jq", "viewer"],
        "",
    ));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    let ran: Vec<String> = case
        .log()
        .into_iter()
        .filter(|l| l.starts_with("apt-get install") && !l.contains("--download-only"))
        .collect();
    assert_eq!(ran.len(), 1, "{:?}", case.log());
    assert!(ran[0].contains(" --no-install-recommends "), "{}", ran[0]);
    assert!(!case.installed().contains("viewer-data"));
}

// ----------------------------------------------- E5: re-checked, and refused before anything ---

/// The graph changes between the plan and the removal: the apply stops there, nothing removed.
#[test]
fn e5_a_removal_set_that_changed_since_the_plan_stops_the_apply() {
    // pacman: the -Syu that installs `bc` gives `tree` a new dependency.
    let case = Case::new("e5-pacman", arch_chosen().offering(Pkg::new("bc")));
    assert!(case.import().status.success());
    replace_in_manifest(&case, "common = [\n", "common = [\n  \"bc\",\n");
    case.delete_line("tree");
    case.edit(|m| {
        m["on"] = serde_json::json!({"upgrade": {
            "tree": {"version": "2.0-1", "explicit": true, "depends": ["libtree"]},
            "libtree": {"version": "1.0-1", "explicit": false},
        }});
    });
    assert!(case.plan().status.success());
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(8), "{}", story(&apply));
    assert!(err(&apply).contains("E_APPLY"), "{}", story(&apply));
    assert!(
        err(&apply).contains("nothing was removed"),
        "{}",
        story(&apply)
    );
    assert!(case.installed().contains("tree"));
    assert!(
        !case.log().iter().any(|l| l.starts_with("pacman -Rs ")),
        "{:?}",
        case.log()
    );

    // apt: the refresh makes something declared depend on the package being removed.
    let case = Case::new("e5-apt", debian_chosen());
    assert!(case.import().status.success());
    case.delete_line("tree");
    let _ = std::fs::remove_file(case.root.path("var/lib/apt/lists/fake_Packages"));
    case.edit(|m| {
        m["on"] = serde_json::json!({"update": {
            "jq": {"version": "2.0-1", "explicit": true, "depends": ["libjq1", "tree"]},
        }});
    });
    assert!(case.plan().status.success());
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(8), "{}", story(&apply));
    assert!(
        err(&apply).contains("nothing was removed"),
        "{}",
        story(&apply)
    );
    assert!(case.installed().contains("tree") && case.installed().contains("jq"));
}

/// A name the index could not place when the plan was made is carried past the refresh and
/// asked about only after it (LD-379, the supervisor's guest run). Under `exact` the apply then
/// asks apt again what installing it takes off the machine, and stops before the transaction
/// when that is more than the plan removed: here a `Conflicts` the refreshed index brings would
/// take a declared package. Nothing is installed or removed.
#[test]
fn e5_apt_a_name_the_refresh_brings_is_rechecked_before_its_transaction() {
    let machine = debian_chosen().offering_once_fetched(Pkg::new("newjq").conflicts(&["jq"]));
    let case = Case::new("e5-apt-fetched-conflict", machine);
    assert!(case.import().status.success());
    let _ = std::fs::remove_file(case.root.path("var/lib/apt/lists/fake_Packages"));
    replace_in_manifest(&case, "common = [\n", "common = [\n  \"newjq\",\n");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(err(&plan).contains("+ package newjq"), "{}", story(&plan));
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(8), "{}", story(&apply));
    assert!(
        err(&apply).contains("would now remove jq where the plan removed nothing"),
        "{}",
        story(&apply)
    );
    assert!(
        err(&apply).contains("nothing was removed"),
        "{}",
        story(&apply)
    );
    assert!(
        case.installed().contains("jq") && !case.installed().contains("newjq"),
        "{}",
        story(&apply)
    );
    assert!(
        !case
            .log()
            .iter()
            .any(|line| line.starts_with("apt-get install") && !line.contains(" -s ")),
        "{:?}",
        case.log()
    );
}

/// A removal that would take a declared name, a baseline package or an unrecorded package from
/// elsewhere is refused before anything changes, naming each.
#[test]
fn e5_a_removal_reaching_what_must_stay_is_refused_before_anything() {
    let cases = [
        // `helper` is declared (installed as a dependency, and marked so by the manifest).
        ("declared", Pkg::new("helper").dep(), true),
        // The kernel, installed as a dependency of `app`.
        ("baseline", Pkg::new("linux-lts").dep(), false),
        // An AUR build nothing recorded.
        ("foreign", Pkg::new("helper").dep().repo(None), false),
    ];
    for (what, dep, declare) in cases {
        let name = dep.name.clone();
        let machine = arch_chosen()
            .with(dep)
            .with(Pkg::new("app").depends(&[name.as_str()]));
        let case = Case::new(&format!("e5-refuse-{what}"), machine);
        assert!(case.import().status.success());
        if declare {
            replace_in_manifest(
                &case,
                "common = [\n",
                &format!("common = [\n  \"{name}\",\n"),
            );
            replace_in_manifest(
                &case,
                "[packages]\n",
                &format!("[packages]\nmark_auto = [\"{name}\"]\n"),
            );
            assert!(case.apply(&[]).status.success());
        }
        case.delete_line("app");
        let before = case.machine();
        for output in [case.plan(), case.apply(&[])] {
            assert_eq!(output.status.code(), Some(11), "{what}: {}", story(&output));
            assert!(
                err(&output).contains("E_DECLINED"),
                "{what}: {}",
                story(&output)
            );
            assert!(err(&output).contains(&name), "{what}: {}", story(&output));
        }
        assert_eq!(case.machine(), before, "{what}: the machine moved");
    }
}

/// apt: the same refusal, where apt's own transaction is what reaches — installing a declared
/// package whose `Conflicts` apt resolves by removing a declared package, a base-system package or
/// a package from elsewhere lodi never recorded. Refused by the plan and by the apply, naming it,
/// with the fake machine untouched (LD-375 validator repair).
#[test]
fn e5_apt_a_transaction_reaching_what_must_stay_is_refused_before_anything() {
    let cases = [
        // Explicitly installed from Debian: the import declares it.
        ("declared", Pkg::new("oldawk")),
        // Priority required: the base system the import subtracts.
        ("baseline", Pkg::new("oldawk").priority("required")),
        // From no repository at all: named under NOT CAPTURED, never recorded.
        ("foreign", Pkg::new("oldawk").repo(None)),
    ];
    for (what, pkg) in cases {
        let machine = debian_chosen()
            .with(pkg)
            .offering(Pkg::new("newawk").conflicts(&["oldawk"]));
        let case = Case::new(&format!("e5-apt-refuse-{what}"), machine);
        assert!(case.import().status.success(), "{what}");
        assert_eq!(
            case.manifest().contains("\"oldawk\""),
            what == "declared",
            "{what}: {}",
            case.manifest()
        );
        replace_in_manifest(&case, "common = [\n", "common = [\n  \"newawk\",\n");
        let before = case.machine();
        for output in [case.plan(), case.apply(&[])] {
            assert_eq!(output.status.code(), Some(11), "{what}: {}", story(&output));
            assert!(
                err(&output).contains("E_DECLINED"),
                "{what}: {}",
                story(&output)
            );
            assert!(
                err(&output).contains("oldawk"),
                "{what}: {}",
                story(&output)
            );
        }
        assert_eq!(case.machine(), before, "{what}: the machine moved");
        assert!(
            !case.log().iter().any(|line| mutates(line)),
            "{what}: {:?}",
            case.log()
        );
    }

    // What apt would call no longer needed once a line goes is removed only when nothing must
    // keep it: a declared name stays, and the plan says why.
    let machine = debian_chosen()
        .with(Pkg::new("helper").dep())
        .with(Pkg::new("app").depends(&["helper"]));
    let case = Case::new("e5-apt-protected-orphan", machine);
    assert!(case.import().status.success());
    replace_in_manifest(&case, "common = [\n", "common = [\n  \"helper\",\n");
    replace_in_manifest(
        &case,
        "[packages]\n",
        "[packages]\nmark_auto = [\"helper\"]\n",
    );
    assert!(case.apply(&[]).status.success());
    case.delete_line("app");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        err(&plan).contains("= package helper (no longer needed, but declared; not removed)"),
        "{}",
        story(&plan)
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(!case.installed().contains("app"), "{}", story(&apply));
    assert!(case.installed().contains("helper"), "{}", story(&apply));
}

// ------------------------------------------------------------------------- E6: the fence ---

/// A package installed by hand after the import is drift: named in the plan, refused by the
/// apply, removed only with --overwrite-drift. An edit of the manifest never needs the flag.
#[test]
fn e6_a_hand_install_is_drift_and_an_edit_is_not() {
    for family in [Family::Pacman, Family::Apt] {
        let case = Case::new(&format!("e6-{family:?}"), machine_for(family));
        assert!(case.import().status.success());
        case.install_by_hand(Pkg::new("htop"));
        case.delete_line("tree");
        let plan = case.plan();
        assert!(plan.status.success(), "{family:?}: {}", story(&plan));
        assert!(
            err(&plan).contains("W_DRIFT: package htop was installed by hand"),
            "{family:?}: {}",
            story(&plan)
        );
        let before = case.machine();
        let apply = case.apply(&[]);
        assert_eq!(
            apply.status.code(),
            Some(11),
            "{family:?}: {}",
            story(&apply)
        );
        // The preview is on standard error too; the refusal is the error and its hint.
        let text = err(&apply);
        let refusal = &text[text.find("E_DECLINED").unwrap_or(0)..];
        assert!(refusal.starts_with("E_DECLINED"), "{}", story(&apply));
        assert!(refusal.contains("htop"), "{}", story(&apply));
        assert!(
            !refusal.contains("tree"),
            "only the drift is named: {}",
            story(&apply)
        );
        assert_eq!(
            case.machine(),
            before,
            "{family:?}: the refusal moved the machine"
        );

        let apply = case.apply(&["--overwrite-drift"]);
        assert!(apply.status.success(), "{family:?}: {}", story(&apply));
        assert!(!case.installed().contains("htop") && !case.installed().contains("tree"));
        assert!(nothing(&case.apply(&[])));
    }
}

// ------------------------------------------------------------------------ E7: the record ---

#[test]
fn e7_the_import_writes_the_record() {
    let case = Case::new("e7-import", debian_chosen());
    case.edit(|m| m["installed"]["jq"]["held"] = serde_json::Value::Bool(true));
    // A record of a file an earlier apply wrote: the import keeps it.
    case.root.write(
        "etc/lodi/host.lock",
        &serde_json::json!({
            "version": 6, "format": "lodi-host-lock/6", "generatedBy": "lodi 2.0.0",
            "appliedAt": "2026-09-22T00:00:00Z", "distro": "debian", "distroVersion": "12",
            "packages": {},
            "files": {"/etc/kept.conf": {"digest": "sha256:00", "mode": "0644",
                      "owner": "root", "group": "root", "onRemove": "restore"}},
        })
        .to_string(),
    );
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let lock = case.lock();
    assert!(lock["files"]["/etc/kept.conf"].is_object(), "{lock}");
    assert_eq!(lock["packages"]["tree"]["mark"], "manual", "{lock}");
    assert_eq!(lock["packages"]["tree"]["version"], "1.0-1", "{lock}");
    assert_eq!(lock["packages"]["jq"]["held"], true, "{lock}");
    assert!(
        lock["packages"]["libc6"].is_null(),
        "the baseline is not recorded: {lock}"
    );
}

/// The record is kept true by every apply: a converged one says so and updates it, and a
/// files-only one records packages too. `nothing to do` only when neither changes.
#[test]
fn e7_every_apply_keeps_the_record_true() {
    let case = Case::new("e7-apply", arch_chosen());
    case.set_manifest(&manifest("packages = \"managed\"\n", &["tree", "jq"], ""));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(
        err(&apply).contains("host: no machine changes; the record is updated\n"),
        "{}",
        story(&apply)
    );
    assert_eq!(
        case.recorded(),
        ["jq", "tree"].iter().map(ToString::to_string).collect(),
        "{}",
        case.lock()
    );
    assert!(nothing(&case.apply(&[])));

    let case = Case::new("e7-files", arch_chosen());
    let (uid, gid) = hostroot::ids(&case.root);
    case.set_manifest(&manifest(
        "packages = \"managed\"\n",
        &["tree"],
        &format!(
            "\n[files.\"/etc/lodi-e7.conf\"]\ncontent = \"x\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
        ),
    ));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(case.recorded().contains("tree"), "{}", case.lock());
}

// ----------------------------------- E11: read as the transaction leaves the machine (LD-412) ---

/// The owner's report of 2026-09-24: `ubuntu-server` is declared, `apt-get remove --autoremove
/// git` took it off the machine by hand, and the next apply asked apt to install it and, in the
/// same transaction, to remove packages it depends on — which apt refused ("held broken
/// packages"). The base system is read as the machine will be **after** the install: the apply
/// puts the declared metapackage back, removes nothing it needs, needs no flag, and a second
/// apply has nothing to do. On pacman the same hand removal (`pacman -Rc` of something `base`
/// depends on) made the explicitly installed part of `base`'s closure look like drift.
fn a_declared_metapackage_a_hand_removal_took_is_restored(family: Family) {
    let (machine, meta, took, stays) = match family {
        Family::Apt => (
            debian_chosen()
                .with(
                    Pkg::new("server-meta")
                        .section("metapackages")
                        .depends(&["gitlike", "lvmlike"]),
                )
                .with(Pkg::new("gitlike").dep().depends(&["libc6"]))
                .with(Pkg::new("lvmlike").depends(&["libc6"])),
            "server-meta",
            "gitlike",
            "lvmlike",
        ),
        Family::Pacman => (
            arch_chosen()
                .with(Pkg::new("base").depends(&["glibc", "bash", "psmisc"]))
                .with(Pkg::new("psmisc").dep().depends(&["glibc"]))
                .with(Pkg::new("bash").depends(&["glibc"])),
            "base",
            "psmisc",
            "bash",
        ),
    };
    let case = Case::new(&format!("e11-restore-{family:?}"), machine);
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let manifest = case.manifest();
    assert!(manifest.contains(&format!("\"{meta}\"")), "{manifest}");
    assert!(!manifest.contains(&format!("\"{stays}\"")), "{manifest}");
    let before = case.installed();
    assert!(case.is_explicit(stays));

    // By hand: removing `took` takes the metapackage that depends on it along.
    case.edit(|m| {
        let installed = m["installed"].as_object_mut().expect("an installed set");
        installed.remove(meta);
        installed.remove(took);
    });

    let plan = case.plan();
    assert!(plan.status.success(), "{family:?}: {}", story(&plan));
    assert!(
        err(&plan).contains(&format!("+ package {meta}")),
        "{family:?}: {}",
        story(&plan)
    );
    assert!(
        !err(&plan).contains(&format!("package {stays} (")) && !err(&plan).contains("W_DRIFT"),
        "{family:?}: what the metapackage needs is neither removed, demoted nor drift\n{}",
        story(&plan)
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{family:?}: {}", story(&apply));
    assert_eq!(
        case.installed(),
        before,
        "{family:?}: the apply puts back what the hand removal took and removes nothing\n{}",
        story(&apply)
    );
    assert!(
        case.is_explicit(meta) && case.is_explicit(stays),
        "{family:?}: {}",
        case.machine()
    );
    let again = case.apply(&[]);
    assert!(again.status.success(), "{family:?}: {}", story(&again));
    assert!(nothing(&again), "{family:?}: {}", story(&again));
}

#[test]
fn e11_apt_a_declared_metapackage_a_hand_removal_took_is_restored() {
    a_declared_metapackage_a_hand_removal_took_is_restored(Family::Apt);
}

#[test]
fn e11_pacman_a_declared_metapackage_a_hand_removal_took_is_restored() {
    a_declared_metapackage_a_hand_removal_took_is_restored(Family::Pacman);
}

/// The same reading for a package that is not a metapackage: one edit declares a new package
/// and takes out the line of a package it depends on. Once the transaction is done the new
/// package needs it, so it is demoted and stays — never removed in the transaction that installs
/// what needs it (apt), nor left for a removal the package manager then refuses (pacman, whose
/// `Required By` resolves through what a package provides).
#[test]
fn e11_a_line_taken_out_that_a_new_declaration_needs_is_demoted() {
    for family in [Family::Pacman, Family::Apt] {
        let machine = match family {
            Family::Apt => debian_chosen()
                .with(Pkg::new("libfoo").depends(&["libc6"]))
                .offering(Pkg::new("app").depends(&["libfoo"])),
            Family::Pacman => arch_chosen()
                .with(Pkg::new("libfoo").provides(&["libfoo.so"]))
                .offering(Pkg::new("app").depends(&["libfoo.so"])),
        };
        let case = Case::new(&format!("e11-needed-{family:?}"), machine);
        assert!(case.import().status.success());
        assert!(
            case.manifest().contains("\"libfoo\""),
            "{}",
            case.manifest()
        );
        case.delete_line("libfoo");
        replace_in_manifest(&case, "common = [\n", "common = [\n  \"app\",\n");

        let plan = case.plan();
        assert!(plan.status.success(), "{family:?}: {}", story(&plan));
        assert!(
            err(&plan).contains("~ package libfoo (a dependency now: needed by app; stays)"),
            "{family:?}: {}",
            story(&plan)
        );
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{family:?}: {}", story(&apply));
        let installed = case.installed();
        assert!(
            installed.contains("app") && installed.contains("libfoo"),
            "{family:?}: {installed:?}\n{}",
            story(&apply)
        );
        assert!(!case.is_explicit("libfoo"), "{family:?}");
        let again = case.apply(&[]);
        assert!(again.status.success(), "{family:?}: {}", story(&again));
        assert!(nothing(&again), "{family:?}: {}", story(&again));
    }
}

// ------------------------------------------------------------- E10: what the documents say ---

/// The how-to page on packages says what `--overwrite-drift` removes under `exact`, and no user
/// page still promises that a package you installed yourself is never removed (LD-375; #711
/// moved the text from the host scope page).
#[test]
fn e10_the_documents_say_what_exact_and_overwrite_drift_do() {
    let read = |rel: &str| {
        let text =
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
                .unwrap_or_else(|e| panic!("{rel}: {e}"));
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let page = read("docs/guide/install-a-package.md");
    assert!(
        page.contains(
            "`--overwrite-drift` removes every package the preview lists as installed by hand"
        ),
        "the package how-to does not say what --overwrite-drift removes"
    );
    for rel in [
        "README.md",
        "docs/guide/install-a-package.md",
        "docs/guide/preview-a-change.md",
    ] {
        assert!(
            !read(rel).contains("It does not remove packages you installed yourself"),
            "{rel} still promises a package installed by hand is never removed"
        );
    }
}

/// The floor an import writes: 2.0's, `2.0.0`, once this crate is 2.0; a 1.x build keeps
/// 1.4.0, since a floor above its own version would refuse its own import (LD-528).
fn floor() -> &'static str {
    match env!("CARGO_PKG_VERSION_MAJOR") {
        "1" => "1.4.0",
        _ => "2.0.0",
    }
}
