//! G5/G6: adversarial derivatives of the http-backend recording, never hand-authored fixtures.
use super::*;
fn objects() -> BTreeMap<String, Object> {
    let response = include_bytes!("../../tests/fixtures/git/tip/fetch-response.bin");
    let bytes = protocol::pack(response).unwrap();
    pack::read(&bytes, &Limits::at(PathBuf::from("unused"))).unwrap()
}
fn tree() -> (BTreeMap<String, Object>, String) {
    let all = objects();
    let commit = all.get("6e0a15fe0e5fac7e72246a8cf932db2d540bfd0f").unwrap();
    let id = std::str::from_utf8(&commit.data[5..45])
        .unwrap()
        .to_string();
    (all, id)
}
fn check(objects: &BTreeMap<String, Object>, id: &str, limits: &Limits) -> Result<(), GitError> {
    let mut used = BTreeSet::new();
    let mut files = Vec::new();
    let mut total = 0;
    let mut state = WalkState {
        used: &mut used,
        files: &mut files,
        total: &mut total,
        limits,
    };
    walk(objects, id, Path::new(""), &mut state, 0)
}
fn change_tree(change: impl FnOnce(&mut Vec<u8>)) -> (BTreeMap<String, Object>, String) {
    let (mut all, old) = tree();
    let mut new = all.remove(&old).unwrap();
    change(&mut new.data);
    let id = pack::id(2, &new.data).unwrap();
    all.insert(id.clone(), new);
    (all, id)
}
fn replace(bytes: &mut Vec<u8>, old: &[u8], new: &[u8]) {
    let i = bytes.windows(old.len()).position(|w| w == old).unwrap();
    bytes.splice(i..i + old.len(), new.iter().copied());
}
#[test]
fn g6_only_regular_git_modes_are_accepted() {
    let (all, tree) = tree();
    check(&all, &tree, &Limits::at(PathBuf::from("unused"))).unwrap();
    let (all, tree) = change_tree(|data| replace(data, b"100644", b"100755"));
    check(&all, &tree, &Limits::at(PathBuf::from("unused"))).unwrap();
    let (all, tree) = change_tree(|data| replace(data, b"100644", b"100664"));
    assert_eq!(
        check(&all, &tree, &Limits::at(PathBuf::from("unused")))
            .unwrap_err()
            .code,
        "E_UNSUPPORTED"
    );
}
#[test]
fn g6_links_submodules_unsafe_components_and_dot_git_are_refused() {
    for (mode, name, code, path) in [
        (Some(&b"120000"[..]), None, "E_PATH_ESCAPE", "data.txt"),
        (Some(&b"160000"[..]), None, "E_UNSUPPORTED", "data.txt"),
        (None, Some(&b".."[..]), "E_PATH_ESCAPE", ".."),
        (None, Some(&b".GIT"[..]), "E_PATH_ESCAPE", ".GIT"),
        (None, Some(&b"/escape"[..]), "E_PATH_ESCAPE", "/escape"),
    ] {
        let (all, tree) = change_tree(|data| {
            if let Some(mode) = mode {
                replace(data, b"100644", mode);
            }
            if let Some(name) = name {
                replace(data, b"data.txt", name);
            }
        });
        let err = check(&all, &tree, &Limits::at(PathBuf::from("unused"))).unwrap_err();
        assert_eq!(err.code, code, "G6 {path}");
        assert!(err.message.contains(path), "G6 {path}: {err}");
    }
}
#[test]
fn g6_manifests_files_and_whole_tree_have_independent_caps() {
    let (mut all, tree) = tree();
    let blob_id = all
        .iter()
        .find(|(_, obj)| obj.kind == 3 && obj.data == b"[host]\npackages = []\n")
        .unwrap()
        .0
        .clone();
    let blob = all.get_mut(&blob_id).unwrap();
    blob.data.resize(1024 * 1024 + 1, b'x');
    assert!(
        check(&all, &tree, &Limits::at(PathBuf::from("unused")))
            .unwrap_err()
            .message
            .contains("host.toml")
    );
    let (all, tree) = self::tree();
    let mut limit = Limits::at(PathBuf::from("unused"));
    limit.max_object_bytes = 3;
    assert!(
        check(&all, &tree, &limit)
            .unwrap_err()
            .message
            .contains("data.txt")
    );
    limit.max_object_bytes = 16 * 1024 * 1024;
    limit.max_tree_bytes = 3;
    assert!(
        check(&all, &tree, &limit)
            .unwrap_err()
            .message
            .contains("whole-tree cap")
    );
}
#[test]
fn g5_missing_and_wrong_object_are_hash_mismatch() {
    let (mut all, tree) = tree();
    all.remove(&tree);
    assert_eq!(
        check(&all, &tree, &Limits::at(PathBuf::from("unused")))
            .unwrap_err()
            .code,
        "E_HASH_MISMATCH"
    );
    let (mut all, tree) = self::tree();
    all.get_mut(&tree).unwrap().kind = 3;
    assert_eq!(
        check(&all, &tree, &Limits::at(PathBuf::from("unused")))
            .unwrap_err()
            .code,
        "E_HASH_MISMATCH"
    );
}
