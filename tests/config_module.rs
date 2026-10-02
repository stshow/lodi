//! The config module (#692, LD-491, LD-493): *find*, *read layout* and *remember*, each input
//! passed in. Every case builds a scratch root holding its own passwd, group, `HOME`, state and
//! config, and runs from a decoy current directory that holds a config of its own, which no case
//! may ever be given; nothing reads the ambient `HOME`, XDG, the real passwd or `/`.

mod support;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Once;

use lodi::config::{self, Identity, Location, Origin};

/// The test's own ids: every user of a scratch passwd is one of them, so its files are its own.
fn ids() -> (u32, u32) {
    // SAFETY: neither call has a precondition.
    unsafe { (libc::geteuid(), libc::getegid()) }
}

/// The decoy current directory: a flat config no case names. Set once for the whole binary.
fn decoy() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = support::scratch("config-decoy");
        fs::write(dir.join("host.toml"), "").unwrap();
        fs::write(dir.join("lodi.lock"), "").unwrap();
        std::env::set_current_dir(&dir).unwrap();
    });
}

/// A scratch root for one case: `etc/passwd` naming `alice` (the test's own ids), her private
/// group, and her home at `/home/alice` (as the root sees it).
struct Machine {
    root: PathBuf,
}

impl Machine {
    fn new(name: &str) -> Machine {
        decoy();
        let root = support::scratch(name);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let (uid, gid) = ids();
        let machine = Machine { root };
        machine.write(
            "etc/passwd",
            &format!("root:x:0:0::/root:/bin/sh\nalice:x:{uid}:{gid}::/home/alice:/bin/sh\n"),
        );
        machine.write("etc/group", &format!("root:x:0:\nalice:x:{gid}:\n"));
        fs::create_dir_all(machine.home()).unwrap();
        machine
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn home(&self) -> PathBuf {
        self.path("home/alice")
    }

    fn write(&self, rel: &str, body: &str) -> PathBuf {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    /// Alice, unprivileged, with `HOME` set and no XDG state.
    fn alice(&self) -> Identity {
        Identity {
            euid: ids().0,
            sudo_uid: None,
            doas_user: None,
            home: Some(self.home()),
            state_home: None,
            config_home: None,
            data_home: None,
            root: self.root.clone(),
            hostname: "laptop".into(),
        }
    }

    /// Root under `sudo`, run by alice: root's `HOME` and an XDG state of root's own.
    fn sudo(&self) -> Identity {
        fs::create_dir_all(self.path("root")).unwrap();
        Identity {
            euid: 0,
            sudo_uid: Some(ids().0.to_string().into()),
            doas_user: None,
            home: Some(self.path("root")),
            state_home: Some(self.path("root/.local/state")),
            config_home: None,
            data_home: None,
            root: self.root.clone(),
            hostname: "laptop".into(),
        }
    }

    /// The remembered-path file of a user whose state folder is `~/.local/state`.
    fn remembered(&self) -> PathBuf {
        self.home()
            .join(".local/state/lodi")
            .join(config::REMEMBERED)
    }

    /// A flat config at `rel` holding `files`.
    fn config(&self, rel: &str, files: &[&str]) -> PathBuf {
        for file in files {
            self.write(&format!("{rel}/{file}"), "");
        }
        self.path(rel)
    }
}

fn local(found: &config::Found) -> &Path {
    match &found.location {
        Location::Local(path) => path,
        Location::Url(url) => panic!("a URL {url}, not a folder"),
    }
}

#[test]
fn a_typed_path_is_the_config() {
    let machine = Machine::new("typed");
    let dir = machine.config("dotfiles", &["host.toml"]);
    machine.config("home/alice/.config/lodi", &["host.toml"]);
    let found = config::find(Some(dir.as_os_str()), None, &machine.alice()).unwrap();
    assert_eq!(
        (local(&found), found.origin),
        (dir.as_path(), Origin::Typed)
    );
}

#[test]
fn a_typed_url_is_kept_exactly_as_typed() {
    let machine = Machine::new("typed-url");
    let found = config::find(
        Some(OsStr::new("github:alice/dots")),
        None,
        &machine.alice(),
    );
    let found = found.unwrap();
    assert_eq!(found.location, Location::Url("github:alice/dots".into()));
    assert_eq!(found.origin, Origin::Typed);
}

#[test]
fn lodi_repo_wins_over_the_remembered_path() {
    let machine = Machine::new("env");
    let remembered = machine.config("remembered", &["host.toml"]);
    let named = machine.config("named", &["home.toml"]);
    let file = machine.remembered();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, format!("{}\n", remembered.display())).unwrap();
    let found = config::find(None, Some(named.as_os_str()), &machine.alice()).unwrap();
    assert_eq!(
        (local(&found), found.origin),
        (named.as_path(), Origin::Environment)
    );
}

#[test]
fn the_remembered_path_comes_before_the_standard_folder() {
    let machine = Machine::new("remembered");
    let remembered = machine.config("remembered", &["host.toml"]);
    machine.config("home/alice/.config/lodi", &["host.toml"]);
    let file = machine.remembered();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, format!("{}\n", remembered.display())).unwrap();
    let found = config::find(None, None, &machine.alice()).unwrap();
    assert_eq!(
        (local(&found), found.origin),
        (remembered.as_path(), Origin::Remembered)
    );
}

#[test]
fn xdg_state_home_holds_the_remembered_path_when_set() {
    let machine = Machine::new("remembered-xdg");
    let remembered = machine.config("remembered", &["host.toml"]);
    machine.write(
        &format!("state/lodi/{}", config::REMEMBERED),
        &format!("{}\n", remembered.display()),
    );
    let mut alice = machine.alice();
    alice.state_home = Some(machine.path("state"));
    let found = config::find(None, None, &alice).unwrap();
    assert_eq!(
        (local(&found), found.origin),
        (remembered.as_path(), Origin::Remembered)
    );
}

#[test]
fn the_standard_folder_is_used_when_nothing_else_is_set_and_empty_lodi_repo_is_unset() {
    let machine = Machine::new("standard");
    let standard = machine.config("home/alice/.config/lodi", &["host.toml"]);
    for env in [None, Some(OsStr::new(""))] {
        let found = config::find(None, env, &machine.alice()).unwrap();
        assert_eq!(
            (local(&found), found.origin),
            (standard.as_path(), Origin::Standard)
        );
    }
}

#[test]
fn nothing_found_stops_naming_the_standard_folder() {
    let machine = Machine::new("nothing");
    let stop = config::find(None, None, &machine.alice()).unwrap_err();
    assert_eq!(stop.code, "E_NO_MANIFEST");
    let standard = machine.home().join(".config/lodi");
    assert!(
        stop.message.contains(&standard.display().to_string()),
        "{stop}"
    );
    assert!(
        stop.notes.iter().any(|note| note.contains("LODI_REPO")),
        "{stop}"
    );
}

/// Write `body` as alice's remembered-path file.
fn remember_raw(machine: &Machine, body: &[u8]) -> PathBuf {
    let file = machine.remembered();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, body).unwrap();
    file
}

#[test]
fn a_remembered_folder_that_is_gone_stops_naming_it() {
    let machine = Machine::new("gone");
    machine.config("home/alice/.config/lodi", &["host.toml"]);
    let gone = machine.path("moved-away");
    remember_raw(&machine, format!("{}\n", gone.display()).as_bytes());
    let stop = config::find(None, None, &machine.alice()).unwrap_err();
    assert_eq!(stop.code, "E_NO_MANIFEST");
    assert!(stop.message.contains(&gone.display().to_string()), "{stop}");
}

#[test]
fn a_damaged_remembered_file_stops_naming_the_file() {
    let machine = Machine::new("damaged");
    machine.config("home/alice/.config/lodi", &["host.toml"]);
    let dir = machine.config("dotfiles", &["host.toml"]);
    let two = format!("{}\n{}\n", dir.display(), dir.display());
    for body in [
        &b""[..],
        b"\n",
        b"relative/path\n",
        two.as_bytes(),
        b"\xff\xfe\n",
    ] {
        let file = remember_raw(&machine, body);
        let stop = config::find(None, None, &machine.alice()).unwrap_err();
        assert_eq!(stop.code, "E_CONFIG", "{body:?}");
        assert!(stop.message.contains(&file.display().to_string()), "{stop}");
    }
}

#[test]
fn under_sudo_the_person_behind_it_is_used_not_root() {
    let machine = Machine::new("sudo");
    let theirs = machine.config("home/alice/.config/lodi", &["host.toml"]);
    machine.config("root/.config/lodi", &["host.toml"]);
    let roots = machine.config("roots-own", &["host.toml"]);
    machine.write(
        &format!("root/.local/state/lodi/{}", config::REMEMBERED),
        &format!("{}\n", roots.display()),
    );
    let found = config::find(None, None, &machine.sudo()).unwrap();
    assert_eq!(
        (local(&found), found.origin),
        (theirs.as_path(), Origin::Standard)
    );
    let remembered = machine.config("remembered", &["home.toml"]);
    remember_raw(&machine, format!("{}\n", remembered.display()).as_bytes());
    let found = config::find(None, None, &machine.sudo()).unwrap();
    assert_eq!(
        (local(&found), found.origin),
        (remembered.as_path(), Origin::Remembered)
    );
}

#[test]
fn under_sudo_a_sudo_uid_the_passwd_does_not_name_stops() {
    let machine = Machine::new("sudo-unknown");
    machine.config("home/alice/.config/lodi", &["host.toml"]);
    let mut identity = machine.sudo();
    identity.sudo_uid = Some("4242".into());
    if ids().0 == 4242 {
        identity.sudo_uid = Some("4243".into());
    }
    let stop = config::find(None, None, &identity).unwrap_err();
    assert!(
        stop.message
            .contains(&machine.path("etc/passwd").display().to_string()),
        "{stop}"
    );
}

#[test]
fn only_remember_writes_and_only_a_typed_path_or_url() {
    let machine = Machine::new("remember");
    let dir = machine.config("dotfiles", &["host.toml"]);
    let file = machine.remembered();
    let typed = config::find(Some(dir.as_os_str()), None, &machine.alice()).unwrap();
    assert!(
        !file.exists(),
        "find never writes; a failed command remembers nothing"
    );
    let other = machine.config("other", &["host.toml"]);
    let env = config::find(None, Some(other.as_os_str()), &machine.alice()).unwrap();
    config::remember(&env).unwrap();
    assert!(!file.exists(), "LODI_REPO is never remembered");
    config::remember(&typed).unwrap();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        format!("{}\n", dir.display())
    );
    let mode = fs::metadata(file.parent().unwrap()).unwrap().mode() & 0o7777;
    assert_eq!(mode, 0o700);
    let again = config::find(None, None, &machine.alice()).unwrap();
    assert_eq!(again.origin, Origin::Remembered);
    config::remember(&again).unwrap();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        format!("{}\n", dir.display())
    );
    let url = config::find(
        Some(OsStr::new("github:alice/dots")),
        None,
        &machine.alice(),
    );
    config::remember(&url.unwrap()).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "github:alice/dots\n");
}

#[test]
fn remember_under_sudo_writes_the_persons_file_owned_by_them() {
    let machine = Machine::new("remember-sudo");
    let dir = machine.config("dotfiles", &["host.toml"]);
    let found = config::find(Some(dir.as_os_str()), None, &machine.sudo()).unwrap();
    config::remember(&found).unwrap();
    let file = machine.remembered();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        format!("{}\n", dir.display())
    );
    assert!(!machine.path("root/.local/state/lodi").exists());
    let (uid, gid) = ids();
    for path in [&file, &file.parent().unwrap().to_path_buf()] {
        let meta = fs::metadata(path).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (uid, gid), "{}", path.display());
    }
}

#[test]
fn a_folder_inside_a_config_stops_naming_the_root() {
    let machine = Machine::new("inside");
    let root = machine.config("dotfiles", &["config.toml"]);
    let inner = machine.config("dotfiles/server", &["host.toml"]);
    let stop = config::find(Some(inner.as_os_str()), None, &machine.alice()).unwrap_err();
    assert_eq!(stop.code, "E_NO_MANIFEST");
    assert!(stop.message.contains(&root.display().to_string()), "{stop}");
    let flat = machine.config("flat", &["host.toml"]);
    let files = machine.config("flat/files", &["home.toml"]);
    let stop = config::find(Some(files.as_os_str()), None, &machine.alice()).unwrap_err();
    assert!(stop.message.contains(&flat.display().to_string()), "{stop}");
}

#[test]
fn a_project_folder_is_never_a_config() {
    let machine = Machine::new("project");
    let project = machine.config("work", &["lodi.toml", "lodi.lock", "host.toml"]);
    let stop = config::find(Some(project.as_os_str()), None, &machine.alice()).unwrap_err();
    assert_eq!(stop.code, "E_NO_MANIFEST");
    assert!(
        stop.message.contains(&project.display().to_string()),
        "{stop}"
    );
}

#[test]
fn a_folder_holding_no_config_stops_and_any_one_marker_makes_it_one() {
    let machine = Machine::new("markers");
    let empty = machine.config("empty", &["README"]);
    let stop = config::find(Some(empty.as_os_str()), None, &machine.alice()).unwrap_err();
    assert_eq!(stop.code, "E_NO_MANIFEST");
    assert!(
        stop.message.contains(&empty.display().to_string()),
        "{stop}"
    );
    assert!(
        stop.notes.iter().any(|note| note.contains("lodi import")),
        "{stop}"
    );
    for marker in ["lodi.lock", "config.toml", "host.toml", "home.toml"] {
        let dir = machine.config(&format!("with-{marker}"), &[marker]);
        let found = config::find(Some(dir.as_os_str()), None, &machine.alice()).unwrap();
        assert_eq!(local(&found), dir.as_path());
    }
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn group_write_is_accepted_only_for_the_owners_private_group() {
    let machine = Machine::new("private-group");
    let dir = machine.config("home/alice/.config/lodi", &["host.toml"]);
    chmod(&dir, 0o775);
    chmod(&dir.join("host.toml"), 0o664);
    config::find(None, None, &machine.alice()).unwrap();
    let gid = ids().1;
    for group in [format!("staff:x:{gid}:\n"), format!("alice:x:{gid}:bob\n")] {
        machine.write("etc/group", &group);
        let stop = config::find(None, None, &machine.alice()).unwrap_err();
        assert_eq!(stop.code, "E_PATH_ESCAPE", "{group}");
        assert!(stop.message.contains(&dir.display().to_string()), "{stop}");
    }
}

#[test]
fn write_by_others_a_link_or_a_folder_outside_the_root_is_refused() {
    let machine = Machine::new("trust");
    let open = machine.config("open", &["host.toml"]);
    chmod(&open, 0o757);
    let linked = machine.config("real", &["host.toml"]);
    std::os::unix::fs::symlink(&linked, machine.path("link")).unwrap();
    let outside = support::scratch("config-outside");
    fs::write(outside.join("host.toml"), "").unwrap();
    for dir in [open, machine.path("link"), outside] {
        let stop = config::find(Some(dir.as_os_str()), None, &machine.alice()).unwrap_err();
        assert_eq!(stop.code, "E_PATH_ESCAPE", "{}", dir.display());
    }
}

/// A selection's (host key, home key), each the manifest's path from the root.
type Keys<'a> = (Option<&'a str>, Option<&'a str>);

/// What a selection names.
fn keys(selection: &config::Selection) -> Keys<'_> {
    (
        selection.host.as_ref().map(|part| part.key.as_str()),
        selection.home.as_ref().map(|part| part.key.as_str()),
    )
}

#[test]
fn a_flat_config_is_this_host_and_the_invoking_users_home() {
    let machine = Machine::new("flat");
    let cases: [(&[&str], Keys); 4] = [
        (
            &["host.toml", "home.toml", "lodi.lock"],
            (Some("host.toml"), Some("home.toml")),
        ),
        (&["host.toml"], (Some("host.toml"), None)),
        (&["home.toml"], (None, Some("home.toml"))),
        (
            &["host.toml", "home.toml"],
            (Some("host.toml"), Some("home.toml")),
        ),
    ];
    for (i, (files, want)) in cases.into_iter().enumerate() {
        let dir = machine.config(&format!("c{i}"), files);
        machine.write(&format!("c{i}/server/host.toml"), "");
        let found = config::find(Some(dir.as_os_str()), None, &machine.alice()).unwrap();
        let layout = config::read_layout(&dir, &found.user).unwrap();
        assert_eq!(layout.root, dir);
        let selection = layout.this_host().unwrap();
        assert_eq!(keys(&selection), want, "{files:?}");
        if let Some(host) = &selection.host {
            assert_eq!(host.path, dir.join("host.toml"));
        }
        let only = layout.named_or_only(None).unwrap();
        assert_eq!(keys(&only), want, "{files:?}");
    }
}

const TWO_HOSTS: &str = "\
[hosts.laptop]
host  = \"host.toml\"
homes = { alice = \"home.toml\" }

[hosts.server]
host  = \"server/host.toml\"
homes = { alice = \"home.toml\", bob = \"bob/home.toml\" }
";

/// A config whose `config.toml` is `index`, holding every file [`TWO_HOSTS`] names.
fn indexed(machine: &Machine, index: &str) -> (PathBuf, config::Config) {
    let dir = machine.config(
        "dotfiles",
        &[
            "host.toml",
            "home.toml",
            "server/host.toml",
            "bob/home.toml",
        ],
    );
    machine.write("dotfiles/config.toml", index);
    let found = config::find(Some(dir.as_os_str()), None, &machine.alice()).unwrap();
    let layout = config::read_layout(&dir, &found.user).unwrap();
    (dir, layout)
}

#[test]
fn config_toml_chooses_this_host_by_hostname_and_the_home_by_login() {
    let machine = Machine::new("indexed");
    let (dir, layout) = indexed(&machine, TWO_HOSTS);
    assert_eq!(layout.hosts(), ["laptop", "server"]);
    let laptop = layout.this_host().unwrap();
    assert_eq!(laptop.name.as_deref(), Some("laptop"));
    assert_eq!(keys(&laptop), (Some("host.toml"), Some("home.toml")));
    let mut server = machine.alice();
    server.hostname = "server".into();
    let found = config::find(Some(dir.as_os_str()), None, &server).unwrap();
    let chosen = config::read_layout(&dir, &found.user)
        .unwrap()
        .this_host()
        .unwrap();
    assert_eq!(keys(&chosen), (Some("server/host.toml"), Some("home.toml")));
    assert_eq!(chosen.host.unwrap().path, dir.join("server/host.toml"));
    assert_eq!(chosen.home, laptop.home, "two hosts share one home file");
}

#[test]
fn config_toml_with_no_host_of_this_hostname_stops_listing_the_hosts() {
    let machine = Machine::new("indexed-none");
    let mut desk = machine.alice();
    desk.hostname = "desk".into();
    let (dir, _) = indexed(&machine, TWO_HOSTS);
    let found = config::find(Some(dir.as_os_str()), None, &desk).unwrap();
    let stop = config::read_layout(&dir, &found.user)
        .unwrap()
        .this_host()
        .unwrap_err();
    assert_eq!(stop.code, "E_NO_MANIFEST");
    let text = stop.to_string();
    assert!(text.contains("laptop") && text.contains("server"), "{text}");
}

#[test]
fn a_login_with_no_homes_entry_reads_the_host_part_alone() {
    let machine = Machine::new("indexed-nologin");
    let index = "[hosts.laptop]\nhost = \"host.toml\"\nhomes = { bob = \"bob/home.toml\" }\n\
                 [hosts.server]\nhost = \"server/host.toml\"\n";
    let (_, layout) = indexed(&machine, index);
    assert_eq!(
        keys(&layout.this_host().unwrap()),
        (Some("host.toml"), None)
    );
    let server = layout.named_or_only(Some("server")).unwrap();
    assert_eq!(keys(&server), (Some("server/host.toml"), None));
}

#[test]
fn named_or_only_needs_host_only_when_config_toml_lists_several() {
    let machine = Machine::new("named");
    let (_, layout) = indexed(&machine, TWO_HOSTS);
    let stop = layout.named_or_only(None).unwrap_err();
    assert_eq!(stop.code, "E_NO_MANIFEST");
    assert!(stop.to_string().contains("--host"), "{stop}");
    let server = layout.named_or_only(Some("server")).unwrap();
    assert_eq!(server.name.as_deref(), Some("server"));
    let stop = layout.named_or_only(Some("desk")).unwrap_err();
    assert_eq!(stop.code, "E_NO_MANIFEST");
    let one = Machine::new("named-one");
    let index = "[hosts.server]\nhost = \"server/host.toml\"\n";
    let (dir, layout) = indexed(&one, index);
    let only = layout.named_or_only(None).unwrap();
    assert_eq!(keys(&only), (Some("server/host.toml"), None));
    assert_eq!(
        layout.this_host().unwrap_err().code,
        "E_NO_MANIFEST",
        "laptop is not listed"
    );
    fs::remove_file(dir.join("config.toml")).unwrap();
    let flat = config::read_layout(
        &dir,
        &config::find(Some(dir.as_os_str()), None, &one.alice())
            .unwrap()
            .user,
    )
    .unwrap();
    assert!(
        flat.named_or_only(Some("server")).is_err(),
        "a flat config has no named host"
    );
}

/// What `read_layout` says of a config whose `config.toml` is `index`.
fn index_stop(machine: &Machine, index: &str) -> lodi::diag::Diagnostic {
    let dir = machine.config("dotfiles", &["host.toml"]);
    let file = machine.write("dotfiles/config.toml", index);
    let found = config::find(Some(dir.as_os_str()), None, &machine.alice()).unwrap();
    let stop = config::read_layout(&dir, &found.user).unwrap_err();
    assert!(stop.message.contains(&file.display().to_string()), "{stop}");
    stop
}

#[test]
fn config_toml_refuses_unknown_keys_and_bad_names_naming_file_and_key() {
    let machine = Machine::new("index-bad");
    let cases = [
        (
            "[hosts.laptop]\nhost = \"host.toml\"\nhomez = {}\n",
            "homez",
        ),
        (
            "version = 1\n[hosts.laptop]\nhost = \"host.toml\"\n",
            "version",
        ),
        ("[hosts.\"a/b\"]\nhost = \"host.toml\"\n", "a/b"),
        ("[hosts.\"..\"]\nhost = \"host.toml\"\n", ".."),
        (
            "[hosts.laptop]\nhost = \"host.toml\"\nhomes = { \"x/y\" = \"home.toml\" }\n",
            "x/y",
        ),
        (
            "[hosts.laptop]\nhomes = { alice = \"home.toml\" }\n",
            "host",
        ),
        ("[hosts.laptop]\nhost = 3\n", "host"),
        ("[hosts.laptop\n", "config.toml"),
    ];
    for (index, key) in cases {
        let stop = index_stop(&machine, index);
        assert_eq!(stop.code, "E_CONFIG", "{index}");
        assert!(stop.message.contains(key), "{index}: {stop}");
    }
}

#[test]
fn config_toml_paths_that_leave_the_config_are_refused() {
    let machine = Machine::new("index-escape");
    for path in ["/etc/passwd", "../host.toml", "server/../../host.toml", ""] {
        for index in [
            format!("[hosts.laptop]\nhost = \"{path}\"\n"),
            format!("[hosts.laptop]\nhost = \"host.toml\"\nhomes = {{ alice = \"{path}\" }}\n"),
        ] {
            assert_eq!(
                index_stop(&machine, &index).code,
                "E_PATH_ESCAPE",
                "{index}"
            );
        }
    }
    let dir = machine.config("linked", &["home.toml"]);
    machine.write(
        "linked/config.toml",
        "[hosts.laptop]\nhost = \"out/host.toml\"\n",
    );
    machine.config("elsewhere", &["host.toml"]);
    std::os::unix::fs::symlink(machine.path("elsewhere"), dir.join("out")).unwrap();
    let found = config::find(Some(dir.as_os_str()), None, &machine.alice()).unwrap();
    let layout = config::read_layout(&dir, &found.user).unwrap();
    assert_eq!(layout.this_host().unwrap_err().code, "E_PATH_ESCAPE");
}

#[test]
fn a_named_file_that_is_missing_stops_when_selected_naming_it() {
    let machine = Machine::new("index-missing");
    let index = "[hosts.laptop]\nhost = \"host.toml\"\nhomes = { alice = \"gone/home.toml\" }\n\
                 [hosts.server]\nhost = \"server/host.toml\"\n";
    let dir = machine.config("dotfiles", &["host.toml"]);
    machine.write("dotfiles/config.toml", index);
    let found = config::find(Some(dir.as_os_str()), None, &machine.alice()).unwrap();
    let layout = config::read_layout(&dir, &found.user).unwrap();
    for (stop, missing) in [
        (layout.this_host().unwrap_err(), "gone/home.toml"),
        (
            layout.named_or_only(Some("server")).unwrap_err(),
            "server/host.toml",
        ),
    ] {
        assert_eq!(stop.code, "E_NO_MANIFEST");
        assert!(
            stop.message
                .contains(&dir.join(missing).display().to_string()),
            "{stop}"
        );
    }
}

#[test]
fn a_file_the_layout_selects_is_judged_like_its_folder() {
    let machine = Machine::new("file-trust");
    let dir = machine.config("home/alice/.config/lodi", &["host.toml"]);
    chmod(&dir.join("host.toml"), 0o664);
    let found = config::find(None, None, &machine.alice()).unwrap();
    let layout = config::read_layout(&dir, &found.user).unwrap();
    assert!(layout.this_host().is_ok(), "the private group may write it");
    machine.write("etc/group", &format!("staff:x:{}:\n", ids().1));
    let stop = layout.this_host().unwrap_err();
    assert_eq!(stop.code, "E_PATH_ESCAPE");
}

#[test]
fn under_doas_the_login_doas_user_names_is_used_and_sudo_uid_wins_over_it() {
    let machine = Machine::new("doas");
    let (uid, gid) = ids();
    let other = if uid == 4242 { 4243 } else { 4242 };
    machine.write(
        "etc/passwd",
        &format!(
            "root:x:0:0::/root:/bin/sh\nalice:x:{uid}:{gid}::/home/alice:/bin/sh\n\
             bob:x:{other}:{other}::/home/bob:/bin/sh\n"
        ),
    );
    let alices = machine.config("home/alice/.config/lodi", &["host.toml"]);
    machine.config("root/.config/lodi", &["host.toml"]);
    let mut doas = machine.sudo();
    doas.sudo_uid = None;
    doas.doas_user = Some("alice".into());
    let found = config::find(None, None, &doas).unwrap();
    assert_eq!(
        (local(&found), found.origin),
        (alices.as_path(), Origin::Standard)
    );
    assert_eq!(found.user.login.as_deref(), Some("alice"));
    let typed = config::find(Some(alices.as_os_str()), None, &doas).unwrap();
    config::remember(&typed).unwrap();
    assert!(machine.remembered().exists(), "remembered in alice's state");
    let mut both = machine.sudo();
    both.doas_user = Some("bob".into());
    let found = config::find(None, None, &both).unwrap();
    assert_eq!(found.user.login.as_deref(), Some("alice"), "SUDO_UID wins");
    doas.doas_user = Some("carol".into());
    let stop = config::find(None, None, &doas).unwrap_err();
    assert!(
        stop.message
            .contains(&machine.path("etc/passwd").display().to_string()),
        "{stop}"
    );
    let mut unprivileged = machine.alice();
    unprivileged.doas_user = Some("bob".into());
    let found = config::find(None, None, &unprivileged).unwrap();
    assert_eq!(
        found.user.login.as_deref(),
        Some("alice"),
        "only root acts for another"
    );
}
