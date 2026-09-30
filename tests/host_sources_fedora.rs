//! `[sources.NAME]` on Fedora 44 (LD-433): a declared dnf repository, written as
//! `/etc/yum.repos.d/lodi-NAME.repo` with `gpgcheck=1` beside its ASCII-armored key at
//! `/etc/pki/rpm-gpg/lodi-NAME.asc`, refreshed alone before the transaction that installs from it.
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakepm.py` behind it (`AGENTS.md` §8): its dnf5 reads the `.repo` files below
//! the root when `makecache` runs, and offers a repository's packages only once that repository
//! was refreshed. Each key is a generated armored block that only passes the framing check; no
//! vendor's key is in this repository.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use fakehost::{Case, Machine, Pkg, err, out, story};
use lodi::util::sha256_hex;

const REPO: &str = "etc/yum.repos.d/lodi-lodigate.repo";
const KEY: &str = "etc/pki/rpm-gpg/lodi-lodigate.asc";
const URI: &str = "https://repo.example.invalid/fedora/44/x86_64/";
/// Fedora's own repository file and key, as a fresh install has them.
const OWN_REPO: &str = "etc/yum.repos.d/fedora.repo";
const OWN_KEY: &str = "etc/pki/rpm-gpg/RPM-GPG-KEY-fedora-44-x86_64";

/// A generated armored public key block: framing only, `seed` makes two of them differ.
fn key(seed: &str) -> Vec<u8> {
    format!(
        "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nZ2VuZXJhdGVkIGZvciBhIHRlc3Q{seed}\n=AAAA\n\
         -----END PGP PUBLIC KEY BLOCK-----\n"
    )
    .into_bytes()
}

/// A Fedora 44 machine with Fedora's own repository file and key, and `lodigate-hello` in the
/// declared repository, which dnf knows only once that repository was refreshed.
fn case(name: &str) -> Case {
    let case = Case::new(name, Machine::fedora());
    case.root.write(
        OWN_REPO,
        "[fedora]\nname=Fedora $releasever - $basearch\n\
         metalink=https://mirrors.fedoraproject.org/metalink?repo=fedora-$releasever&arch=$basearch\n\
         enabled=1\ngpgcheck=1\ngpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-fedora-$releasever-$basearch\n",
    );
    case.root
        .write(OWN_KEY, &String::from_utf8(key("fedora")).unwrap());
    // The declared repository offers one package, known to dnf once `lodi-lodigate` is refreshed.
    case.edit(|m| {
        m["repos"] = serde_json::json!({
            "lodi-lodigate": {
                "lodigate-hello": {"version": "0:1.0-1.fc44", "depends": ["glibc"],
                                   "repo": "lodi-lodigate"},
            },
        });
    });
    case
}

/// The manifest: one `[sources.lodigate]` over `keys` (the key file is written with `bytes`),
/// and `packages` to install.
fn declare(case: &Case, bytes: &[u8], keys: &str, packages: &[&str]) {
    let file = case.root.path("etc/lodi/files/lodigate.asc");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, bytes).unwrap();
    let names: Vec<String> = packages.iter().map(|p| format!("\"{p}\"")).collect();
    case.set_manifest(&format!(
        "[host]\nversion = \"1\"\ndistro = \"fedora\"\npackages = \"managed\"\n\n\
         [sources.lodigate]\nuris = [\"{URI}\"]\nsigned_by = \"files/lodigate.asc\"\n{keys}\n\
         [packages.fedora]\nadd = [{}]\n",
        names.join(", ")
    ));
}

fn sha(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

/// Every regular file below the root, with its bytes' digest and its change time: what "nothing
/// changed" is checked against.
fn census(case: &Case) -> Vec<(String, String)> {
    fn visit(base: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                visit(base, &path, out);
            } else {
                out.push((
                    path.strip_prefix(base).unwrap().display().to_string(),
                    format!(
                        "{}:{}",
                        sha(&fs::read(&path).unwrap_or_default()),
                        meta.ctime_nsec()
                    ),
                ));
            }
        }
    }
    let mut out = Vec::new();
    visit(&case.root.dir, &case.root.dir, &mut out);
    out.sort();
    out
}

/// The dnf5 argv the fake logged, without the root option every invocation under `--root` has.
fn dnf_log(case: &Case) -> Vec<String> {
    let root = case.root.dir.display().to_string();
    case.log()
        .into_iter()
        .filter(|line| line.starts_with("dnf5 "))
        .map(|line| line.replace(&format!(" --installroot={root}"), ""))
        .collect()
}

fn changes(line: &str) -> bool {
    line.contains(" -y ") || line.contains("makecache")
}

#[test]
fn a_declared_repository_is_added_with_its_verified_key() {
    let case = case("fedora-sources-add");
    let bytes = key("a");
    declare(
        &case,
        &bytes,
        &format!("signed_by_sha256 = \"{}\"\n", sha(&bytes)),
        &["lodigate-hello"],
    );

    // The plan names the repository first, then the refresh of that repository alone.
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    let text = out(&plan).replace(&format!(" --installroot={}", case.root.dir.display()), "");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "+ source lodigate (key, repo)",
        "{}",
        story(&plan)
    );
    assert_eq!(
        lines[1],
        "~ index (dnf5 --refresh --repo=lodi-lodigate makecache)",
        "{}",
        story(&plan)
    );
    assert_eq!(
        lines[2],
        "+ package lodigate-hello (deferred: after the source refresh)",
        "{}",
        story(&plan)
    );
    assert!(!case.root.exists(REPO) && !case.root.exists(KEY));

    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert_eq!(fs::read(case.root.path(KEY)).unwrap(), bytes);
    assert_eq!(
        case.root.read(REPO),
        "# Written by lodi from a [sources] table of the host manifest; edit that table, not \
         this file.\n[lodi-lodigate]\nname=lodigate (lodi)\n\
         baseurl=https://repo.example.invalid/fedora/44/x86_64/\nenabled=1\ngpgcheck=1\n\
         gpgkey=file:///etc/pki/rpm-gpg/lodi-lodigate.asc\nskip_if_unavailable=0\n"
    );
    let mode = fs::metadata(case.root.path(REPO)).unwrap().mode() & 0o7777;
    assert_eq!(mode, 0o644);
    // The refresh covered that repository only, and saw its key already in place.
    let refreshes: Vec<String> = dnf_log(&case)
        .into_iter()
        .filter(|l| l.contains("makecache"))
        .collect();
    assert_eq!(
        refreshes,
        ["dnf5 --refresh --repo=lodi-lodigate makecache"],
        "{:?}",
        case.log()
    );
    let saw = fs::read_to_string(case.base().join("state/makecache-saw")).unwrap();
    assert_eq!(
        saw,
        "lodi-lodigate.repo file:///etc/pki/rpm-gpg/lodi-lodigate.asc present\n"
    );
    assert!(
        case.installed().contains("lodigate-hello"),
        "{}",
        story(&apply)
    );
    let machine = case.machine();
    assert_eq!(
        machine["installed"]["lodigate-hello"]["from_repo"], "lodi-lodigate",
        "installed from the declared repository"
    );
    // Fedora's own repository and key are untouched, and a second apply has nothing to do.
    assert!(case.root.read(OWN_REPO).starts_with("[fedora]\n"));
    assert_eq!(case.apply(&[]).stdout, b"nothing to do\n");

    // A machine whose index is stale, or was never fetched, gets the one full refresh instead:
    // it covers the declared repository too, and Fedora's own are not left without a cache.
    let stale = self::case("fedora-sources-add-stale");
    fs::remove_file(
        stale
            .root
            .path("var/cache/libdnf5/fedora-0/repodata/repomd.xml"),
    )
    .unwrap();
    declare(
        &stale,
        &bytes,
        &format!("signed_by_sha256 = \"{}\"\n", sha(&bytes)),
        &["lodigate-hello"],
    );
    let plan = stale.plan();
    let text = out(&plan).replace(&format!(" --installroot={}", stale.root.dir.display()), "");
    assert_eq!(
        text.lines().nth(1),
        Some("~ index (dnf5 --refresh makecache)"),
        "{}",
        story(&plan)
    );
    let apply = stale.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(stale.installed().contains("lodigate-hello"));
    let refreshes: Vec<String> = dnf_log(&stale)
        .into_iter()
        .filter(|l| l.contains("makecache"))
        .collect();
    assert_eq!(refreshes, ["dnf5 --refresh makecache"], "{:?}", stale.log());
}

#[test]
fn an_unsafe_or_duplicate_repository_is_refused_before_any_change() {
    let case = case("fedora-sources-refuse");
    let bytes = key("a");
    let good = format!("signed_by_sha256 = \"{}\"\n", sha(&bytes));
    let refusals: Vec<(&str, String, &str, &str)> = vec![
        (
            "a key whose SHA-256 differs",
            format!("signed_by_sha256 = \"{}\"\n", sha(&key("b"))),
            "E_HASH_MISMATCH",
            "[sources.lodigate]: the keyring files/lodigate.asc has sha256:",
        ),
        (
            "gpgcheck switched off",
            format!("{good}gpgcheck = false\n"),
            "E_GPGCHECK_OFF",
            "[sources.lodigate] turns the signature check `gpgcheck` off",
        ),
        (
            "repo_gpgcheck switched off",
            format!("{good}repo_gpgcheck = false\n"),
            "E_GPGCHECK_OFF",
            "[sources.lodigate] turns the signature check `repo_gpgcheck` off",
        ),
    ];
    for (what, keys, code, words) in refusals {
        declare(&case, &bytes, &keys, &["lodigate-hello"]);
        let before = census(&case);
        let _ = fs::remove_file(case.base().join("state/log"));
        for verb in ["plan", "apply"] {
            let run = case.verb(verb, &[]);
            assert!(!run.status.success(), "{what}: {}", story(&run));
            assert!(
                err(&run).contains(code) && err(&run).contains(words),
                "{what}: {}",
                story(&run)
            );
        }
        assert_eq!(census(&case), before, "{what}: nothing changed");
        assert!(
            !dnf_log(&case).iter().any(|l| changes(l)),
            "{what}: {:?}",
            case.log()
        );
    }

    // A plain http:// address, in place of the https:// one.
    let file = case.root.path("etc/lodi/files/lodigate.asc");
    fs::write(&file, &bytes).unwrap();
    case.set_manifest(
        &case
            .manifest()
            .replace("repo_gpgcheck = false\n", "")
            .replace("gpgcheck = false\n", "")
            .replace(URI, "http://repo.example.invalid/fedora/44/x86_64/"),
    );
    let apply = case.apply(&[]);
    assert!(!apply.status.success(), "{}", story(&apply));
    assert!(
        err(&apply).contains("E_UNSUPPORTED") && err(&apply).contains("is not an https:// URI"),
        "{}",
        story(&apply)
    );

    // A key dnf cannot read: binary packets, where rpm imports an armored block only.
    declare(&case, &[0xC6, 3, 4, 0, 0], "", &[]);
    let binary = [0xC6u8, 3, 4, 0, 0];
    case.set_manifest(&case.manifest().replace(
        "signed_by = \"files/lodigate.asc\"\n",
        &format!(
            "signed_by = \"files/lodigate.asc\"\nsigned_by_sha256 = \"{}\"\n",
            sha(&binary)
        ),
    ));
    let apply = case.apply(&[]);
    assert!(
        err(&apply).contains("E_TYPE") && err(&apply).contains("ASCII-armored"),
        "{}",
        story(&apply)
    );

    // A repository the machine already lists, under another id and file.
    declare(&case, &bytes, &good, &["lodigate-hello"]);
    case.root.write(
        "etc/yum.repos.d/vendor.repo",
        &format!("[vendor]\nname=Vendor\nbaseurl={URI}\nenabled=1\ngpgcheck=1\n"),
    );
    let before_dup = census(&case);
    let apply = case.apply(&[]);
    assert!(!apply.status.success(), "{}", story(&apply));
    assert!(
        err(&apply).contains("E_DUP_RESOURCE")
            && err(&apply).contains(
                "[sources.lodigate] lists https://repo.example.invalid/fedora/44/x86_64/, which \
                 /etc/yum.repos.d/vendor.repo already lists"
            ),
        "{}",
        story(&apply)
    );
    assert_eq!(census(&case), before_dup);
    // Disabled there, it is no longer a duplicate.
    case.root.write(
        "etc/yum.repos.d/vendor.repo",
        &format!("[vendor]\nname=Vendor\nbaseurl={URI}\nenabled=0\ngpgcheck=1\n"),
    );
    assert!(case.plan().status.success());
    assert!(!case.root.exists(REPO) && !case.root.exists(KEY));
}

#[test]
fn a_failed_refresh_restores_the_repository_files() {
    let case = case("fedora-sources-refresh-fails");
    let first = key("a");
    declare(
        &case,
        &first,
        &format!("signed_by_sha256 = \"{}\"\n", sha(&first)),
        &[],
    );
    let armed = case.apply(&[]);
    assert!(armed.status.success(), "{}", story(&armed));
    let repo_before = fs::read(case.root.path(REPO)).unwrap();
    let key_before = fs::read(case.root.path(KEY)).unwrap();

    // A new key and a new address; the repository's metadata cannot be fetched.
    let second = key("b");
    declare(
        &case,
        &second,
        &format!("signed_by_sha256 = \"{}\"\n", sha(&second)),
        &["lodigate-hello"],
    );
    case.set_manifest(
        &case
            .manifest()
            .replace(URI, "https://moved.example.invalid/f44/"),
    );
    case.edit(|m| {
        m["fail_repo"] = serde_json::json!({
            "lodi-lodigate": "Curl error (6): Couldn't resolve host name",
        });
    });
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(8), "{}", story(&apply));
    let text = err(&apply);
    assert!(
        text.contains("E_APPLY")
            && text.contains("the index refresh [sources.lodigate] forced failed"),
        "{}",
        story(&apply)
    );
    assert!(text.contains("put back as it was"), "{}", story(&apply));
    assert_eq!(fs::read(case.root.path(REPO)).unwrap(), repo_before);
    assert_eq!(fs::read(case.root.path(KEY)).unwrap(), key_before);
    assert!(!case.installed().contains("lodigate-hello"));

    // A first apply whose refresh fails leaves neither file behind.
    let fresh = self::case("fedora-sources-refresh-fails-fresh");
    declare(
        &fresh,
        &first,
        &format!("signed_by_sha256 = \"{}\"\n", sha(&first)),
        &["lodigate-hello"],
    );
    fresh.edit(|m| {
        m["fail_repo"] = serde_json::json!({ "lodi-lodigate": "Curl error (6)" });
    });
    let apply = fresh.apply(&[]);
    assert_eq!(apply.status.code(), Some(8), "{}", story(&apply));
    assert!(!fresh.root.exists(REPO) && !fresh.root.exists(KEY));
    assert!(
        err(&apply).contains("[sources.lodigate]"),
        "{}",
        story(&apply)
    );

    // Once the repository answers again, the apply converges.
    fresh.edit(|m| {
        m.as_object_mut().unwrap().remove("fail_repo");
    });
    let again = fresh.apply(&[]);
    assert!(again.status.success(), "{}", story(&again));
    assert!(fresh.installed().contains("lodigate-hello"));
}

#[test]
fn import_writes_third_party_repositories_as_commented_sources() {
    let mut vendor = Pkg::new("vendor-tool").repo(Some("vendor"));
    vendor.version = "0:2.0-1.fc44".to_string();
    let case = Case::new("fedora-sources-import", Machine::fedora().with(vendor));
    case.edit(|m| m["installed"]["vendor-tool"]["from_repo"] = "vendor".into());
    case.root.write(
        OWN_REPO,
        "[fedora]\nname=Fedora $releasever - $basearch\nmetalink=https://mirrors.example/m\n\
         enabled=1\ngpgcheck=1\ngpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-fedora-44-x86_64\n",
    );
    case.root
        .write(OWN_KEY, &String::from_utf8(key("fedora")).unwrap());
    let vendor_key = key("vendor");
    case.root.write(
        "etc/pki/rpm-gpg/RPM-GPG-KEY-vendor",
        &String::from_utf8(vendor_key.clone()).unwrap(),
    );
    case.root.write(
        "etc/yum.repos.d/vendor.repo",
        "[vendor]\nname=Vendor $releasever\nbaseurl=https://vendor.example.invalid/f44/\n\
         enabled=1\ngpgcheck=1\ngpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-vendor\n\n\
         [vendor-source]\nname=Vendor sources\nbaseurl=https://vendor.example.invalid/src/\n\
         enabled=0\ngpgcheck=1\n",
    );
    case.root.write(
        "etc/yum.repos.d/plain.repo",
        "[plain]\nname=Plain\nbaseurl=http://plain.example.invalid/f44/\nenabled=1\ngpgcheck=0\n",
    );

    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let manifest = case.manifest();
    let block =
        "# [sources.vendor]\n# # read from /etc/yum.repos.d/vendor.repo, repository vendor\n";
    assert!(manifest.contains(block), "{manifest}");
    assert!(
        manifest.contains(&format!(
            "# uris = [\"https://vendor.example.invalid/f44/\"]\n\
             # signed_by = \"files/etc/pki/rpm-gpg/lodi-vendor.asc\"\n\
             # signed_by_sha256 = \"{}\"\n",
            sha(&vendor_key)
        )),
        "{manifest}"
    );
    assert!(manifest.contains("# #   vendor-tool\n"), "{manifest}");
    // The key travels beside the manifest, at the path the block names.
    assert_eq!(
        fs::read(
            case.root
                .path("etc/lodi/files/etc/pki/rpm-gpg/lodi-vendor.asc")
        )
        .unwrap(),
        vendor_key
    );
    // Fedora's own repository is counted, never written; the unsafe one is named with its reason.
    assert!(!manifest.contains("[sources.fedora]"), "{manifest}");
    assert!(!manifest.contains("pacman"), "{manifest}");
    assert!(
        manifest.contains("#   /etc/yum.repos.d/plain.repo, repository plain\n"),
        "{manifest}"
    );
    assert!(
        manifest.contains("its baseurl is not https://, which every source lodi arms is"),
        "{manifest}"
    );
    assert!(
        manifest.contains("# the distribution's own repositories, which every install has: 1\n"),
        "{manifest}"
    );
    assert!(
        !manifest.contains("vendor-source"),
        "a disabled repository: {manifest}"
    );

    // A second import writes the same bytes.
    let again = case.verb("import", &["--force"]);
    assert!(again.status.success(), "{}", story(&again));
    assert_eq!(case.manifest(), manifest);
    let stdout = case.verb("import", &["--stdout"]);
    let stdout_again = case.verb("import", &["--stdout"]);
    assert_eq!(out(&stdout), out(&stdout_again));
}

#[test]
fn removing_the_block_removes_the_repository_and_key() {
    let case = case("fedora-sources-remove");
    let bytes = key("a");
    declare(
        &case,
        &bytes,
        &format!("signed_by_sha256 = \"{}\"\n", sha(&bytes)),
        &["lodigate-hello"],
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(case.root.exists(REPO) && case.root.exists(KEY));
    let own_repo = fs::read(case.root.path(OWN_REPO)).unwrap();
    let own_key = fs::read(case.root.path(OWN_KEY)).unwrap();

    case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"fedora\"\npackages = \"managed\"\n\n\
         [packages.fedora]\nadd = [\"lodigate-hello\"]\n",
    );
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        out(&plan).starts_with("- source lodigate (repo: remove /etc/yum.repos.d/lodi-lodigate.repo, key: remove /etc/pki/rpm-gpg/lodi-lodigate.asc)\n"),
        "{}",
        story(&plan)
    );
    let _ = fs::remove_file(case.base().join("state/log"));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(!case.root.exists(REPO) && !case.root.exists(KEY));
    assert!(
        !dnf_log(&case).iter().any(|l| l.contains("makecache")),
        "removing a repository refreshes nothing: {:?}",
        case.log()
    );
    assert_eq!(fs::read(case.root.path(OWN_REPO)).unwrap(), own_repo);
    assert_eq!(fs::read(case.root.path(OWN_KEY)).unwrap(), own_key);
    let files = case.lock()["files"].clone();
    assert!(
        !files.to_string().contains("lodi-lodigate"),
        "the lock records neither file: {files}"
    );
    assert_eq!(case.apply(&[]).stdout, b"nothing to do\n");
}
