//! The internal ids a user must never meet in what lodi prints or writes: a design spec
//! reference, a section sign, a decision id (`LD-`, `ADR-`, `OD-`), a milestone name (`M-Pin`)
//! or a design call. The same set `scripts/check-user-docs.py` counts as UD16 in the source,
//! read here from the real output instead. Included by `#[path]`, like `hostroot.rs`.

/// Every internal id in `text`, in order.
pub fn internal_ids(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    if text.contains("design call") {
        found.push("design call".to_string());
    }
    let words = text.split(|c: char| !(c.is_ascii_alphanumeric() || "-/§.".contains(c)));
    for word in words {
        let digit_after = |prefix: &str| {
            word.strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        };
        let milestone = word.strip_prefix("M-").is_some_and(|rest| {
            let mut chars = rest.chars();
            chars.next().is_some_and(|c| c.is_ascii_uppercase())
                && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        });
        if word.contains('§')
            || ["spec/", "LD-", "ADR-", "OD-"]
                .iter()
                .any(|p| digit_after(p))
            || milestone
        {
            found.push(word.to_string());
        }
    }
    found
}
