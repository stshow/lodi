//! The host manifest, the lock and the plan (M-0.6 T-1).
//!
//! `spec/01` §5 key by key, every rejection with its code, its exit status and its hint, the
//! lock's round trip and its drift digests, and the plan's printed grammar. Nothing here runs a
//! package manager, and nothing here touches anything outside a scratch root the test owns.

#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::collections::BTreeMap;

use lodi::diag::exit_status;
use lodi::hostscope::lock::{FileRecord, HostLock, PackageRecord};
use lodi::hostscope::manifest::{Context, FileState, HostManifest, OnRemove, parse as parse_host};
use lodi::manifest::ManifestErrors;

use hostroot::{Root, ids};

fn context() -> Context {
    Context {
        arch: "x86_64".into(),
        os: "linux".into(),
        distro: "debian".into(),
        release: "12".into(),
        codename: "bookworm".into(),
        lodi_version: env!("CARGO_PKG_VERSION").into(),
    }
}

fn parse(text: &str) -> HostManifest {
    match parse_host(text, "host.toml", &context()) {
        Ok(manifest) => manifest,
        Err(errors) => panic!("expected a valid manifest, got:\n{errors}"),
    }
}

fn errors(text: &str) -> ManifestErrors {
    match parse_host(text, "host.toml", &context()) {
        Ok(_) => panic!("expected a rejection, the manifest parsed"),
        Err(errors) => errors,
    }
}

/// Every rejection carries its code, the exit status `spec/11` gives that code, and a hint. A
/// diagnostic without a hint is a diagnostic that tells an operator nothing to do.
#[track_caller]
fn rejects(text: &str, code: &'static str, status: u8) -> ManifestErrors {
    let errors = errors(text);
    assert!(
        errors.codes().contains(&code),
        "expected {code}, got {:?}:\n{errors}",
        errors.codes()
    );
    let found = errors
        .diagnostics
        .iter()
        .find(|d| d.code == code)
        .expect("the diagnostic");
    assert_eq!(exit_status(found.code), status, "exit status of {code}");
    // Every rejection this build composes carries a hint. A syntax error is the exception:
    // it is the TOML parser's own message with the parser's own span, and inventing advice
    // for it would be inventing advice.
    assert!(
        code == "E_SYNTAX" || found.notes.iter().any(|note| note.starts_with("hint: ")),
        "{code} carries no hint:\n{found}"
    );
    errors
}

// ---------------------------------------------------------------- spec/01 §5.1, the [host] table

#[test]
fn every_host_key_and_its_default() {
    let empty = parse("");
    assert_eq!(empty.host.version, "1");
    assert_eq!(empty.host.distro, None);
    assert!(empty.host.auto_remove, "auto_remove defaults to true");
    assert_eq!(empty.host.min_lodi_version, "");
    assert!(empty.packages.is_empty());
    assert!(empty.files.is_empty());
    assert!(empty.vars.is_empty());

    let full = parse(
        "[host]\nversion = \"1\"\ndistro = \"debian\"\nauto_remove = false\n\
         min_lodi_version = \">=0.1\"\n",
    );
    assert_eq!(full.host.version, "1");
    assert_eq!(full.host.distro.as_deref(), Some("debian"));
    assert!(!full.host.auto_remove);
    assert_eq!(full.host.min_lodi_version, ">=0.1");
}

#[test]
fn a_host_schema_version_this_build_does_not_read_is_refused() {
    rejects("[host]\nversion = \"2\"\n", "E_UNSUPPORTED", 3);
}

#[test]
fn a_min_lodi_version_this_build_does_not_satisfy_is_refused() {
    rejects("[host]\nmin_lodi_version = \">=99.0.0\"\n", "E_VERSION", 3);
    rejects("[host]\nmin_lodi_version = \"nonsense\"\n", "E_TYPE", 3);
}

#[test]
fn an_unknown_key_is_named_and_a_near_miss_is_suggested() {
    let errors = rejects("[host]\nauto_remve = true\n", "E_UNKNOWN_ATTR", 3);
    assert!(
        errors.to_string().contains("did you mean `auto_remove`?"),
        "{errors}"
    );
    let at_root = rejects("[hostt]\n", "E_UNKNOWN_BLOCK", 3);
    assert!(
        at_root.to_string().contains("did you mean `host`?"),
        "{at_root}"
    );
}

#[test]
fn a_wrong_type_is_a_type_error_with_a_location() {
    let errors = rejects("[host]\nauto_remove = \"yes\"\n", "E_TYPE", 3);
    let found = errors
        .diagnostics
        .iter()
        .find(|d| d.code == "E_TYPE")
        .unwrap();
    let at = found.location.as_ref().expect("a location");
    assert_eq!((at.line, at.column), (2, 15));
}

// ------------------------------------------------------- spec/01 §2.2, [vars] and substitution

#[test]
fn vars_and_the_host_names_substitute_and_a_missing_one_is_refused() {
    let manifest = parse(
        "[vars]\nwho = \"team\"\n\n[files.\"/etc/x.conf\"]\n\
         content = \"${vars.who} on ${host.distro} ${host.release} (${host.codename}) \
         ${host.arch} ${host.os} lodi ${lodi.version}\"\n",
    );
    let entry = manifest.files.get("/etc/x.conf").expect("the entry");
    assert_eq!(
        entry.content.as_deref(),
        Some(
            format!(
                "team on debian 12 (bookworm) x86_64 linux lodi {}",
                env!("CARGO_PKG_VERSION")
            )
            .as_str()
        )
    );
    rejects(
        "[files.\"/etc/x.conf\"]\ncontent = \"${vars.absent}\"\n",
        "E_VAR_UNSET",
        3,
    );
    rejects(
        "[files.\"/etc/x.conf\"]\ncontent = \"${project.name}\"",
        "E_UNSUPPORTED",
        3,
    );
    rejects(
        "[files.\"/etc/x.conf\"]\ncontent = \"${1 + 1}\"",
        "E_EXCLUDED_CONSTRUCT",
        3,
    );
}

#[test]
fn a_literal_dollar_brace_is_written_with_a_doubled_dollar() {
    let manifest = parse("[files.\"/etc/x.conf\"]\ncontent = \"$${not.a.name} and $PATH\"\n");
    assert_eq!(
        manifest.files["/etc/x.conf"].content.as_deref(),
        Some("${not.a.name} and $PATH")
    );
}

// ------------------------------------------------------------- spec/01 §5.2, the package tables

#[test]
fn the_package_tables_merge_as_spec_01_section_6_2_says() {
    let manifest = parse(
        "[packages]\ncommon = [\"git\", \"curl\", \"git\"]\nhold = [\"git\"]\n\
         mark_auto = [\"git\"]\noptional = [\"ripgrep\"]\nabsent = [\"nano\"]\n\n\
         [packages.debian]\nadd = [\"build-essential\"]\nremove = [\"curl\"]\n\n\
         [packages.arch]\nadd = [\"base-devel\"]\n\n\
         [packages.x86_64]\nadd = [\"ripgrep\"]\n",
    );
    assert_eq!(
        manifest.packages.effective("debian", "x86_64"),
        vec!["git", "build-essential", "ripgrep"]
    );
    assert_eq!(
        manifest.packages.effective("arch", "aarch64"),
        vec!["git", "curl", "base-devel"]
    );
    assert_eq!(manifest.packages.hold, vec!["git"]);
    assert_eq!(manifest.packages.absent, vec!["nano"]);
}

#[test]
fn a_per_distro_table_may_carry_the_lists_and_they_union_with_the_outer_ones() {
    let manifest = parse(
        "[packages]\ncommon = [\"git\"]\nhold = [\"git\"]\n\n\
         [packages.debian]\nadd = [\"curl\"]\nhold = [\"curl\"]\nmark_auto = [\"curl\"]\n\
         optional = [\"curl\"]\nabsent = [\"nano\"]\n",
    );
    assert_eq!(manifest.packages.hold, vec!["git", "curl"]);
    assert_eq!(manifest.packages.mark_auto, vec!["curl"]);
    assert_eq!(manifest.packages.absent, vec!["nano"]);
}

#[test]
fn a_version_qualified_package_name_is_refused_rather_than_ignored() {
    rejects(
        "[packages]\ncommon = [\"git=1:2.39.2\"]\n",
        "E_UNSUPPORTED",
        3,
    );
    rejects("[packages]\ncommon = [\"-git\"]\n", "E_IDENT", 3);
}

#[test]
fn a_hold_outside_the_effective_list_is_an_unknown_package() {
    rejects(
        "[packages]\ncommon = [\"git\"]\nhold = [\"curl\"]\n",
        "E_UNKNOWN_PACKAGE",
        3,
    );
    rejects(
        "[packages]\ncommon = [\"git\"]\n\n[packages.debian]\nremove = [\"curl\"]\n",
        "E_UNKNOWN_PACKAGE",
        3,
    );
}

// -------------------------------------------------------------- spec/01 §5.3, the [files] tables

#[test]
fn every_file_key_and_its_default() {
    let manifest = parse("[files.\"/etc/x.conf\"]\ncontent = \"hello\\n\"\n");
    let entry = &manifest.files["/etc/x.conf"];
    assert_eq!(entry.path, "/etc/x.conf");
    assert_eq!(entry.content.as_deref(), Some("hello\n"));
    assert_eq!(entry.source, None);
    assert_eq!(entry.mode, 0o644);
    assert_eq!(entry.state, FileState::Present);
    assert!(entry.backup, "backup defaults to true");
    assert_eq!(entry.on_remove, OnRemove::Restore);
    assert_eq!(entry.owner, "root");
    assert_eq!(entry.group, "root");

    let full = parse(
        "[files.\"/etc/y.conf\"]\nsource = \"payload/y.conf\"\nmode = \"0600\"\n\
         state = \"present\"\nbackup = false\non_remove = \"delete\"\nowner = \"nobody\"\n\
         group = \"nogroup\"\n",
    );
    let entry = &full.files["/etc/y.conf"];
    assert_eq!(entry.source.as_deref(), Some("payload/y.conf"));
    assert_eq!(entry.mode, 0o600);
    assert!(!entry.backup);
    assert_eq!(entry.on_remove, OnRemove::Delete);
    assert_eq!(entry.owner, "nobody");
    assert_eq!(entry.group, "nogroup");
}

#[test]
fn an_absent_file_needs_neither_content_nor_source() {
    let manifest = parse("[files.\"/etc/x.conf\"]\nstate = \"absent\"\n");
    assert_eq!(manifest.files["/etc/x.conf"].state, FileState::Absent);
}

#[test]
fn the_file_value_rejections_each_name_what_is_allowed() {
    rejects(
        "[files.\"/etc/x.conf\"]\ncontent = \"a\"\nsource = \"b\"\n",
        "E_ATTR_CONFLICT",
        3,
    );
    rejects("[files.\"/etc/x.conf\"]\n", "E_ATTR_CONFLICT", 3);
    rejects(
        "[files.\"/etc/x.conf\"]\ncontent = \"a\"\nmode = \"rwxr-xr-x\"\n",
        "E_TYPE",
        3,
    );
    rejects(
        "[files.\"/etc/x.conf\"]\ncontent = \"a\"\nstate = \"gone\"\n",
        "E_TYPE",
        3,
    );
    rejects(
        "[files.\"/etc/x.conf\"]\ncontent = \"a\"\non_remove = \"revert\"\n",
        "E_TYPE",
        3,
    );
    rejects(
        "[files.\"/etc/x.conf\"]\ncontent = \"a\"\nmoed = \"0644\"\n",
        "E_UNKNOWN_ATTR",
        3,
    );
}

#[test]
fn a_relative_key_an_escaping_key_and_an_escaping_source_are_path_escapes() {
    rejects(
        "[files.\"etc/x.conf\"]\ncontent = \"a\"\n",
        "E_PATH_ESCAPE",
        3,
    );
    rejects(
        "[files.\"/etc/../../x.conf\"]\ncontent = \"a\"\n",
        "E_PATH_ESCAPE",
        3,
    );
    rejects(
        "[files.\"/etc/x.conf\"]\nsource = \"../outside\"\n",
        "E_PATH_ESCAPE",
        3,
    );
    rejects(
        "[files.\"/etc/x.conf\"]\nsource = \"/etc/passwd\"\n",
        "E_PATH_ESCAPE",
        3,
    );
}

#[test]
fn two_keys_that_name_the_same_path_are_a_duplicate_resource() {
    rejects(
        "[files.\"/etc/x.conf\"]\ncontent = \"a\"\n\n\
         [files.\"/etc/./sub/../x.conf\"]\ncontent = \"b\"\n",
        "E_DUP_RESOURCE",
        3,
    );
}

// ------------------------------------------------------------ what this scope refuses outright

#[test]
fn inputs_and_modules_are_refused_and_name_the_release_that_may_add_them() {
    let errors = rejects("[inputs]\n", "E_UNSUPPORTED", 3);
    assert!(errors.to_string().contains("1.0"), "{errors}");
    rejects("[modules]\n", "E_UNSUPPORTED", 3);
}

#[test]
fn every_deferred_resource_table_is_an_error_and_never_a_silent_default() {
    for table in ["sysctls", "hostname", "snapshots", "tools"] {
        let errors = rejects(&format!("[{table}]\n"), "E_UNSUPPORTED", 3);
        let text = errors.to_string();
        assert!(text.contains("remove the table"), "{table}: {errors}");
        assert!(!text.contains("ADR-"), "{table}: {errors}");
    }
}

/// sc-1: `[services]` names each unit in full, `enabled` or `disabled`, and nothing else.
#[test]
fn a_services_table_takes_unit_names_and_two_states() {
    let manifest =
        parse("[services]\n\"ssh.service\" = \"enabled\"\n\"fstrim.timer\" = \"disabled\"\n");
    assert_eq!(manifest.services.get("ssh.service"), Some(&true));
    assert_eq!(manifest.services.get("fstrim.timer"), Some(&false));
    for bad in [
        "ssh = \"enabled\"",
        "\"-x.service\" = \"enabled\"",
        "\"a/b.service\" = \"enabled\"",
        "\"ssh.service\" = \"running\"",
        "\"ssh.service\" = true",
    ] {
        rejects(&format!("[services]\n{bad}\n"), "E_TYPE", 3);
    }
}

/// sd-1: `[system]`, `[firewall]` and `[network]` take what their tools take, and nothing else.
#[test]
fn the_basics_tables_take_their_grammar() {
    let manifest = parse(
        "[system]\nhostname = \"box\"\ntimezone = \"Europe/Berlin\"\nlocale = \"en_GB.UTF-8\"\n\
         keymap = \"de\"\n\n[firewall]\nallow = [\"22/tcp\", \"8000-8100/udp\"]\n\n\
         [network]\nconfirm_within = 30\n\n[network.interfaces.enp0s2]\ndhcp = false\n\
         addresses = [\"10.0.2.50/24\"]\ngateway = \"10.0.2.2\"\ndns = [\"10.0.2.3\"]\n",
    );
    assert_eq!(manifest.system.hostname.as_deref(), Some("box"));
    let firewall = manifest.firewall.expect("a firewall");
    assert!(firewall.drop);
    assert_eq!(
        firewall.allow[1],
        ("udp".to_string(), "8000-8100".to_string())
    );
    let network = manifest.network.expect("a network");
    assert_eq!(network.confirm_within, 30);
    assert_eq!(network.interfaces["enp0s2"].dns, ["10.0.2.3"]);
    for bad in [
        "[system]\nhostname = \"-x\"\n",
        "[system]\ntimezone = \"../etc/passwd\"\n",
        "[system]\nlocale = \"en US\"\n",
        "[system]\nkeymap = \"de;us\"\n",
        "[firewall]\nallow = [\"22\"]\n",
        "[firewall]\nallow = [\"9-8/tcp\"]\n",
        "[firewall]\ninput = \"reject\"\n",
        "[network]\nconfirm_within = 0\n",
        "[network.interfaces.\"-e\"]\n",
        "[network.interfaces.e0]\ndhcp = false\n",
        "[network.interfaces.e0]\naddresses = [\"10.0.2.50\"]\n",
        "[network.interfaces.e0]\ngateway = \"10.0.2.2/24\"\n",
    ] {
        rejects(bad, "E_TYPE", 3);
    }
}

#[test]
fn syntax_is_refused_with_a_location() {
    let errors = rejects("[host\n", "E_SYNTAX", 3);
    assert!(
        errors.diagnostics[0].location.is_some(),
        "a syntax error has a location:\n{errors}"
    );
}

/// Rule: a missing host manifest is named, and its hint is the command that writes one in
/// place — `lodi host import`, with the `--root` it was asked about, and no privilege under a
/// `--root`.
#[test]
fn a_missing_manifest_is_named_and_the_hint_is_host_import() {
    let root = Root::new("core-missing");
    let euid = lodi::hostscope::safety::current_euid();
    let errors = lodi::hostscope::manifest::load(&root.dir, false, euid, &context())
        .expect_err("no manifest");
    assert_eq!(errors.codes(), vec!["E_NO_MANIFEST"]);
    let text = errors.to_string();
    assert!(
        text.contains(&format!(
            "`lodi host import --root {}` (it writes {})",
            root.dir.display(),
            root.path("etc/lodi/host.toml").display()
        )),
        "{text}"
    );
    assert!(!text.contains("host init"), "{text}");
    assert!(!text.contains("sudo"), "{text}");
}

/// The same hint for the running system's `/`, proved on the function that builds it: no host
/// command is run against `/` to produce it.
#[test]
fn the_missing_manifest_hint_on_the_system_root_is_sudo_host_import() {
    let hint = lodi::hostscope::manifest::import_hint(
        std::path::Path::new("/"),
        true,
        "/etc/lodi/host.toml",
    );
    assert!(
        hint.contains("`sudo lodi host import` (it writes /etc/lodi/host.toml)"), // check-host-safety: refusal
        "{hint}"
    );
    assert!(!hint.contains("--root"), "{hint}");
}

// ------------------------------------------------------------------------------- the host lock

#[test]
fn the_lock_round_trips_and_its_digests_detect_a_hand_edited_file() {
    let root = Root::new("core-lock");
    let (uid, gid) = ids(&root);
    root.debian_with(&format!(
        "[files.\"/etc/managed.conf\"]\ncontent = \"one\\n\"\nowner = \"{uid}\"\n\
         group = \"{gid}\"\n"
    ));

    let report = lodi::hostscope::apply(&root.options()).expect("the first apply");
    assert!(report.contains("+ file /etc/managed.conf"), "{report}");
    assert_eq!(root.read("etc/managed.conf"), "one\n");

    // The lock is a record of what was applied: it round-trips byte for byte and carries no
    // constraint on a later apply (OD-15).
    let path = root.path("etc/lodi/host.lock");
    let written = HostLock::read(&path)
        .expect("a readable lock")
        .expect("a lock");
    assert_eq!(written.files.len(), 1);
    let record = &written.files["/etc/managed.conf"];
    assert_eq!(record.mode, "0644");
    assert_eq!(record.owner, uid.to_string());
    assert!(record.digest.starts_with("sha256:"));
    assert_eq!(
        std::fs::read(&path).expect("the bytes"),
        written.to_bytes(),
        "the lock round-trips byte for byte"
    );
    assert!(
        !String::from_utf8_lossy(&written.to_bytes()).contains("constraint"),
        "the host lock records, it never constrains"
    );

    // A second apply over an unchanged root plans nothing and changes nothing.
    let before = std::fs::metadata(root.path("etc/managed.conf")).unwrap();
    let again = lodi::hostscope::apply(&root.options()).expect("the second apply");
    assert!(again.contains("nothing to do"), "{again}");
    let after = std::fs::metadata(root.path("etc/managed.conf")).unwrap();
    assert_eq!(
        before.modified().unwrap(),
        after.modified().unwrap(),
        "a second apply over an unchanged root changes no byte and no mtime"
    );

    // A hand-edited managed file is drift: the plan says so and the apply stops.
    root.write("etc/managed.conf", "edited by hand\n");
    let plan = lodi::hostscope::plan(&root.options()).expect("the drift plan");
    assert!(plan.contains("W_DRIFT"), "{plan}");
    assert!(
        plan.contains("~ file /etc/managed.conf (drift, content)"),
        "{plan}"
    );
    let refused = lodi::hostscope::apply(&root.options()).expect_err("drift stops the apply");
    assert_eq!(refused.codes(), vec!["E_DECLINED"]);
    assert_eq!(refused.exit_status(), 11);

    let mut confirmed = root.options();
    confirmed.overwrite_drift = true;
    let overwritten = lodi::hostscope::apply(&confirmed).expect("the confirmed apply");
    assert!(
        overwritten.contains("~ file /etc/managed.conf"),
        "{overwritten}"
    );
    assert_eq!(root.read("etc/managed.conf"), "one\n");
}

#[test]
fn a_lock_the_build_cannot_read_is_refused_rather_than_ignored() {
    let root = Root::new("core-lockversion");
    root.write("etc/lodi/host.lock", "{\"version\":99,\"format\":\"x\"}\n");
    let error = HostLock::read(&root.path("etc/lodi/host.lock")).expect_err("a refusal");
    assert_eq!(error.code, "E_LOCK_VERSION");
}

#[test]
fn a_lock_serialises_its_records_in_a_stable_order() {
    let mut lock = HostLock::new("2026-09-21T00:00:00Z".into(), "debian".into(), "12".into());
    lock.packages.insert(
        "git".into(),
        PackageRecord {
            version: "1:2.39.2-1.1".into(),
            mark: "manual".into(),
            held: false,
        },
    );
    lock.files.insert(
        "/etc/b".into(),
        FileRecord {
            digest: "sha256:b".into(),
            mode: "0644".into(),
            owner: "root".into(),
            group: "root".into(),
            backup: None,
            on_remove: "restore".into(),
        },
    );
    lock.files.insert(
        "/etc/a".into(),
        FileRecord {
            digest: "sha256:a".into(),
            mode: "0600".into(),
            owner: "root".into(),
            group: "root".into(),
            backup: Some("/var/lib/lodi/host/backups/a".into()),
            on_remove: "restore".into(),
        },
    );
    let text = String::from_utf8(lock.to_bytes()).expect("utf-8");
    assert!(
        text.find("/etc/a").unwrap() < text.find("/etc/b").unwrap(),
        "records are ordered, so that two applies of the same state produce the same bytes"
    );
    let keys: Vec<&String> = lock.files.keys().collect();
    assert_eq!(keys, vec!["/etc/a", "/etc/b"]);
    let _: BTreeMap<String, FileRecord> = lock.files.clone();
}
