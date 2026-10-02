//! The one-key rewrite of `host.toml` (V3–V7). Every function parses the text with `toml_edit`,
//! changes only the keys it names, and displays the document again, so every other byte — order,
//! spacing, comments — is kept. A key that is replaced keeps its own spacing and trailing
//! comment. A change that would not change the value returns the input unchanged, which is what
//! makes pinning twice byte-identical.

use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use crate::diag::Diagnostic;

/// What a rewrite produced: the whole new text, and whether it differs from the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub text: String,
    pub changed: bool,
}

impl Edit {
    fn unchanged(text: &str) -> Edit {
        Edit {
            text: text.to_string(),
            changed: false,
        }
    }
}

fn parse(text: &str) -> Result<DocumentMut, Diagnostic> {
    text.parse::<DocumentMut>().map_err(|e| {
        Diagnostic::new(
            "E_SYNTAX",
            format!(
                "host.toml is not valid TOML: {}",
                e.message().lines().next().unwrap_or("")
            ),
        )
    })
}

fn not_a_table(key: &str) -> Diagnostic {
    Diagnostic::new("E_TYPE", format!("`{key}` in host.toml is not a table"))
}

/// Replace `item`'s value by `new`, keeping the value's own decor (the spacing after `=` and a
/// trailing comment).
fn replace_value(item: &mut Item, new: &str) {
    let decor = item.as_value().map(|v| v.decor().clone());
    let mut value = Value::from(new);
    if let Some(decor) = decor {
        *value.decor_mut() = decor;
    }
    *item = Item::Value(value);
}

/// Set `[packages.pin] NAME = VALUE`, or remove that one key when `value` is `None`. The table is
/// created, last in the file and after one blank line, when the first pin needs it, and removed
/// with its last key.
pub fn set_pin(text: &str, name: &str, value: Option<&str>) -> Result<Edit, Diagnostic> {
    set_pin_in(text, None, name, value)
}

/// [`set_pin`] in `[packages.<distro>.pin]` when `distro` names one, the table a pin for one
/// distribution lives in. That table is never created: a pin goes there only when it already
/// holds the name.
pub fn set_pin_in(
    text: &str,
    distro: Option<&str>,
    name: &str,
    value: Option<&str>,
) -> Result<Edit, Diagnostic> {
    let mut doc = parse(text)?;
    let root = doc.as_table_mut();
    let packages = match root.get_mut("packages") {
        None => {
            if value.is_none() || distro.is_some() {
                return Ok(Edit::unchanged(text));
            }
            let mut table = Table::new();
            table.set_implicit(true);
            root.insert("packages", Item::Table(table));
            root.get_mut("packages").unwrap()
        }
        Some(item) => item,
    };
    let packages = packages
        .as_table_like_mut()
        .ok_or_else(|| not_a_table("packages"))?;
    let (owner, key): (&mut dyn TableLike, String) = match distro {
        None => (packages, "packages.pin".to_string()),
        Some(distro) => match packages.get_mut(distro) {
            None => return Ok(Edit::unchanged(text)),
            Some(item) => (
                item.as_table_like_mut()
                    .ok_or_else(|| not_a_table(&format!("packages.{distro}")))?,
                format!("packages.{distro}.pin"),
            ),
        },
    };
    match value {
        None => {
            let Some(pin) = owner.get_mut("pin") else {
                return Ok(Edit::unchanged(text));
            };
            let pin = pin.as_table_like_mut().ok_or_else(|| not_a_table(&key))?;
            if pin.remove(name).is_none() {
                return Ok(Edit::unchanged(text));
            }
            if pin.is_empty() {
                owner.remove("pin");
            }
        }
        Some(new) => {
            if owner.get("pin").is_none() {
                if distro.is_some() {
                    return Ok(Edit::unchanged(text));
                }
                let mut table = Table::new();
                table.set_position(usize::MAX);
                table.decor_mut().set_prefix("\n");
                owner.insert("pin", Item::Table(table));
            }
            let pin = owner
                .get_mut("pin")
                .and_then(Item::as_table_like_mut)
                .ok_or_else(|| not_a_table(&key))?;
            match pin.get_mut(name) {
                Some(item) if item.as_str() == Some(new) => return Ok(Edit::unchanged(text)),
                Some(item) => replace_value(item, new),
                None => {
                    pin.insert(name, Item::Value(Value::from(new)));
                }
            }
        }
    }
    Ok(finish(text, doc))
}

/// Set `[host] snapshot`, or remove it when `value` is `None`. A new key goes last in `[host]`.
pub fn set_snapshot(text: &str, value: Option<&str>) -> Result<Edit, Diagnostic> {
    let mut doc = parse(text)?;
    let root = doc.as_table_mut();
    if root.get("host").is_none() {
        if value.is_none() {
            return Ok(Edit::unchanged(text));
        }
        let mut table = Table::new();
        table.set_position(0);
        root.insert("host", Item::Table(table));
    }
    let host = root
        .get_mut("host")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| not_a_table("host"))?;
    match value {
        None => {
            if host.remove("snapshot").is_none() {
                return Ok(Edit::unchanged(text));
            }
        }
        Some(new) => match host.get_mut("snapshot") {
            Some(item) if item.as_str() == Some(new) => return Ok(Edit::unchanged(text)),
            Some(item) => replace_value(item, new),
            None => {
                host.insert("snapshot", Item::Value(Value::from(new)));
            }
        },
    }
    Ok(finish(text, doc))
}

/// `lodi unpin --all`: remove `[host] snapshot`, `[packages.pin]` and every
/// `[packages.<distro>.pin]`, and nothing else.
pub fn float_all(text: &str) -> Result<Edit, Diagnostic> {
    let mut doc = parse(text)?;
    let root = doc.as_table_mut();
    if let Some(host) = root.get_mut("host") {
        host.as_table_like_mut()
            .ok_or_else(|| not_a_table("host"))?
            .remove("snapshot");
    }
    if let Some(packages) = root.get_mut("packages") {
        let packages = packages
            .as_table_like_mut()
            .ok_or_else(|| not_a_table("packages"))?;
        packages.remove("pin");
        let keys: Vec<String> = packages.iter().map(|(k, _)| k.to_string()).collect();
        for key in keys {
            if let Some(table) = packages.get_mut(&key).and_then(Item::as_table_like_mut) {
                TableLike::remove(table, "pin");
            }
        }
    }
    Ok(finish(text, doc))
}

fn finish(text: &str, doc: DocumentMut) -> Edit {
    let new = doc.to_string();
    Edit {
        changed: new != text,
        text: new,
    }
}

/// The instant `lodi pin --all` sets when it is given no `--to`, as the import writes it:
/// the last UTC day that has ended. Arch publishes a day only then (LD-381); a dated apt index
/// of a more recent instant can still change after it is read, so its digest would not replay
/// (LD-444).
pub fn default_snapshot(distro: &str, now: i64) -> Result<String, Diagnostic> {
    match distro {
        "debian" | "ubuntu" | "arch" => Ok(crate::util::format_utc(
            crate::arch::base::latest_published_day(now),
        )),
        other => Err(Diagnostic::new(
            "E_UNSUPPORTED",
            format!("`{other}` has no dated archive this build pins to"),
        )),
    }
}
