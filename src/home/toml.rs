//! An owned, sorted TOML value: what a `[programs.<name>]` table is handed to a module as
//! (M-Home, `docs/design/HOME_PROGRAMS.md` §3.1).
//!
//! The manifest parser is `toml_edit`, pinned exactly because later releases parse TOML 1.1
//! (LD-14), and its tables carry spans and formatting a module has no business with. A module
//! instead reads this model, with the method names of the `toml` crate's own types; no
//! third-party `toml` crate is linked.
//!
//! [`Table`] is a `BTreeMap`, so every walk of it is in key order: a module that iterates a table
//! renders the same bytes whatever order the manifest wrote the keys in (invariant I1).

use std::collections::BTreeMap;
use std::fmt;

/// A TOML table: keys in lexicographic order.
pub type Table = BTreeMap<String, Value>;

/// A TOML array.
pub type Array = Vec<Value>;

/// A TOML date-time, kept as the text the manifest wrote. Nothing in the home scope interprets
/// a date, and nothing reads a clock.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Datetime(pub String);

impl fmt::Display for Datetime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One TOML value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Datetime(Datetime),
    Array(Array),
    Table(Table),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// A float, and only a float: an integer is not silently widened.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_datetime(&self) -> Option<&Datetime> {
        match self {
            Value::Datetime(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Array> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_table(&self) -> Option<&Table> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }

    pub fn is_str(&self) -> bool {
        self.as_str().is_some()
    }

    pub fn is_integer(&self) -> bool {
        self.as_integer().is_some()
    }

    pub fn is_float(&self) -> bool {
        self.as_float().is_some()
    }

    pub fn is_bool(&self) -> bool {
        self.as_bool().is_some()
    }

    pub fn is_array(&self) -> bool {
        self.as_array().is_some()
    }

    pub fn is_table(&self) -> bool {
        self.as_table().is_some()
    }

    /// A key of a table value, `None` for anything else.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_table().and_then(|t| t.get(key))
    }

    /// The type's name as the `toml` crate spells it, for a diagnostic.
    pub fn type_str(&self) -> &'static str {
        match self {
            Value::String(_) => "string",
            Value::Integer(_) => "integer",
            Value::Float(_) => "float",
            Value::Boolean(_) => "boolean",
            Value::Datetime(_) => "datetime",
            Value::Array(_) => "array",
            Value::Table(_) => "table",
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::String(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::String(s)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Integer(i)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Value {
        Value::Float(f)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Boolean(b)
    }
}

impl From<Table> for Value {
    fn from(t: Table) -> Value {
        Value::Table(t)
    }
}

impl From<Array> for Value {
    fn from(a: Array) -> Value {
        Value::Array(a)
    }
}

/// Parse a TOML document into the owned model: how a TOML `extra` string is read (§6). The error
/// is the parser's first line and the byte offset inside `text` it points at, so the caller can
/// place it inside the string.
pub fn parse(text: &str) -> Result<Table, (String, usize)> {
    match toml_edit::ImDocument::parse(text) {
        Ok(doc) => Ok(from_table_like(doc.as_table())),
        Err(e) => {
            let first = e.message().lines().next().unwrap_or("").trim().to_string();
            let offset = e.span().map_or(0, |span| span.start);
            Err((first, offset))
        }
    }
}

/// A `toml_edit` table, owned and sorted.
pub fn from_table_like(table: &dyn toml_edit::TableLike) -> Table {
    table
        .iter()
        .filter_map(|(key, item)| from_item(item).map(|v| (key.to_string(), v)))
        .collect()
}

/// A `toml_edit` item, owned; `None` for the empty item, which a parsed document never holds.
pub fn from_item(item: &toml_edit::Item) -> Option<Value> {
    match item {
        toml_edit::Item::None => None,
        toml_edit::Item::Value(value) => Some(from_value(value)),
        toml_edit::Item::Table(table) => Some(Value::Table(from_table_like(table))),
        toml_edit::Item::ArrayOfTables(array) => Some(Value::Array(
            array
                .iter()
                .map(|t| Value::Table(from_table_like(t)))
                .collect(),
        )),
    }
}

/// A `toml_edit` value, owned.
pub fn from_value(value: &toml_edit::Value) -> Value {
    match value {
        toml_edit::Value::String(s) => Value::String(s.value().clone()),
        toml_edit::Value::Integer(i) => Value::Integer(*i.value()),
        toml_edit::Value::Float(f) => Value::Float(*f.value()),
        toml_edit::Value::Boolean(b) => Value::Boolean(*b.value()),
        toml_edit::Value::Datetime(d) => Value::Datetime(Datetime(d.value().to_string())),
        toml_edit::Value::Array(a) => Value::Array(a.iter().map(from_value).collect()),
        toml_edit::Value::InlineTable(t) => Value::Table(from_table_like(t)),
    }
}
