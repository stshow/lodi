//! The home scope's privilege guard (M-1.0 S-1): a root process manages a home only when `HOME`
//! and the configuration root are root's own. The decision is a pure function of the effective
//! uid and the two owners, so it is tested here without a root process; `Roots::from_env` is
//! the one caller and feeds it the real values.

use std::path::Path;

use lodi::roots::privilege_guard;

#[test]
fn root_is_refused_a_home_that_is_not_its_own() {
    let home = Path::new("/srv/victim");
    let config = Path::new("/srv/attacker/lodi");
    // A user's home under root: refused, naming HOME.
    let error = privilege_guard(0, 1000, 0, home, config).expect_err("refused");
    assert_eq!(error.code, "E_CONFIG");
    assert!(
        error.message.contains("home directory") && error.message.contains("1000"),
        "{error}"
    );
    // Root's own home, but a configuration root someone else owns: refused, naming it.
    let error = privilege_guard(0, 0, 1000, home, config).expect_err("refused");
    assert_eq!(error.code, "E_CONFIG");
    assert!(error.message.contains("configuration directory"), "{error}");
    // Both someone else's: refused.
    assert!(privilege_guard(0, 1000, 1000, home, config).is_err());
}

#[test]
fn root_manages_only_a_root_owned_home_and_users_are_untouched() {
    let home = Path::new("/root");
    let config = Path::new("/root/.config/lodi");
    privilege_guard(0, 0, 0, home, config).expect("root's own home");
    // An ordinary user is never judged by ownership: the guard adds nothing for them.
    privilege_guard(1000, 1000, 1000, home, config).expect("a user's own home");
    privilege_guard(1000, 0, 0, home, config).expect("not root: not this guard's business");
    privilege_guard(1000, 1001, 0, home, config).expect("not root: not this guard's business");
}

/// The pure function above is only half the fix: a guard nothing calls is not a guard. This is
/// the wiring half, in the source-scan idiom of `tests/home_containment.rs` — `Roots::from_env`
/// is the one acquisition point every `home` verb goes through (`src/main.rs` has the three call
/// sites), so the call must be in its body, and the syscall wrapper must feed the decision the
/// effective uid rather than deciding anything itself. The negative control at the end proves the
/// scan really sees the call go missing.
#[test]
fn the_guard_is_wired_into_the_one_acquisition_point() {
    let source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/roots.rs"))
            .expect("src/roots.rs");

    let from_env = method_body(&source, "pub fn from_env() -> Result<Roots, Diagnostic> {");
    assert!(
        from_env.contains("check_privilege()"),
        "Roots::from_env must run the guard:\n{from_env}"
    );

    let check = method_body(
        &source,
        "fn check_privilege(&self) -> Result<(), Diagnostic> {",
    );
    assert!(
        check.contains("geteuid()"),
        "the guard is fed the real euid:\n{check}"
    );
    assert!(
        check.contains("privilege_guard("),
        "the wrapper decides nothing itself:\n{check}"
    );

    // The negative control: with the call taken out, the same scan must fail to find it.
    let unwired = source.replace("roots.check_privilege()?;", "// the call is gone");
    assert!(
        !method_body(&unwired, "pub fn from_env() -> Result<Roots, Diagnostic> {")
            .contains("check_privilege()"),
        "the scan must notice an unwired guard, or it proves nothing"
    );
}

/// The body of the method whose signature is `signature`, up to whatever starts the next item at
/// the same indentation. Deliberately simple; the negative control above is what keeps it honest.
fn method_body(source: &str, signature: &str) -> String {
    let at = source
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` is not in src/roots.rs"));
    let rest = &source[at + signature.len()..];
    let end = ["\n    pub fn ", "\n    fn ", "\n    /// "]
        .iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(rest.len());
    rest[..end].to_string()
}
