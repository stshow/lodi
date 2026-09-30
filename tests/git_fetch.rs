//! G2–G9: offline replay of git http-backend recordings on loopback.
#[path = "support/git_http.rs"]
mod git_http;
mod support;
use lodi::fetch::HttpFetcher;
use lodi::git::{GitRemote, HttpGitRemote, Limits};
use std::sync::atomic::Ordering;

#[test]
fn g1_the_measured_capabilities_replay_offline() {
    // The v2, shallow and SHA-1 capability the three forges were measured to have, through a
    // recorded smart-HTTP exchange: provenance, not a live-network gate.
    let server = git_http::Server::recorded("tip");
    let refs = client(&server).ls_refs(URL).unwrap();
    assert_eq!(refs.resolve(None).unwrap(), TIP);
    let (destination, _cleanup) = checkout_at("git-g1");
    let checkout = client(&server)
        .fetch_commit(URL, TIP, &Limits::at(destination))
        .expect("v2 fetch with deepen 1 and verified SHA-1 tree");
    assert_eq!(
        std::fs::read(checkout.tree.join("data.txt")).unwrap(),
        b"two\n"
    );
    assert_eq!(server.count.load(Ordering::SeqCst), 4);
}

fn fixture(file: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/git/tip/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}
fn replace(bytes: &mut [u8], before: &[u8], after: &[u8]) {
    assert_eq!(before.len(), after.len());
    let at = bytes
        .windows(before.len())
        .position(|w| w == before)
        .expect("in recorded bytes");
    bytes[at..at + before.len()].copy_from_slice(after);
}
fn client(server: &git_http::Server) -> HttpGitRemote {
    HttpGitRemote {
        fetcher: HttpFetcher::new(server.rewrite()),
    }
}
struct OwnDir(std::path::PathBuf);
impl Drop for OwnDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A checkout path, not made yet, inside a fresh scratch directory of this process
/// ([`support::scratch`], #350) that the returned guard removes.
fn checkout_at(name: &str) -> (std::path::PathBuf, OwnDir) {
    let root = support::scratch(name);
    (root.join("checkout"), OwnDir(root))
}

const URL: &str = "https://git.example.org/repo.git";
const TIP: &str = "6e0a15fe0e5fac7e72246a8cf932db2d540bfd0f";

#[test]
fn g2_g3_g4_advertisement_and_ls_refs_are_v2_post() {
    let server = git_http::Server::recorded("tip");
    let client = HttpGitRemote {
        fetcher: HttpFetcher::new(server.rewrite()),
    };
    let refs = client
        .ls_refs(URL)
        .expect("G2/G3/G4: recorded git http-backend must serve refs over POST");
    assert_eq!(
        refs.head.as_deref(),
        Some("refs/heads/main"),
        "G4 HEAD symref"
    );
    assert!(
        refs.names
            .iter()
            .any(|(name, id)| name == "refs/heads/main" && id == TIP),
        "G4 branch"
    );
    assert_eq!(
        server.count.load(Ordering::SeqCst),
        2,
        "G2 GET advertisement then POST ls-refs"
    );
}

#[test]
fn g5_g6_one_commit_is_sha1dc_verified_and_nar_hashed() {
    let server = git_http::Server::recorded("tip");
    let client = HttpGitRemote {
        fetcher: HttpFetcher::new(server.rewrite()),
    };
    let (destination, _cleanup) = checkout_at("git-one");
    let checkout = client
        .fetch_commit(URL, TIP, &Limits::at(destination.clone()))
        .expect("G5/G6: recorded commit must verify and materialise");
    assert_eq!(checkout.commit, TIP);
    assert_eq!(
        std::fs::read_to_string(checkout.tree.join("data.txt")).unwrap(),
        "two\n"
    );
    assert_eq!(checkout.nar_hash, lodi::nar::hash(&checkout.tree).unwrap());
}

#[test]
fn g2_post_redirect_is_refused_without_forwarding_body() {
    let server = git_http::Server::redirect_posts(
        fixture("advertisement.bin"),
        fixture("ls-response.bin"),
        fixture("fetch-response.bin"),
    );
    let error = client(&server).ls_refs(URL).unwrap_err();
    assert_eq!(error.code, "E_FETCH");
    assert!(error.message.contains("POST redirect"), "{error}");
    assert_eq!(server.count.load(Ordering::SeqCst), 2, "no redirected POST");
}

#[test]
fn g2_get_and_post_require_git_content_types() {
    let server = git_http::Server::with_types(
        fixture("advertisement.bin"),
        fixture("ls-response.bin"),
        fixture("fetch-response.bin"),
        Some("text/html"),
        None,
    );
    let error = client(&server).ls_refs(URL).unwrap_err();
    assert_eq!(error.code, "E_FETCH", "G2 wrong GET type");
    assert!(error.message.contains("Content-Type"));
    assert_eq!(
        server.count.load(Ordering::SeqCst),
        1,
        "G2 no POST after wrong GET type"
    );
    let server = git_http::Server::with_types(
        fixture("advertisement.bin"),
        fixture("ls-response.bin"),
        fixture("fetch-response.bin"),
        None,
        Some("text/html"),
    );
    let error = client(&server).ls_refs(URL).unwrap_err();
    assert_eq!(error.code, "E_FETCH", "G2 wrong POST type");
    assert!(error.message.contains("Content-Type"));
    assert_eq!(
        server.count.load(Ordering::SeqCst),
        2,
        "G2 POST was attempted"
    );
}

#[test]
fn g3_protocol_v2_shallow_sha1_and_pkt_length_are_required() {
    let refs = fixture("ls-response.bin");
    let pack = fixture("fetch-response.bin");
    for (before, after, needle) in [
        (&b"version 2"[..], &b"version 0"[..], "protocol version 2"),
        (&b"ls-refs"[..], &b"no-refs"[..], "ls-refs"),
        (&b"shallow"[..], &b"no-shal"[..], "shallow"),
    ] {
        let mut advert = fixture("advertisement.bin");
        replace(&mut advert, before, after);
        let server = git_http::Server::new(advert, refs.clone(), pack.clone());
        let error = client(&server).ls_refs(URL).unwrap_err();
        assert_eq!(error.code, "E_FETCH", "G3 {needle}");
        assert!(error.message.contains(needle), "G3 {needle}: {error}");
    }
    let mut advert = fixture("advertisement.bin");
    advert[0] = b'X';
    let server = git_http::Server::new(advert, refs, pack);
    assert_eq!(
        client(&server).ls_refs(URL).unwrap_err().code,
        "E_FETCH",
        "G3 pkt bounds"
    );
}

#[test]
fn g3_git_v2_without_object_format_advertisement_defaults_to_sha1() {
    let mut advert = fixture("advertisement.bin");
    let at = advert
        .windows(23)
        .position(|w| w == b"0017object-format=sha1\n")
        .unwrap();
    advert.drain(at..at + 23);
    let server = git_http::Server::new(
        advert,
        fixture("ls-response.bin"),
        fixture("fetch-response.bin"),
    );
    assert_eq!(
        client(&server).ls_refs(URL).unwrap().resolve(None).unwrap(),
        TIP,
        "G3 git v2's omitted object-format means sha1, not protocol failure"
    );
}

#[test]
fn g3_a_missing_terminal_flush_is_not_a_complete_exchange() {
    let mut advert = fixture("advertisement.bin");
    advert.truncate(advert.len() - 4);
    let server = git_http::Server::new(
        advert,
        fixture("ls-response.bin"),
        fixture("fetch-response.bin"),
    );
    assert_eq!(
        client(&server).ls_refs(URL).unwrap_err().code,
        "E_FETCH",
        "G3 truncated advertisement"
    );

    let mut refs = fixture("ls-response.bin");
    refs.truncate(refs.len() - 4);
    let server = git_http::Server::new(
        fixture("advertisement.bin"),
        refs,
        fixture("fetch-response.bin"),
    );
    assert_eq!(
        client(&server).ls_refs(URL).unwrap_err().code,
        "E_FETCH",
        "G3 truncated ls-refs"
    );

    let mut response = fixture("fetch-response.bin");
    response.truncate(response.len() - 4);
    let server = git_http::Server::new(
        fixture("advertisement.bin"),
        fixture("ls-response.bin"),
        response,
    );
    let (dir, _cleanup) = checkout_at("git-truncated-flush");
    assert_eq!(
        client(&server)
            .fetch_commit(URL, TIP, &Limits::at(dir))
            .unwrap_err()
            .code,
        "E_FETCH",
        "G3 truncated fetch response"
    );
}

#[test]
fn g3_sha256_advertisement_and_server_errors_are_named_without_control_bytes() {
    let mut advert = fixture("advertisement.bin");
    let at = advert
        .windows(23)
        .position(|w| w == b"0017object-format=sha1\n")
        .unwrap();
    advert.splice(at..at + 23, b"0019object-format=sha256\n".iter().copied());
    let server = git_http::Server::new(
        advert,
        fixture("ls-response.bin"),
        fixture("fetch-response.bin"),
    );
    let error = client(&server).ls_refs(URL).unwrap_err();
    assert_eq!(error.code, "E_FETCH");
    assert!(
        error.message.contains("sha256"),
        "G3 sha256 repository named"
    );

    let mut refs = fixture("ls-response.bin");
    refs.splice(0..0, b"000eERR bad\x1b!\n".iter().copied());
    let server = git_http::Server::new(
        fixture("advertisement.bin"),
        refs,
        fixture("fetch-response.bin"),
    );
    let error = client(&server).ls_refs(URL).unwrap_err();
    assert_eq!(error.code, "E_FETCH");
    assert!(
        !error.message.contains('\x1b') && error.message.contains("bad?!"),
        "G3 sanitised ERR"
    );

    let mut response = fixture("fetch-response.bin");
    let at = response.windows(2).position(|p| p == b"\x02E").unwrap();
    response[at] = 3;
    let server = git_http::Server::new(
        fixture("advertisement.bin"),
        fixture("ls-response.bin"),
        response,
    );
    let (dir, _cleanup) = checkout_at("git-sideband-three");
    let error = client(&server)
        .fetch_commit(URL, TIP, &Limits::at(dir))
        .unwrap_err();
    assert_eq!(error.code, "E_FETCH", "G3 side-band 3");
    assert!(error.message.contains("Enumerating objects"));
}

#[test]
fn g4_ambiguous_tag_peel_branch_and_unknown_ref() {
    let refs = client(&git_http::Server::recorded("tip"))
        .ls_refs(URL)
        .unwrap();
    assert_eq!(refs.resolve(None).unwrap(), TIP, "G4 HEAD");
    assert_eq!(refs.resolve(Some("main")).unwrap(), TIP, "G4 branch");
    assert_eq!(
        refs.resolve(Some("v1")).unwrap(),
        "e6c89d9b455f2126035656cb0474352dd7ac7b6e",
        "G4 annotated tag peeled"
    );
    let mut both = refs.clone();
    both.names.push(("refs/tags/main".into(), TIP.into()));
    let e = both.resolve(Some("main")).unwrap_err();
    assert_eq!(e.code, "E_NO_MATCH");
    assert!(e.message.contains("refs/heads/main") && e.message.contains("refs/tags/main"));
    let e = refs.resolve(Some("unknown")).unwrap_err();
    assert_eq!(e.code, "E_NO_MATCH");
    assert!(e.message.contains("refs/heads/main"));
}

#[test]
fn g4_unknown_ref_lists_nearest_not_first_eight_refs() {
    let mut refs = client(&git_http::Server::recorded("tip"))
        .ls_refs(URL)
        .unwrap();
    for n in 0..10 {
        refs.names.push((format!("refs/heads/zzz{n}"), TIP.into()));
    }
    refs.names.push(("refs/heads/neighbour".into(), TIP.into()));
    let error = refs.resolve(Some("neighbou")).unwrap_err();
    assert_eq!(error.code, "E_NO_MATCH");
    assert!(
        error.message.contains("refs/heads/neighbour"),
        "G4 nearest ref: {error}"
    );
}

#[test]
fn g6_nested_tree_directories_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let server = git_http::Server::recorded("nested");
    let (dest, _cleanup) = checkout_at("git-nested");
    let checkout = client(&server)
        .fetch_commit(
            URL,
            "708baa45e7db0f0dfb2e3fa9404a97d75e35fa84",
            &Limits::at(dest.clone()),
        )
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(checkout.tree.join("nested/file.txt")).unwrap(),
        "nested\n"
    );
    assert_eq!(
        std::fs::metadata(checkout.tree.join("nested"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "G6 nested tree dir must be 0700"
    );
}

#[test]
fn g5_reachable_non_tip_commit_is_fetched_with_deepen_one() {
    let server = git_http::Server::recorded("non-tip");
    let (destination, _cleanup) = checkout_at("git-non-tip");
    let rev = "e6c89d9b455f2126035656cb0474352dd7ac7b6e";
    let checkout = client(&server)
        .fetch_commit(URL, rev, &Limits::at(destination.clone()))
        .expect("G5 non-tip accepted by recorded git http-backend");
    assert_eq!(checkout.commit, rev);
    assert_eq!(
        std::fs::read_to_string(checkout.tree.join("data.txt")).unwrap(),
        "one\n"
    );
}

#[test]
fn g5_caller_limits_cannot_raise_absolute_ld400_caps() {
    let server = git_http::Server::recorded("tip");
    let (dir, _cleanup) = checkout_at("git-unbounded");
    let mut limits = Limits::at(dir);
    limits.max_object_bytes = usize::MAX;
    assert_eq!(
        client(&server)
            .fetch_commit(URL, TIP, &limits)
            .unwrap_err()
            .code,
        "E_UNSUPPORTED",
        "G5 fixed 16 MiB bound must not be raised by a caller"
    );
    assert_eq!(
        server.count.load(Ordering::SeqCst),
        0,
        "G5 reject invalid bounds before network"
    );
}

#[test]
fn g5_truncation_and_bit_flip_return_nothing() {
    for changed in [false, true] {
        let mut response = fixture("fetch-response.bin");
        if changed {
            let at = response.windows(4).position(|w| w == b"PACK").unwrap() + 50;
            response[at] ^= 1;
        } else {
            response.truncate(response.len() - 20);
        }
        let server = git_http::Server::new(
            fixture("advertisement.bin"),
            fixture("ls-response.bin"),
            response,
        );
        let (dir, _cleanup) = checkout_at(&format!("git-fault-{changed}"));
        assert!(!dir.exists());
        let error = client(&server)
            .fetch_commit(URL, TIP, &Limits::at(dir.clone()))
            .unwrap_err();
        assert!(
            matches!(error.code, "E_HASH_MISMATCH" | "E_FETCH"),
            "G5 {error}"
        );
        assert!(!dir.exists(), "G5 no checkout after faulty pack");
    }
}

#[test]
fn g9_client_is_internal_until_url_lane() {
    // gu-1 (LD-401) is the URL lane: the client is reached only through a host SOURCE URL,
    // which the help and docs/CLI.md now name, and never as a git verb of its own.
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_lodi"))
        // check-host-safety: refusal — the help of that command is read, and nothing is run.
        .args(["help", "host", "plan"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for text in [
        help.as_str(),
        include_str!("../docs/CLI.md"),
        include_str!("../README.md"),
    ] {
        assert!(!text.contains("git fetch"), "G9 no git reader verb");
    }
    assert!(
        help.contains("git+https://") && include_str!("../docs/CLI.md").contains("git+https://")
    );
    let verb = std::process::Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["git", "fetch"])
        .output()
        .unwrap();
    assert_eq!(verb.status.code(), Some(2), "G9 no git verb");
}

#[test]
fn g7_sha1dc_recomputes_a_recorded_blob_id() {
    let bytes = b"two\n"; // data.txt in the recorded tip
    let mut h = sha1collisiondetection::Sha1CD::default();
    h.update(format!("blob {}\0", bytes.len()));
    h.update(bytes);
    let digest = h
        .finalize_cd()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_eq!(
        digest, "f719efd430d52bcfc8566a43b2eb655688d38871",
        "G7 git's SHA-1DC blob identity"
    );
}

#[test]
fn g8_recorded_provenance_hashes_match_every_fixture_byte() {
    use sha2::{Digest, Sha256};
    for (case, commit, path, contents) in [
        ("tip", TIP, "data.txt", "two\n"),
        (
            "non-tip",
            "e6c89d9b455f2126035656cb0474352dd7ac7b6e",
            "data.txt",
            "one\n",
        ),
        (
            "nested",
            "708baa45e7db0f0dfb2e3fa9404a97d75e35fa84",
            "nested/file.txt",
            "nested\n",
        ),
    ] {
        let server = git_http::Server::recorded(case);
        let (destination, _cleanup) = checkout_at(&format!("git-g8-{case}"));
        let checkout = client(&server)
            .fetch_commit(URL, commit, &Limits::at(destination))
            .expect("G8 recorded GET and POST exchanges must yield verified checkout");
        assert_eq!(checkout.commit, commit);
        assert_eq!(
            std::fs::read_to_string(checkout.tree.join(path)).unwrap(),
            contents
        );
        assert_eq!(checkout.nar_hash, lodi::nar::hash(&checkout.tree).unwrap());
        assert_eq!(server.count.load(Ordering::SeqCst), 2);
        // The hashes alone do not establish behavior; retain them as a provenance check.
        let root = format!("{}/tests/fixtures/git/{case}", env!("CARGO_MANIFEST_DIR"));
        let provenance: serde_json::Value =
            serde_json::from_slice(&std::fs::read(format!("{root}/PROVENANCE")).unwrap()).unwrap();
        let rows = provenance["sha256"].as_object().unwrap();
        assert_eq!(rows.len(), 5, "G8 record every exchange file");
        for (name, hash) in rows {
            let data = std::fs::read(format!("{root}/{name}")).unwrap();
            let actual = Sha256::digest(&data)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            assert_eq!(actual, hash.as_str().unwrap(), "G8 {case}/{name}");
        }
    }
}
