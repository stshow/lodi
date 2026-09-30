//! The release assets check (scripts/release.sh inspect_paths) rejects any '/home/' in a
//! release binary other than the neutral /home/builder, so product code must not hold a
//! "/home/..." string literal: a default such as a new account's home directory is built
//! from its parts instead. This test finds such a literal at landing, not at the release pass.

use std::fs;
use std::path::Path;

fn walk(dir: &Path, found: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path).unwrap();
            // Unit tests at the end of a file are not built into the release binary.
            let product = text.split("#[cfg(test)]").next().unwrap_or("");
            let needle = ["\"", "home", ""].join("/");
            for (n, line) in product.lines().enumerate() {
                if line.contains(&needle) {
                    found.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                }
            }
        }
    }
}

#[test]
fn product_code_holds_no_home_path_literal() {
    let mut found = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut found,
    );
    assert!(
        found.is_empty(),
        "a \"/home/...\" literal in product code fails the release assets check; \
         build the path from its parts:\n{}",
        found.join("\n")
    );
}
