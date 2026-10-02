//! sc-1: systemd services declared in `host.toml`. A `[services]` table names units, each
//! `"enabled"` or `"disabled"`; an apply enables and starts, or disables and stops, each one after
//! the packages and files; a declaration deleted since the last apply goes back to the
//! distribution's preset; a unit never declared is never touched.
//!
//! Every case is the real binary against a scratch `--root` with a **fake machine** behind it
//! (`tests/support/fakehost.rs`): its `systemctl` answers from, and changes, the units the case
//! gives it. Nothing reaches `/` and no real `systemctl` runs (`AGENTS.md` §8).

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use fakehost::{Case, Machine, Pkg, err, nothing, story};

/// A machine of each family the host scope runs on, with two units a manifest declares and two
/// it never names.
fn machines() -> Vec<(&'static str, Machine)> {
    [
        ("debian", Machine::debian()),
        ("arch", Machine::arch()),
        ("fedora", Machine::fedora()),
    ]
    .into_iter()
    .map(|(distro, machine)| {
        (
            distro,
            machine
                .unit("web.service", "disabled", false, "disabled")
                .unit("print.service", "enabled", true, "enabled")
                .unit("mine.service", "enabled", true, "disabled")
                .unit("spare.timer", "disabled", false, "enabled"),
        )
    })
    .collect()
}

fn manifest(distro: &str, services: &str) -> String {
    format!("[host]\nversion = \"1\"\ndistro = \"{distro}\"\n\n[services]\n{services}")
}

/// The `systemctl` calls that change something, from the fake's log of the last command.
fn changes(case: &Case) -> Vec<String> {
    case.log()
        .into_iter()
        .filter(|line| line.starts_with("systemctl") && !line.contains(" is-"))
        .collect()
}

/// How the plan names a command: `systemctl` pointed at this case's scratch root.
fn systemctl(case: &Case, args: &str) -> String {
    format!("systemctl --root={} {args}", case.root.dir.display())
}

#[test]
fn service_enable_and_disable_declared() {
    for (distro, machine) in machines() {
        let case = Case::new(&format!("svc-both-{distro}"), machine);
        case.set_manifest(&manifest(
            distro,
            "\"web.service\" = \"enabled\"\n\"print.service\" = \"disabled\"\n",
        ));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        assert_eq!(
            case.unit("web.service"),
            ("enabled".into(), true),
            "{distro}"
        );
        assert_eq!(
            case.unit("print.service"),
            ("disabled".into(), false),
            "{distro}"
        );
        assert_eq!(
            changes(&case),
            vec![
                systemctl(&case, "disable --now -- print.service"),
                systemctl(&case, "enable --now -- web.service"),
            ],
            "{distro}: {}",
            story(&apply)
        );
        assert_eq!(
            case.lock()["services"]["web.service"],
            "enabled",
            "{distro}"
        );
        assert_eq!(case.lock()["format"], "lodi-host-lock/6", "{distro}");

        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(changes(&case).is_empty(), "{distro}: {}", story(&again));
        let plan = case.plan();
        assert!(nothing(&plan), "{distro}: {}", story(&plan));
    }
}

#[test]
fn plan_lists_service_changes_and_changes_nothing() {
    let (distro, machine) = machines().remove(0);
    let case = Case::new("svc-plan", machine);
    case.set_manifest(&manifest(distro, "\"mine.service\" = \"enabled\"\n"));
    let first = case.apply(&[]);
    assert!(first.status.success(), "{}", story(&first));

    case.set_manifest(&manifest(
        distro,
        "\"web.service\" = \"enabled\"\n\"print.service\" = \"disabled\"\n",
    ));
    let before = case.machine();
    let lock = case.root.read("etc/lodi/host.lock");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    let text = err(&plan);
    for line in [
        format!(
            "~ service mine.service ({})\n",
            systemctl(&case, "preset -- mine.service")
        ),
        format!(
            "- service print.service ({})\n",
            systemctl(&case, "disable --now -- print.service")
        ),
        format!(
            "+ service web.service ({})\n",
            systemctl(&case, "enable --now -- web.service")
        ),
        "host: 3 services\n".to_string(),
    ] {
        assert!(text.contains(&line), "missing {line:?}\n{}", story(&plan));
    }
    assert!(changes(&case).is_empty(), "{}", story(&plan));
    assert_eq!(case.machine(), before, "the plan changed the machine");
    assert_eq!(case.root.read("etc/lodi/host.lock"), lock);
}

#[test]
fn service_preset_on_removed_declaration() {
    for (distro, machine) in machines() {
        let case = Case::new(&format!("svc-preset-{distro}"), machine);
        case.set_manifest(&manifest(
            distro,
            "\"web.service\" = \"enabled\"\n\"mine.service\" = \"disabled\"\n",
        ));
        let first = case.apply(&[]);
        assert!(first.status.success(), "{distro}: {}", story(&first));
        assert_eq!(case.unit("mine.service"), ("disabled".into(), false));

        case.set_manifest(&manifest(distro, "\"web.service\" = \"enabled\"\n"));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        assert_eq!(
            changes(&case),
            vec![systemctl(&case, "preset -- mine.service")],
            "{distro}"
        );
        // Its preset is disabled, and so it stays where the declaration left it.
        assert_eq!(case.unit("mine.service").0, "disabled", "{distro}");
        assert!(case.lock()["services"].get("mine.service").is_none());

        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(changes(&case).is_empty(), "{distro}: {}", story(&again));
    }
    // A unit whose preset is enabled goes back to enabled.
    let (distro, machine) = machines().remove(0);
    let case = Case::new("svc-preset-on", machine);
    case.set_manifest(&manifest(distro, "\"spare.timer\" = \"disabled\"\n"));
    assert!(case.apply(&[]).status.success());
    case.set_manifest(&manifest(distro, ""));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert_eq!(case.unit("spare.timer").0, "enabled");
}

#[test]
fn service_undeclared_units_untouched() {
    for (distro, machine) in machines() {
        let case = Case::new(&format!("svc-undeclared-{distro}"), machine);
        case.set_manifest(&manifest(distro, "\"web.service\" = \"enabled\"\n"));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        for (unit, state) in [
            ("print.service", ("enabled".to_string(), true)),
            ("mine.service", ("enabled".to_string(), true)),
            ("spare.timer", ("disabled".to_string(), false)),
        ] {
            assert_eq!(case.unit(unit), state, "{distro}: {unit}");
            assert!(
                case.log().iter().all(|line| !line.contains(unit)),
                "{distro}: lodi asked about {unit}: {:?}",
                case.log()
            );
        }
        // Without a `[services]` table lodi runs no systemctl at all.
        case.set_manifest(&format!("[host]\nversion = \"1\"\ndistro = \"{distro}\"\n"));
        std::fs::remove_file(case.root.path("etc/lodi/host.lock")).unwrap();
        let plain = case.apply(&[]);
        assert!(plain.status.success(), "{distro}: {}", story(&plain));
        assert!(
            case.log().iter().all(|line| !line.starts_with("systemctl")),
            "{distro}: {:?}",
            case.log()
        );
        assert!(
            !case.has_lock(),
            "{distro}: a host with nothing to do writes no record"
        );
    }
}

#[test]
fn an_unknown_unit_is_refused_before_any_change() {
    for (distro, machine) in machines() {
        let case = Case::new(&format!("svc-unknown-{distro}"), machine);
        case.set_manifest(&format!(
            "{}\n[files.\"/etc/lodi-example.conf\"]\ncontent = \"x\\n\"\n",
            manifest(
                distro,
                "\"web.service\" = \"enabled\"\n\"nosuch.service\" = \"enabled\"\n",
            )
        ));
        case.root.write("etc/group", "root:x:0:\n");
        let before = case.machine();
        for verb in ["plan", "apply"] {
            let refused = case.verb(verb, &[]);
            assert_eq!(
                refused.status.code(),
                Some(3),
                "{distro} {verb}: {}",
                story(&refused)
            );
            assert!(
                err(&refused).contains("E_UNKNOWN_UNIT")
                    && err(&refused).contains("nosuch.service"),
                "{distro} {verb}: {}",
                story(&refused)
            );
            assert!(changes(&case).is_empty(), "{distro} {verb}");
        }
        assert_eq!(case.machine(), before, "{distro}");
        assert!(!case.root.exists("etc/lodi-example.conf"), "{distro}");
        assert!(!case.has_lock(), "{distro}");
    }
}

/// A unit the same apply installs the package of is enabled after the transaction; one that no
/// installed package brought stops the apply before any service changes.
#[test]
fn a_unit_arrives_with_its_package_in_the_same_apply() {
    let machine = Machine::debian()
        .offering(Pkg::new("webd"))
        .unit_of("webd.service", "webd")
        .unit("print.service", "enabled", true, "enabled");
    let case = Case::new("svc-package", machine);
    case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"debian\"\n\n[packages]\ncommon = [\"webd\"]\n\n\
         [services]\n\"webd.service\" = \"enabled\"\n",
    );
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        err(&plan).contains(&format!(
            "+ service webd.service ({}; after the package transaction)\n",
            systemctl(&case, "enable --now -- webd.service")
        )),
        "{}",
        story(&plan)
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(case.installed().contains("webd"));
    assert_eq!(case.unit("webd.service"), ("enabled".into(), true));

    case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"debian\"\n\n[packages]\ncommon = [\"webd\", \"x\"]\n\n\
         [services]\n\"webd.service\" = \"enabled\"\n\"print.service\" = \"disabled\"\n\
         \"none.service\" = \"enabled\"\n",
    );
    case.edit(|m| m["available"]["x"] = serde_json::json!({"version": "1.0", "repo": "main"}));
    let refused = case.apply(&[]);
    assert_eq!(refused.status.code(), Some(3), "{}", story(&refused));
    assert!(
        err(&refused).contains("E_UNKNOWN_UNIT"),
        "{}",
        story(&refused)
    );
    assert_eq!(case.unit("print.service"), ("enabled".into(), true));
}

/// Package exact mode removes packages; it never enables, disables or presets a unit, not even
/// one the removed package shipped.
#[test]
fn package_exact_mode_never_touches_a_service() {
    let machine = Machine::debian()
        .with(Pkg::new("webd"))
        .unit_of("webd.service", "webd");
    let case = Case::new("svc-exact", machine);
    case.edit(|m| m["units"]["webd.service"]["state"] = "enabled".into());
    case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"debian\"\npackages = \"exact\"\n\n\
         [packages]\ncommon = [\"bash\", \"libc6\", \"linux-image-amd64\"]\n",
    );
    let apply = case.apply(&["--overwrite-drift"]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(!case.installed().contains("webd"), "{}", story(&apply));
    assert!(
        case.log().iter().all(|line| !line.starts_with("systemctl")),
        "{:?}",
        case.log()
    );
}

/// A re-import keeps the `[services]` the person wrote.
#[test]
fn a_reimport_keeps_the_services_table() {
    let (_, machine) = machines().remove(0);
    let case = Case::new("svc-reimport", machine);
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let written = format!(
        "{}\n[services]\n\"web.service\" = \"enabled\"\n",
        fakehost::without_snapshot(&case.manifest())
    );
    case.set_manifest(&written);
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    assert!(
        case.manifest()
            .contains("[services]\n\"web.service\" = \"enabled\"\n"),
        "{}",
        case.manifest()
    );
}
