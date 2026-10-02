//! sd-1: the OS basics declared in `host.toml`. `[system]` names the hostname, time zone, locale
//! and keymap; `[firewall]` is one nftables table lodi owns; `[network]` goes through the
//! machine's own network stack, staged: a change the machine cannot confirm in time is rolled
//! back by itself.
//!
//! Every case is the real binary against a scratch `--root` with a **fake machine** behind it
//! (`tests/support/fakehost.rs`): each basics tool is a shim that needs the `--root=` lodi passes
//! under a scratch root, and answers from, and changes, that root and the fake's state. Nothing
//! reaches `/` and no real tool runs (`AGENTS.md` §8).

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::os::unix::fs::symlink;

use fakehost::{Case, Machine, Pkg, err, nothing, story};
use serde_json::{Value, json};

/// The three network stacks a guest runs, each on the distribution that runs it there.
fn machines() -> Vec<(&'static str, &'static str, Machine)> {
    let networkd = |m: Machine| m.unit("systemd-networkd.service", "enabled", true, "enabled");
    vec![
        ("debian", "netplan", networkd(Machine::debian())),
        ("arch", "networkd", networkd(Machine::arch())),
        (
            "fedora",
            "networkmanager",
            Machine::fedora().unit("NetworkManager.service", "enabled", true, "enabled"),
        ),
    ]
}

/// A root as a fresh guest has it: a hostname, `UTC`, `C.UTF-8`, the `us` keymap, the zones the
/// cases name, a default route through `10.0.2.2`, and the stack's own configuration.
fn case(name: &str, stack: &str, machine: Machine) -> Case {
    let machine = machine
        .os("locale", json!(["LANG=C.UTF-8"]))
        .os("locales", json!(["C.UTF-8", "en_GB.UTF-8"]))
        .os("keymap", json!("us"));
    let case = Case::new(name, machine);
    case.root.write("etc/hostname", "lodi-test\n");
    for zone in ["UTC", "Europe/Berlin"] {
        case.root
            .write(&format!("usr/share/zoneinfo/{zone}"), "TZif2\n");
    }
    symlink("../usr/share/zoneinfo/UTC", case.root.path("etc/localtime")).expect("localtime");
    case.root.write(
        "proc/net/route",
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
         enp0s2\t00000000\t0202000A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n",
    );
    if stack == "netplan" {
        case.root.write(
            "etc/netplan/50-cloud-init.yaml",
            "network:\n  version: 2\n  ethernets:\n    enp0s2:\n      dhcp4: true\n",
        );
    }
    case
}

fn manifest(distro: &str, body: &str) -> String {
    format!("[host]\nversion = \"1\"\ndistro = \"{distro}\"\n\n{body}")
}

const BASICS: &str = "[system]\nhostname = \"box\"\ntimezone = \"Europe/Berlin\"\n\
                      locale = \"en_GB.UTF-8\"\nkeymap = \"de\"\n";

/// The calls of the last command that change something: every basics call but a read.
fn changes(case: &Case) -> Vec<String> {
    const TOOLS: [&str; 8] = [
        "hostnamectl",
        "timedatectl",
        "localectl",
        "nft",
        "netplan",
        "networkctl",
        "nmcli",
        "systemd-run",
    ];
    case.log()
        .into_iter()
        .filter(|line| {
            let mut words = line.split(' ');
            let program = words.next().unwrap_or("");
            let verb = words.find(|w| !w.starts_with("--root=")).unwrap_or("");
            TOOLS.contains(&program) && !matches!(verb, "status" | "list-locales" | "list")
        })
        .collect()
}

fn hostname(case: &Case) -> String {
    case.root.read("etc/hostname").trim().to_string()
}

fn timezone(case: &Case) -> String {
    std::fs::read_link(case.root.path("etc/localtime"))
        .expect("etc/localtime is a link")
        .display()
        .to_string()
        .rsplit("zoneinfo/")
        .next()
        .unwrap()
        .to_string()
}

fn os(case: &Case, key: &str) -> Value {
    case.machine()["os"][key].clone()
}

fn tables(case: &Case) -> serde_json::Map<String, Value> {
    os(case, "nft").as_object().cloned().unwrap_or_default()
}

#[test]
fn basics_apply_and_revert() {
    for (distro, stack, machine) in machines() {
        let case = case(&format!("os-basics-{distro}"), stack, machine);
        let before = manifest(distro, "");
        case.set_manifest(&before);
        let first = case.apply(&[]);
        assert!(first.status.success(), "{distro}: {}", story(&first));

        // Without a hostname line, the hostname is never touched.
        let without = BASICS.replace("hostname = \"box\"\n", "");
        case.set_manifest(&manifest(distro, &without));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        assert_eq!(hostname(&case), "lodi-test", "{distro}");
        assert!(
            !case
                .log()
                .iter()
                .any(|line| line.starts_with("hostnamectl")),
            "{distro}: {:?}",
            case.log()
        );
        assert_eq!(timezone(&case), "Europe/Berlin", "{distro}");
        assert_eq!(os(&case, "locale"), json!(["LANG=en_GB.UTF-8"]), "{distro}");
        assert_eq!(os(&case, "keymap"), json!("de"), "{distro}");

        case.set_manifest(&manifest(distro, BASICS));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        assert_eq!(hostname(&case), "box", "{distro}");
        assert_eq!(case.lock()["format"], "lodi-host-lock/6", "{distro}");
        assert_eq!(
            case.lock()["basics"]["hostname"]["value"],
            "box",
            "{distro}"
        );
        assert_eq!(
            case.lock()["basics"]["hostname"]["was"],
            "lodi-test",
            "{distro}"
        );
        assert_eq!(case.lock()["basics"]["timezone"]["was"], "UTC", "{distro}");

        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(changes(&case).is_empty(), "{distro}: {}", story(&again));
        let plan = case.plan();
        assert!(nothing(&plan), "{distro}: {}", story(&plan));
        // An unchanged basic is not a plan line, as an unchanged package is not (#585).
        assert!(
            !err(&plan).contains("= hostname"),
            "{distro}: {}",
            story(&plan)
        );

        // The revert: the file as it was, and every setting as lodi found it.
        case.set_manifest(&before);
        let revert = case.apply(&[]);
        assert!(revert.status.success(), "{distro}: {}", story(&revert));
        assert_eq!(hostname(&case), "lodi-test", "{distro}");
        assert_eq!(timezone(&case), "UTC", "{distro}");
        assert_eq!(os(&case, "locale"), json!(["LANG=C.UTF-8"]), "{distro}");
        assert_eq!(os(&case, "keymap"), json!("us"), "{distro}");
        assert!(
            case.lock().get("basics").is_none(),
            "{distro}: {}",
            case.lock()
        );
        assert_eq!(case.lock()["format"], "lodi-host-lock/6", "{distro}");
    }
}

#[test]
fn nft_owned_table_only() {
    let others = [
        (
            "inet filter",
            "table inet filter {\n\tchain input {\n\t\ttype filter hook input priority filter; \
             policy accept;\n\t\ttcp dport 8080 drop\n\t}\n}",
        ),
        ("ip nat", "table ip nat {\n}"),
    ];
    for (distro, stack, machine) in machines() {
        let machine = machine.os(
            "nft",
            others
                .iter()
                .map(|(name, listing)| (name.to_string(), json!(listing)))
                .collect::<serde_json::Map<_, _>>()
                .into(),
        );
        let case = case(&format!("os-nft-{distro}"), stack, machine);
        case.set_manifest(&manifest(
            distro,
            "[firewall]\nallow = [\"22/tcp\", \"8000-8100/udp\"]\n",
        ));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        let after = tables(&case);
        assert_eq!(
            after.keys().collect::<Vec<_>>(),
            ["inet filter", "inet lodi", "ip nat"],
            "{distro}"
        );
        for (name, listing) in others {
            assert_eq!(after[name], listing, "{distro}: {name} changed");
        }
        let lodi = after["inet lodi"].as_str().unwrap();
        for rule in [
            "policy drop;",
            "tcp dport 22 accept",
            "udp dport 8000-8100 accept",
        ] {
            assert!(lodi.contains(rule), "{distro}: {lodi}");
        }
        let script = case.root.read("etc/lodi/firewall.nft");
        assert!(!script.contains("flush"), "{distro}: {script}");
        for line in script.lines().filter(|line| line.starts_with(['t', 'd'])) {
            assert!(line.contains(" inet lodi"), "{distro}: {line}");
        }
        assert!(
            case.root.exists("etc/systemd/system/lodi-firewall.service"),
            "{distro}"
        );
        assert_eq!(
            case.unit("lodi-firewall.service").0,
            "enabled",
            "{distro}: the table comes back at boot"
        );
        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(changes(&case).is_empty(), "{distro}: {}", story(&again));

        case.set_manifest(&manifest(distro, ""));
        let removed = case.apply(&[]);
        assert!(removed.status.success(), "{distro}: {}", story(&removed));
        let after = tables(&case);
        assert_eq!(
            after.keys().collect::<Vec<_>>(),
            ["inet filter", "ip nat"],
            "{distro}"
        );
        for (name, listing) in others {
            assert_eq!(after[name], listing, "{distro}: {name} changed");
        }
        assert!(!case.root.exists("etc/lodi/firewall.nft"), "{distro}");
        assert!(
            !case.root.exists("etc/systemd/system/lodi-firewall.service"),
            "{distro}"
        );
    }
}

#[test]
fn firewall_refuses_active_ufw_or_firewalld() {
    for unit in ["ufw.service", "firewalld.service"] {
        for (distro, stack, machine) in machines() {
            let machine = machine.unit(unit, "enabled", true, "enabled");
            let case = case(&format!("os-ufw-{distro}-{unit}"), stack, machine);
            case.set_manifest(&manifest(
                distro,
                &format!("{BASICS}\n[firewall]\nallow = [\"22/tcp\"]\n"),
            ));
            // Ubuntu runs ufw's unit whether ufw is on or not: its own file says which.
            if unit == "ufw.service" {
                case.root.write("etc/ufw/ufw.conf", "ENABLED=no\n");
                let plan = case.plan();
                assert!(plan.status.success(), "{distro}: {}", story(&plan));
                case.root.write("etc/ufw/ufw.conf", "# ufw\nENABLED=yes\n");
            }
            let before = case.machine();
            let apply = case.apply(&[]);
            assert_eq!(apply.status.code(), Some(7), "{distro}: {}", story(&apply));
            assert!(
                err(&apply).contains("E_FIREWALL_CONFLICT") && err(&apply).contains(unit),
                "{distro}: {}",
                story(&apply)
            );
            assert!(changes(&case).is_empty(), "{distro}: {:?}", case.log());
            assert_eq!(case.machine(), before, "{distro}");
            assert_eq!(hostname(&case), "lodi-test", "{distro}");
            assert_eq!(timezone(&case), "UTC", "{distro}");
            assert!(!case.has_lock(), "{distro}");
        }
    }
}

/// Where each stack's lodi file for `enp0s2` lives below the root.
fn network_file(stack: &str) -> &'static str {
    match stack {
        "netplan" => "etc/netplan/90-lodi.yaml",
        "networkd" => "etc/systemd/network/00-lodi-enp0s2.network",
        _ => "etc/NetworkManager/system-connections/lodi-enp0s2.nmconnection",
    }
}

const CUT: &str = "[network]\nconfirm_within = 2\n\n[network.interfaces.enp0s2]\n\
                   dhcp = false\naddresses = [\"192.168.77.10/24\"]\ngateway = \"192.168.77.1\"\n";
const FINE: &str = "[network]\nconfirm_within = 2\n\n[network.interfaces.enp0s2]\n\
                    dhcp = true\naddresses = [\"10.0.2.50/24\"]\n";

#[test]
fn network_timeout_restores_live_config() {
    for (distro, stack, machine) in machines() {
        let machine = machine.os("cut", json!("192.168.77.10"));
        let case = case(&format!("os-net-{distro}"), stack, machine);
        let file = network_file(stack);

        // From nothing: the cut is rolled back to no lodi file at all.
        case.set_manifest(&manifest(distro, CUT));
        let cut = case.apply(&[]);
        assert_eq!(cut.status.code(), Some(8), "{distro}: {}", story(&cut));
        assert!(
            err(&cut).contains("E_NETWORK_ROLLED_BACK")
                && err(&cut).contains("the earlier network configuration is back"),
            "{distro}: {}",
            story(&cut)
        );
        assert!(!case.root.exists(file), "{distro}: {}", story(&cut));
        assert!(
            os(&case, "timers").as_object().is_none_or(|t| t.is_empty()),
            "{distro}"
        );
        assert!(
            case.log().iter().any(|line| line.starts_with("systemd-run")
                && line.contains("--unit=lodi-network-rollback")),
            "{distro}: the rollback does not depend on lodi surviving: {:?}",
            case.log()
        );

        // A change the machine confirms stays, and is recorded.
        case.set_manifest(&manifest(distro, FINE));
        let fine = case.apply(&[]);
        assert!(fine.status.success(), "{distro}: {}", story(&fine));
        assert!(
            !err(&fine).contains("is outstanding"),
            "{distro}: the rollback closed its journal: {}",
            story(&fine)
        );
        let kept = case.root.read(file);
        assert!(kept.contains("10.0.2.50/24"), "{distro}: {kept}");
        assert_eq!(
            case.lock()["basics"]["network"]["value"],
            "enp0s2",
            "{distro}"
        );
        let lock = case.root.read("etc/lodi/host.lock");

        // A cut from there puts that configuration back, and the record stays as it was.
        case.set_manifest(&manifest(distro, CUT));
        let cut = case.apply(&[]);
        assert_eq!(cut.status.code(), Some(8), "{distro}: {}", story(&cut));
        assert!(
            err(&cut).contains("192.168.77.1"),
            "{distro}: {}",
            story(&cut)
        );
        assert_eq!(case.root.read(file), kept, "{distro}");
        assert_eq!(case.root.read("etc/lodi/host.lock"), lock, "{distro}");
        let activations = changes(&case)
            .into_iter()
            .filter(|line| !line.starts_with("systemd-run"))
            .count();
        assert!(activations >= 2, "{distro}: {:?}", case.log());
    }
}

#[test]
fn plan_lists_basics_and_changes_nothing() {
    for (distro, stack, machine) in machines() {
        let case = case(&format!("os-plan-{distro}"), stack, machine);
        case.set_manifest(&manifest(
            distro,
            &format!("{BASICS}\n[firewall]\nallow = [\"22/tcp\"]\n\n{FINE}"),
        ));
        let before = case.machine();
        let plan = case.plan();
        assert!(plan.status.success(), "{distro}: {}", story(&plan));
        let text = err(&plan);
        for line in [
            "~ hostname lodi-test -> box (hostnamectl --root=",
            "~ timezone UTC -> Europe/Berlin (timedatectl --root=",
            "~ locale LANG=C.UTF-8 -> LANG=en_GB.UTF-8 (localectl --root=",
            "~ keymap us -> de (localectl --root=",
            "+ firewall table inet lodi (",
            "+ network enp0s2 (",
        ] {
            assert!(text.contains(line), "{distro}: {line:?} in\n{text}");
        }
        assert!(text.contains("host: 6 other changes\n"), "{distro}: {text}");
        assert!(changes(&case).is_empty(), "{distro}: {:?}", case.log());
        assert_eq!(case.machine(), before, "{distro}");
        assert_eq!(hostname(&case), "lodi-test", "{distro}");
        assert_eq!(timezone(&case), "UTC", "{distro}");
        assert!(!case.root.exists(network_file(stack)), "{distro}");
        assert!(!case.root.exists("etc/lodi/firewall.nft"), "{distro}");
        assert!(!case.has_lock(), "{distro}");
    }
}

#[test]
fn an_unknown_zone_or_locale_is_refused_before_any_change() {
    let (distro, stack, machine) = machines().remove(0);
    let case = case("os-unknown", stack, machine);
    case.set_manifest(&manifest(
        distro,
        "[system]\nhostname = \"box\"\ntimezone = \"Mars/Olympus\"\nlocale = \"xx_YY.UTF-8\"\n",
    ));
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(3), "{}", story(&apply));
    for word in ["E_UNKNOWN_SETTING", "Mars/Olympus", "xx_YY.UTF-8"] {
        assert!(err(&apply).contains(word), "{word}: {}", story(&apply));
    }
    assert!(changes(&case).is_empty(), "{:?}", case.log());
    assert_eq!(hostname(&case), "lodi-test");
}

/// A Debian root whose localed reports `locale` and lists `locales`, and nothing else declared.
fn locale_case(name: &str, locale: Value, locales: Value) -> (&'static str, Case) {
    let (distro, _, machine) = machines().remove(0);
    let machine = machine
        .os("locale", locale)
        .os("locales", locales)
        .os("keymap", json!("us"));
    let case = Case::new(name, machine);
    case.root.write("etc/hostname", "lodi-test\n");
    (distro, case)
}

#[test]
fn a_locale_localed_does_not_report_is_read_from_the_file_the_import_reads() {
    // Debian, Ubuntu and Arch guests: `localectl status` names no LANG and lists no locale, but
    // /etc/default/locale has it. The locale the import wrote back is never a change.
    let (distro, case) = locale_case("os-locale-file", json!([]), json!([]));
    case.root.write("etc/default/locale", "LANG=C.UTF-8\n");
    case.set_manifest(&manifest(distro, "[system]\nlocale = \"C.UTF-8\"\n"));
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    let text = err(&plan);
    assert!(!text.contains("~ locale"), "{text}");
    assert!(
        !err(&plan).contains("E_UNKNOWN_SETTING"),
        "{}",
        story(&plan)
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(changes(&case).is_empty(), "{:?}", case.log());
}

#[test]
fn a_locale_listed_with_another_codeset_spelling_is_known() {
    // glibc lists C.UTF-8 as `C.utf8`: the same locale.
    let (distro, case) = locale_case(
        "os-locale-spelling",
        json!(["LANG=en_GB.UTF-8"]),
        json!(["C.utf8", "en_GB.utf8"]),
    );
    case.set_manifest(&manifest(distro, "[system]\nlocale = \"C.UTF-8\"\n"));
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        err(&plan).contains("~ locale LANG=en_GB.UTF-8 -> LANG=C.UTF-8 (localectl --root="),
        "{}",
        story(&plan)
    );
}

#[test]
fn package_exact_mode_keeps_the_network_stack_and_firewall_tools() {
    let (distro, stack, machine) = machines().remove(0);
    let machine = machine
        .with(Pkg::new("nftables"))
        .with(Pkg::new("netplan.io"))
        .with(Pkg::new("htop"));
    let case = case("os-exact", stack, machine);
    case.set_manifest(&format!(
        "[host]\nversion = \"1\"\ndistro = \"{distro}\"\npackages = \"exact\"\n\n\
         [packages]\ncommon = [\"bash\"]\n\n[firewall]\nallow = [\"22/tcp\"]\n"
    ));
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    let text = err(&plan);
    assert!(
        text.contains("htop"),
        "exact mode still removes the rest: {text}"
    );
    for name in ["nftables", "netplan.io"] {
        assert!(
            !text
                .lines()
                .any(|line| line.starts_with('-') && line.contains(name)),
            "{name}: {text}"
        );
    }
}

#[test]
fn a_reimport_keeps_the_basics_tables() {
    let (_, stack, machine) = machines().remove(0);
    let case = case("os-reimport", stack, machine);
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let tables = format!("{BASICS}\n[firewall]\nallow = [\"22/tcp\"]\n");
    // The import writes the machine's own [system]; the owner's replaces it.
    let imported = fakehost::without_snapshot(&case.manifest());
    let start = imported
        .find("[system]\n")
        .expect("the import writes [system]");
    let end = start + imported[start..].find("\n\n").expect("its end") + 2;
    case.set_manifest(&format!(
        "{}{}\n{tables}",
        &imported[..start],
        &imported[end..]
    ));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    assert!(case.manifest().contains(&tables), "{}", case.manifest());
}

#[test]
fn a_keymap_kept_in_etc_default_keyboard_is_written_there() {
    // Ubuntu's localed keeps the layout in /etc/default/keyboard and refuses set-x11-keymap.
    let (distro, stack, machine) = machines().remove(0);
    let case = case("os-keyboard", stack, machine);
    let keyboard = "XKBMODEL=\"pc105\"\nXKBLAYOUT=\"us\"\nXKBVARIANT=\"\"\n";
    case.root.write("etc/default/keyboard", keyboard);
    case.set_manifest(&manifest(distro, "[system]\nkeymap = \"de\"\n"));
    let plan = case.plan();
    assert!(
        err(&plan).contains("~ keymap us -> de (XKBLAYOUT in /etc/default/keyboard)"),
        "{}",
        story(&plan)
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert_eq!(
        case.root.read("etc/default/keyboard"),
        keyboard.replace("\"us\"", "\"de\"")
    );
    assert!(changes(&case).is_empty(), "{:?}", case.log());
    let again = case.plan();
    assert!(nothing(&again), "{}", story(&again));

    case.set_manifest(&manifest(distro, ""));
    let revert = case.apply(&[]);
    assert!(revert.status.success(), "{}", story(&revert));
    assert_eq!(case.root.read("etc/default/keyboard"), keyboard);
}

#[test]
fn a_keymap_the_keyboard_file_holds_is_read_from_it_when_localed_reports_none() {
    // The Debian and Ubuntu guests' `localectl status` says `X11 Layout: (unset)` while
    // /etc/default/keyboard names the layout the import wrote back: that is never a change, and
    // an apply does not rewrite it on every run (#585).
    let (distro, stack, machine) = machines().remove(0);
    let machine = machine.os("localed_ignores_keyboard", json!(true));
    let case = case("os-keyboard-unread", stack, machine);
    let keyboard = "XKBMODEL=\"pc105\"\nXKBLAYOUT=\"us\"\n";
    case.root.write("etc/default/keyboard", keyboard);
    case.set_manifest(&manifest(distro, "[system]\nkeymap = \"us\"\n"));
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(!err(&plan).contains("keymap"), "{}", story(&plan));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(!err(&apply).contains("keymap"), "{}", story(&apply));
    assert_eq!(case.root.read("etc/default/keyboard"), keyboard);
    let again = case.plan();
    assert!(nothing(&again), "{}", story(&again));
}
