//! A reconcile's changes, made to the person's own `host.toml` **in place** (LD-378).
//!
//! The file is parsed into a [`toml_edit::DocumentMut`] and only the elements that change are
//! touched; the document is never regenerated, so every comment, blank line, key order and
//! array layout a person gave it survives. What that takes:
//!
//! - an adopted name goes into the list the import writes (`common`, or `[packages.fedora]
//!   add` on Fedora) at its sorted position when the array is sorted, and at the end when it is
//!   not, in the array's own one-per-line layout;
//! - a removed name's trailing comment goes with it. `toml_edit` stores the comment after
//!   `"bat",` as the **next** element's prefix (or as the array's trailing text after the last
//!   one), so a plain removal would hand it to the wrong name;
//! - the `# NOT CAPTURED` block at the end of the file is Lodi's own text: it is regenerated when
//!   it is still there and never added back once a person deleted it;
//! - nothing to change is the input, byte for byte.

use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::diag::Diagnostic;

/// The first line of the block the emitter writes at the end of an imported manifest.
pub const NOT_CAPTURED: &str = "# NOT CAPTURED";

/// The lists a removal takes a name out of. `absent` and the `remove` lists say what the manifest
/// keeps off the machine, which a name that has left it already satisfies.
const LISTS: [&str; 4] = ["common", "hold", "mark_auto", "optional"];

/// One file table to write: a captured file's declaration, new or again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileKeys {
    pub path: String,
    /// Where its bytes travel, for a new declaration; an existing one keeps its own `source`.
    pub source: String,
    pub mode: u32,
    pub owner: String,
    pub group: String,
}

/// Everything a reconcile changes in the manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edits {
    pub adopt: Vec<String>,
    /// The per-distribution table an adopted name goes into, as the import writes it on that
    /// distribution (`fedora`: `[packages.fedora] add`); `None` is `[packages] common`.
    pub adopt_into: Option<&'static str>,
    pub remove: Vec<String>,
    pub hold_add: Vec<String>,
    pub hold_remove: Vec<String>,
    /// Existing declarations whose mode, owner and group are taken again from the machine.
    pub recapture: Vec<FileKeys>,
    /// New declarations.
    pub declare: Vec<FileKeys>,
    /// The regenerated `# NOT CAPTURED` block, from its first line to the end of the file.
    pub not_captured: Option<String>,
}

/// Apply `edits` to `text` and return the new text. A manifest that does not parse is refused;
/// the reconcile's caller has already validated it.
pub fn apply(text: &str, edits: &Edits) -> Result<String, Diagnostic> {
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        Diagnostic::new("E_SYNTAX", format!("host.toml does not parse: {e}"))
    })?;
    if !edits.remove.is_empty() || !edits.hold_remove.is_empty() {
        if let Some(packages) = doc.get_mut("packages").and_then(Item::as_table_like_mut) {
            for list in LISTS {
                if let Some(array) = packages.get_mut(list).and_then(Item::as_array_mut) {
                    for name in &edits.remove {
                        remove(array, name);
                    }
                    if list == "hold" {
                        for name in &edits.hold_remove {
                            remove(array, name);
                        }
                    }
                }
            }
            // The per-distribution and per-architecture `add` lists.
            for (_, item) in packages.iter_mut() {
                if let Some(table) = item.as_table_like_mut()
                    && let Some(array) = table.get_mut("add").and_then(Item::as_array_mut)
                {
                    for name in &edits.remove {
                        remove(array, name);
                    }
                }
            }
        }
    }
    if !edits.adopt.is_empty() {
        let array = match edits.adopt_into {
            Some(distro) => add_list_mut(&mut doc, distro),
            None => list_mut(&mut doc, "common"),
        };
        for name in &edits.adopt {
            add(array, name);
        }
    }
    if !edits.hold_add.is_empty() {
        let array = list_mut(&mut doc, "hold");
        for name in &edits.hold_add {
            add(array, name);
        }
    }
    for keys in &edits.recapture {
        if let Some(table) = file_table(&mut doc, &keys.path) {
            set(table, "mode", &format!("{:04o}", keys.mode));
            set(table, "owner", &keys.owner);
            set(table, "group", &keys.group);
        }
    }
    for keys in &edits.declare {
        declare(&mut doc, keys);
    }
    if let Some(block) = &edits.not_captured {
        let trailing = doc.trailing().as_str().unwrap_or_default().to_string();
        if let Some(at) = block_start(&trailing) {
            let mut regenerated = trailing[..at].to_string();
            regenerated.push_str(block);
            doc.set_trailing(regenerated);
        }
    }
    Ok(doc.to_string())
}

/// Where the `# NOT CAPTURED` block starts in the text after the last table, if it is there.
pub fn block_start(trailing: &str) -> Option<usize> {
    if trailing.starts_with(NOT_CAPTURED) {
        return Some(0);
    }
    trailing.find(&format!("\n{NOT_CAPTURED}")).map(|at| at + 1)
}

/// `[packages] <key>`, made as an empty one-per-line array when the file has none.
fn list_mut<'a>(doc: &'a mut DocumentMut, key: &str) -> &'a mut Array {
    if doc.get("packages").is_none() {
        doc["packages"] = Item::Table(Table::new());
    }
    let packages = doc["packages"]
        .as_table_like_mut()
        .expect("[packages] is a table");
    if packages.get(key).and_then(Item::as_array).is_none() {
        packages.insert(key, Item::Value(Value::Array(Array::new())));
    }
    packages
        .get_mut(key)
        .and_then(Item::as_array_mut)
        .expect("the list was just made")
}

/// `[packages.<distro>] add`, made as an empty one-per-line array when the file has none.
fn add_list_mut<'a>(doc: &'a mut DocumentMut, distro: &str) -> &'a mut Array {
    if doc.get("packages").is_none() {
        doc["packages"] = Item::Table(Table::new());
    }
    let packages = doc["packages"]
        .as_table_like_mut()
        .expect("[packages] is a table");
    if packages.get(distro).and_then(Item::as_table_like).is_none() {
        packages.insert(distro, Item::Table(Table::new()));
    }
    let table = packages
        .get_mut(distro)
        .and_then(Item::as_table_like_mut)
        .expect("the table was just made");
    if table.get("add").and_then(Item::as_array).is_none() {
        table.insert("add", Item::Value(Value::Array(Array::new())));
    }
    table
        .get_mut("add")
        .and_then(Item::as_array_mut)
        .expect("the list was just made")
}

/// Split a decor string at its first line end: the text up to and including the newline (a
/// trailing comment of whatever stands before it), and what follows.
fn split_line(decor: &str) -> (&str, &str) {
    match decor.find('\n') {
        Some(at) => (&decor[..=at], &decor[at + 1..]),
        None => (decor, ""),
    }
}

fn prefix_of(value: &Value) -> String {
    value
        .decor()
        .prefix()
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The indentation an array's elements are written with, when they are one per line.
fn indentation(array: &Array) -> Option<String> {
    array.iter().find_map(|value| {
        let prefix = prefix_of(value);
        prefix.rfind('\n').map(|at| prefix[at + 1..].to_string())
    })
}

/// Take `name` out of `array`, with its trailing comment.
fn remove(array: &mut Array, name: &str) {
    loop {
        let found = array.iter().position(|v| v.as_str() == Some(name));
        let Some(index) = found else {
            break;
        };
        let own = prefix_of(array.get(index).expect("the element"));
        array.remove(index);
        if index < array.len() {
            let next = array.get_mut(index).expect("the next element");
            let prefix = prefix_of(next);
            let rebuilt = if prefix.contains('\n') {
                // What stood on the removed line after it — its comment — leaves with it; the
                // next element takes the removed one's place, and keeps its own lines.
                let (_, rest) = split_line(&prefix);
                format!("{}{rest}", own.trim_end_matches([' ', '\t']))
            } else {
                own
            };
            next.decor_mut().set_prefix(rebuilt);
        } else {
            // The last element's comment is the array's trailing text, and the first line of
            // its own prefix is the comment of the element before it, which stays.
            let trailing = array.trailing().as_str().unwrap_or_default().to_string();
            let (line, rest) = split_line(&trailing);
            let (before, _) = split_line(&own);
            if line.contains('#') || before.contains('#') {
                let before = if before.contains('#') { before } else { "\n" };
                let rest = if line.contains('#') {
                    rest
                } else {
                    &trailing[line.len()..]
                };
                array.set_trailing(format!("{before}{rest}"));
            }
            if array.is_empty() {
                array.set_trailing("");
                array.set_trailing_comma(false);
            }
        }
    }
}

/// Put `name` into `array`: at its sorted place when the array is sorted, else at the end, in the
/// array's own layout. A name already there is left where it is.
fn add(array: &mut Array, name: &str) {
    if array.iter().any(|v| v.as_str() == Some(name)) {
        return;
    }
    let names: Vec<Option<&str>> = array.iter().map(Value::as_str).collect();
    let sorted = names.iter().all(Option::is_some) && names.windows(2).all(|w| w[0] <= w[1]);
    let index = if sorted {
        names
            .iter()
            .take_while(|n| n.is_some_and(|n| n < name))
            .count()
    } else {
        array.len()
    };
    let one_per_line = array.is_empty() || indentation(array).is_some();
    let indent = indentation(array).unwrap_or_else(|| "  ".to_string());
    let mut value = Value::from(name);
    if one_per_line {
        if index < array.len() {
            // The displaced element's first prefix line is the trailing comment of the one
            // before it, and stays with that one: the new element takes it, then its own line.
            let displaced = array.get_mut(index).expect("the displaced element");
            let prefix = prefix_of(displaced);
            let (line, rest) = split_line(&prefix);
            value.decor_mut().set_prefix(format!("{line}{indent}"));
            displaced.decor_mut().set_prefix(format!("\n{rest}"));
        } else {
            let trailing = array.trailing().as_str().unwrap_or_default().to_string();
            let (line, rest) = split_line(&trailing);
            let line = if line.contains('\n') { line } else { "\n" };
            value.decor_mut().set_prefix(format!("{line}{indent}"));
            array.set_trailing(format!("\n{rest}"));
            array.set_trailing_comma(true);
        }
        value.decor_mut().set_suffix("");
        array.insert_formatted(index, value);
    } else {
        value
            .decor_mut()
            .set_prefix(if index == 0 { "" } else { " " });
        array.insert_formatted(index, value);
        if index == 0
            && let Some(next) = array.get_mut(1)
        {
            next.decor_mut().set_prefix(" ");
        }
    }
}

fn file_table<'a>(doc: &'a mut DocumentMut, path: &str) -> Option<&'a mut Table> {
    doc.get_mut("files")?
        .as_table_mut()?
        .get_mut(path)?
        .as_table_mut()
}

/// Set a string key, keeping its decor, and only when its value changes.
fn set(table: &mut Table, key: &str, text: &str) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(value) if value.as_str() == Some(text) => {}
        Some(value) => {
            let decor = value.decor().clone();
            *value = Value::from(text);
            *value.decor_mut() = decor;
        }
        None => {
            table.insert(key, toml_edit::value(text));
        }
    }
}

fn declare(doc: &mut DocumentMut, keys: &FileKeys) {
    if doc.get("files").and_then(Item::as_table).is_none() {
        let mut files = Table::new();
        files.set_implicit(true);
        doc.insert("files", Item::Table(files));
    }
    let files = doc["files"].as_table_mut().expect("[files] is a table");
    let mut table = Table::new();
    table.insert("source", toml_edit::value(keys.source.as_str()));
    table.insert("mode", toml_edit::value(format!("{:04o}", keys.mode)));
    table.insert("owner", toml_edit::value(keys.owner.as_str()));
    table.insert("group", toml_edit::value(keys.group.as_str()));
    table.decor_mut().set_prefix("\n");
    files.insert(&keys.path, Item::Table(table));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    const IMPORTED: &str = "\
# Generated by lodi.

[host]
version = \"1\"
distro = \"debian\"

[packages]
# my own note
common = [
  \"bat\",
  \"jq\",
  \"tree\",
]

# NOT CAPTURED
#
# 0 package(s) are NOT declared above.
";

    #[test]
    fn nothing_to_change_is_the_input_byte_for_byte() {
        let edits = Edits {
            not_captured: Some(
                "# NOT CAPTURED\n#\n# 0 package(s) are NOT declared above.\n".into(),
            ),
            ..Edits::default()
        };
        assert_eq!(apply(IMPORTED, &edits).unwrap(), IMPORTED);
        assert_eq!(apply(IMPORTED, &Edits::default()).unwrap(), IMPORTED);
    }

    #[test]
    fn comments_order_and_layout_survive_an_adoption_and_a_removal() {
        let reordered = IMPORTED.replace("  \"bat\",\n  \"jq\",\n", "  \"jq\",\n  \"bat\",\n");
        let edits = Edits {
            adopt: names(&["zip"]),
            remove: names(&["tree"]),
            ..Edits::default()
        };
        let out = apply(&reordered, &edits).unwrap();
        // Unsorted, so zip goes at the end; the note and the order a person gave stay.
        let expected = reordered.replace("  \"tree\",\n", "  \"zip\",\n");
        assert_eq!(out, expected);
    }

    #[test]
    fn on_fedora_an_adopted_name_goes_into_the_list_the_import_writes() {
        let text = "[packages.fedora]\n# mine\nadd = [\n  \"bash\",\n  \"tree\",\n]\n";
        let edits = Edits {
            adopt: names(&["zip"]),
            adopt_into: Some("fedora"),
            ..Edits::default()
        };
        assert_eq!(
            apply(text, &edits).unwrap(),
            "[packages.fedora]\n# mine\nadd = [\n  \"bash\",\n  \"tree\",\n  \"zip\",\n]\n"
        );
    }

    #[test]
    fn an_adopted_name_goes_in_sorted_position_when_the_list_is_sorted() {
        let edits = Edits {
            adopt: names(&["curl", "aaa", "zzz"]),
            ..Edits::default()
        };
        let out = apply(IMPORTED, &edits).unwrap();
        let expected = IMPORTED.replace(
            "common = [\n  \"bat\",\n  \"jq\",\n  \"tree\",\n]",
            "common = [\n  \"aaa\",\n  \"bat\",\n  \"curl\",\n  \"jq\",\n  \"tree\",\n  \"zzz\",\n]",
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn a_removed_element_takes_its_trailing_comment_with_it() {
        let text =
            "[packages]\ncommon = [\n  \"bat\", # a better cat\n  \"jq\", # json\n  \"tree\",\n]\n";
        let edits = Edits {
            remove: names(&["bat"]),
            ..Edits::default()
        };
        assert_eq!(
            apply(text, &edits).unwrap(),
            "[packages]\ncommon = [\n  \"jq\", # json\n  \"tree\",\n]\n"
        );
        // The last element's comment is the array's trailing text.
        let text = "[packages]\ncommon = [\n  \"jq\", # json\n  \"tree\", # a tree\n]\n";
        let edits = Edits {
            remove: names(&["tree"]),
            ..Edits::default()
        };
        assert_eq!(
            apply(text, &edits).unwrap(),
            "[packages]\ncommon = [\n  \"jq\", # json\n]\n"
        );
        // A comment on a line of its own above a name is not the removed name's.
        let text = "[packages]\ncommon = [\n  \"bat\",\n  # tools\n  \"jq\",\n]\n";
        let edits = Edits {
            remove: names(&["bat"]),
            ..Edits::default()
        };
        assert_eq!(
            apply(text, &edits).unwrap(),
            "[packages]\ncommon = [\n  # tools\n  \"jq\",\n]\n"
        );
    }

    #[test]
    fn an_adoption_keeps_a_trailing_comment_on_its_own_name() {
        let text = "[packages]\ncommon = [\n  \"bat\", # a better cat\n  \"tree\",\n]\n";
        let edits = Edits {
            adopt: names(&["jq"]),
            ..Edits::default()
        };
        assert_eq!(
            apply(text, &edits).unwrap(),
            "[packages]\ncommon = [\n  \"bat\", # a better cat\n  \"jq\",\n  \"tree\",\n]\n"
        );
        let text = "[packages]\ncommon = [\n  \"bat\", # a better cat\n]\n";
        assert_eq!(
            apply(text, &edits).unwrap(),
            "[packages]\ncommon = [\n  \"bat\", # a better cat\n  \"jq\",\n]\n"
        );
    }

    #[test]
    fn a_removal_reaches_every_list_but_absent_and_remove() {
        let text = "[packages]\ncommon = [\"jq\", \"tree\"]\nhold = [\"tree\"]\nmark_auto = [\"tree\"]\n\
                    optional = [\"tree\"]\nabsent = [\"tree\"]\n\n[packages.debian]\nadd = [\"tree\"]\n\
                    remove = [\"tree\"]\n";
        let edits = Edits {
            remove: names(&["tree"]),
            ..Edits::default()
        };
        assert_eq!(
            apply(text, &edits).unwrap(),
            "[packages]\ncommon = [\"jq\"]\nhold = []\nmark_auto = []\noptional = []\nabsent = [\"tree\"]\n\n\
             [packages.debian]\nadd = []\nremove = [\"tree\"]\n"
        );
    }

    #[test]
    fn an_inline_list_stays_inline_and_a_missing_hold_is_made_one_per_line() {
        let text = "[packages]\ncommon = [\"jq\", \"tree\"]\n";
        let edits = Edits {
            adopt: names(&["bat"]),
            hold_add: names(&["jq"]),
            ..Edits::default()
        };
        assert_eq!(
            apply(text, &edits).unwrap(),
            "[packages]\ncommon = [\"bat\", \"jq\", \"tree\"]\nhold = [\n  \"jq\",\n]\n"
        );
    }

    #[test]
    fn the_not_captured_block_is_regenerated_when_there_and_never_re_added() {
        let edits = Edits {
            not_captured: Some(
                "# NOT CAPTURED\n#\n# 1 package(s) are NOT declared above.\n".into(),
            ),
            ..Edits::default()
        };
        let out = apply(IMPORTED, &edits).unwrap();
        assert_eq!(out, IMPORTED.replace("# 0 package(s)", "# 1 package(s)"));
        let deleted = IMPORTED
            .split("\n# NOT CAPTURED")
            .next()
            .unwrap()
            .to_string()
            + "\n";
        assert_eq!(apply(&deleted, &edits).unwrap(), deleted);
    }

    #[test]
    fn a_captured_file_is_declared_before_the_block_and_recaptured_in_place() {
        let text = "[packages]\ncommon = []\n\n[files.\"/etc/app.conf\"]\nsource = \"files/etc/app.conf\"\n\
                    mode = \"0644\" # as found\nowner = \"root\"\ngroup = \"root\"\n\n# NOT CAPTURED\n# none\n";
        let edits = Edits {
            recapture: vec![FileKeys {
                path: "/etc/app.conf".into(),
                source: String::new(),
                mode: 0o600,
                owner: "root".into(),
                group: "adm".into(),
            }],
            declare: vec![FileKeys {
                path: "/etc/new.conf".into(),
                source: "files/etc/new.conf".into(),
                mode: 0o644,
                owner: "root".into(),
                group: "root".into(),
            }],
            ..Edits::default()
        };
        assert_eq!(
            apply(text, &edits).unwrap(),
            "[packages]\ncommon = []\n\n[files.\"/etc/app.conf\"]\nsource = \"files/etc/app.conf\"\n\
             mode = \"0600\" # as found\nowner = \"root\"\ngroup = \"adm\"\n\n\
             [files.\"/etc/new.conf\"]\nsource = \"files/etc/new.conf\"\nmode = \"0644\"\nowner = \"root\"\n\
             group = \"root\"\n\n# NOT CAPTURED\n# none\n"
        );
    }
}
