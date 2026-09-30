//! Deterministic writers for the file formats the home program modules render
//! (M-Home, `docs/design/HOME_PROGRAMS.md` §4, §6, invariants I1 and I3).
//!
//! Each writer is a pure function of its argument: no clock, no environment, no file, no
//! locale, no hash-map order. Equal input gives byte-identical output, which the unit tests
//! below prove by rendering the same value built in two different insertion orders, and
//! `tests/fixtures/programs/` holds a golden file per format rendered through the whole pipeline.
//! None of them writes the ownership header: [`crate::home::programs::compose`] does, once.
//!
//! - [`toml_document`]: a TOML document with sorted keys, one fixed escaping, plain keys first
//!   and then one `[section]` per sub-table (helix, alacritty, starship).
//! - [`git_config`]: git's configuration format — `[section]` and `[section "subsection"]`
//!   headers, tab-indented `key = value` lines, every string double-quoted with git's escapes.
//! - [`kv_lines`]: one `key<sep>value` line per pair (kitty's space-separated form).
//! - [`tmux_lines`]: one `set -g option value` line per option, strings quoted as tmux reads
//!   them.
//! - [`lua_assignments`]: one `name = value` line per pair, values as Lua literals (Neovim).
//! - [`merge_toml`]: a TOML `extra` merged into a module's typed keys, refusing a key set twice.

use crate::home::toml::{Table, Value};

/// A TOML document: every table's keys in lexicographic order ([`Table`] is sorted), a table's
/// plain values before its sub-tables, a non-empty array whose elements are all tables as an
/// array of tables, and everything else inline. Strings are basic strings with one fixed escape
/// set; floats keep a fraction or an exponent so they read back as floats. The output ends with
/// a newline unless it is empty.
pub fn toml_document(t: &Table) -> String {
    let mut out = String::new();
    write_table(&mut out, &[], t, false);
    out
}

fn is_array_of_tables(value: &Value) -> bool {
    matches!(value, Value::Array(a) if !a.is_empty() && a.iter().all(Value::is_table))
}

fn is_section(value: &Value) -> bool {
    value.is_table() || is_array_of_tables(value)
}

/// Write the plain values of `t`, then its sections. `headed` says the caller already wrote the
/// header (an array-of-tables element).
fn write_table(out: &mut String, path: &[String], t: &Table, headed: bool) {
    let plain: Vec<(&String, &Value)> = t.iter().filter(|(_, v)| !is_section(v)).collect();
    let has_sections = t.values().any(is_section);
    if !path.is_empty() && !headed && (!plain.is_empty() || !has_sections) {
        blank(out);
        out.push('[');
        out.push_str(&dotted(path));
        out.push_str("]\n");
    }
    for (key, value) in plain {
        out.push_str(&key_text(key));
        out.push_str(" = ");
        write_inline(out, value);
        out.push('\n');
    }
    for (key, value) in t {
        let mut child = path.to_vec();
        child.push(key.clone());
        match value {
            Value::Table(sub) => write_table(out, &child, sub, false),
            Value::Array(items) if is_array_of_tables(value) => {
                for item in items {
                    let sub = item.as_table().expect("checked: every element is a table");
                    blank(out);
                    out.push_str("[[");
                    out.push_str(&dotted(&child));
                    out.push_str("]]\n");
                    write_table(out, &child, sub, true);
                }
            }
            _ => {}
        }
    }
}

/// One empty line before a header, except at the very start of the document.
fn blank(out: &mut String) {
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
}

fn dotted(path: &[String]) -> String {
    path.iter()
        .map(|k| key_text(k))
        .collect::<Vec<_>>()
        .join(".")
}

/// A key bare when TOML allows it bare, else as a basic string.
fn key_text(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if bare {
        key.to_string()
    } else {
        basic_string(key)
    }
}

/// A TOML basic string with the one escaping this writer uses: `\"`, `\\`, `\b`, `\t`, `\n`,
/// `\f`, `\r`, and `\uXXXX` for every other control character and DEL. Everything else,
/// non-ASCII included, is written as itself.
pub fn basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn float_text(f: f64) -> String {
    if f.is_nan() {
        "nan".to_string()
    } else if f.is_infinite() {
        if f > 0.0 { "inf" } else { "-inf" }.to_string()
    } else {
        // `Debug` always keeps a fraction or an exponent (`1.0`, `1e-7`, `1e20`), so the value
        // reads back as a float, and it is the shortest text that round-trips.
        format!("{f:?}")
    }
}

fn write_inline(out: &mut String, value: &Value) {
    match value {
        Value::String(s) => out.push_str(&basic_string(s)),
        Value::Integer(i) => out.push_str(&i.to_string()),
        Value::Float(f) => out.push_str(&float_text(*f)),
        Value::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Datetime(d) => out.push_str(&d.0),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_inline(out, item);
            }
            out.push(']');
        }
        Value::Table(t) => {
            if t.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{ ");
            for (i, (key, item)) in t.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&key_text(key));
                out.push_str(" = ");
                write_inline(out, item);
            }
            out.push_str(" }");
        }
    }
}

/// One value of a line-oriented format: what [`git_config`] and [`tmux_lines`] write.
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Bool(bool),
    Int(i64),
    Str(String),
}

/// Whether git's configuration format can hold `s` as a value: every character but NUL and the
/// control characters other than tab, newline and backspace, which have no escape in it. A module
/// refuses anything else (`E_TYPE`) before [`git_config`] is reached.
pub fn git_representable(s: &str) -> bool {
    !s.chars()
        .any(|c| (c.is_control() && !matches!(c, '\t' | '\n' | '\u{8}')) || c == '\u{7f}')
}

/// A git configuration value: a boolean or an integer bare, a string always double-quoted, with
/// `"`, `\\`, newline, tab and backspace escaped — so leading or trailing blanks, `#` and `;`
/// survive exactly.
fn git_value(value: &Scalar) -> String {
    match value {
        Scalar::Bool(b) => b.to_string(),
        Scalar::Int(i) => i.to_string(),
        Scalar::Str(s) => {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    '\u{8}' => out.push_str("\\b"),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
    }
}

/// One git configuration section: its name, an optional subsection, and its keys in the order
/// they are written.
#[derive(Debug, Clone, PartialEq)]
pub struct GitSection {
    pub name: String,
    pub subsection: Option<String>,
    pub keys: Vec<(String, Scalar)>,
}

/// A git configuration file, in the shape `git config` itself writes: a `[section]` or
/// `[section "subsection"]` header (the subsection with `"` and `\\` escaped) and one
/// `\tkey = value` line per key, **in the order given** — a module that wants sorted output
/// passes sorted input.
pub fn git_config(sections: &[GitSection]) -> String {
    let mut out = String::new();
    for section in sections {
        out.push('[');
        out.push_str(&section.name);
        if let Some(sub) = &section.subsection {
            out.push_str(" \"");
            for c in sub.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        }
        out.push_str("]\n");
        for (key, value) in &section.keys {
            out.push('\t');
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(&git_value(value));
            out.push('\n');
        }
    }
    out
}

/// A tmux option value: `on`/`off`, an integer bare, and a string single-quoted, which tmux
/// takes literally — or, when it holds a `'`, double-quoted with `\\`, `"`, `$` and `~` escaped
/// so that tmux expands nothing in it.
fn tmux_value(value: &Scalar) -> String {
    match value {
        Scalar::Bool(b) => if *b { "on" } else { "off" }.to_string(),
        Scalar::Int(i) => i.to_string(),
        Scalar::Str(s) if !s.contains('\'') => format!("'{s}'"),
        Scalar::Str(s) => {
            let mut out = String::from("\"");
            for c in s.chars() {
                if matches!(c, '\\' | '"' | '$' | '~') {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
            out
        }
    }
}

/// One `set -g <option> <value>` line per option, in the order given.
pub fn tmux_lines(options: &[(String, Scalar)]) -> String {
    let mut out = String::new();
    for (option, value) in options {
        out.push_str("set -g ");
        out.push_str(option);
        out.push(' ');
        out.push_str(&tmux_value(value));
        out.push('\n');
    }
    out
}

const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// Whether `name` is a Lua identifier, usable bare as a table key.
pub fn lua_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !LUA_KEYWORDS.contains(&name)
}

/// A Lua string literal: double-quoted, with `\\`, `"`, newline, carriage return and tab
/// escaped by name and every other control character as a three-digit decimal escape, which
/// every Lua since 5.1 reads.
fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_control() => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A TOML value as a Lua literal: a table's keys sorted, bare when they are identifiers and
/// `["…"]` otherwise; an array as a sequence; a date-time as its text.
pub fn lua_value(value: &Value) -> String {
    match value {
        Value::String(s) => lua_string(s),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) if f.is_nan() => "(0/0)".to_string(),
        Value::Float(f) if f.is_infinite() => {
            if *f > 0.0 { "math.huge" } else { "-math.huge" }.to_string()
        }
        Value::Float(f) => format!("{f:?}"),
        Value::Boolean(b) => b.to_string(),
        Value::Datetime(d) => lua_string(&d.0),
        Value::Array(items) if items.is_empty() => "{}".to_string(),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(lua_value).collect();
            format!("{{ {} }}", items.join(", "))
        }
        Value::Table(t) if t.is_empty() => "{}".to_string(),
        Value::Table(t) => {
            let items: Vec<String> = t
                .iter()
                .map(|(k, v)| {
                    if lua_identifier(k) {
                        format!("{k} = {}", lua_value(v))
                    } else {
                        format!("[{}] = {}", lua_string(k), lua_value(v))
                    }
                })
                .collect();
            format!("{{ {} }}", items.join(", "))
        }
    }
}

/// One `<name> = <value>` line per pair, in the order given. `name` is the module's (such as
/// `vim.opt.number`) and is written as it is.
pub fn lua_assignments(pairs: &[(String, Value)]) -> String {
    let mut out = String::new();
    for (name, value) in pairs {
        out.push_str(name);
        out.push_str(" = ");
        out.push_str(&lua_value(value));
        out.push('\n');
    }
    out
}

/// One `key<sep>value` line per pair, in the order given, each ending in a newline.
pub fn kv_lines(pairs: &[(String, String)], sep: &str) -> String {
    let mut out = String::new();
    for (key, value) in pairs {
        out.push_str(key);
        out.push_str(sep);
        out.push_str(value);
        out.push('\n');
    }
    out
}

/// Merge `extra` into `base` for the TOML formats (§6): tables merge key by key, and a key both
/// set is refused with its dotted path, which the caller reports as `E_ATTR_CONFLICT`.
pub fn merge_toml(base: &mut Table, extra: Table) -> Result<(), String> {
    merge_at(base, extra, &mut Vec::new())
}

fn merge_at(base: &mut Table, extra: Table, path: &mut Vec<String>) -> Result<(), String> {
    for (key, value) in extra {
        path.push(key.clone());
        match (base.get_mut(&key), value) {
            (None, value) => {
                base.insert(key, value);
            }
            (Some(Value::Table(have)), Value::Table(more)) => merge_at(have, more, path)?,
            (Some(_), _) => return Err(dotted(path)),
        }
        path.pop();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: Vec<(&str, Value)>) -> Table {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    fn sample(reversed: bool) -> Table {
        let mut pairs = vec![
            ("theme", Value::from("onedark")),
            ("zeta", Value::Integer(-3)),
            ("alpha", Value::Float(1.0)),
            ("tiny", Value::Float(1e-7)),
            ("on", Value::Boolean(true)),
            (
                "list",
                Value::Array(vec![Value::Integer(80), Value::Integer(120)]),
            ),
            (
                "editor",
                Value::Table(table(vec![
                    ("mouse", Value::Boolean(false)),
                    ("line-number", Value::from("relative")),
                    (
                        "cursor-shape",
                        Value::Table(table(vec![("insert", Value::from("bar"))])),
                    ),
                ])),
            ),
            (
                "odd key",
                Value::from("quote \" backslash \\ tab \t nl \n bell \u{7}"),
            ),
            (
                "servers",
                Value::Array(vec![
                    Value::Table(table(vec![("name", Value::from("a"))])),
                    Value::Table(table(vec![("name", Value::from("b"))])),
                ]),
            ),
            (
                "inline",
                Value::Array(vec![Value::Table(Table::new()), Value::Integer(1)]),
            ),
        ];
        if reversed {
            pairs.reverse();
        }
        table(pairs)
    }

    #[test]
    fn a_toml_document_is_byte_identical_for_equal_input_in_any_order() {
        let a = toml_document(&sample(false));
        let b = toml_document(&sample(true));
        assert_eq!(a, b);
        assert_eq!(a, toml_document(&sample(false)), "a second render differs");
        assert_eq!(
            a,
            "alpha = 1.0\n\
             inline = [{}, 1]\n\
             list = [80, 120]\n\
             \"odd key\" = \"quote \\\" backslash \\\\ tab \\t nl \\n bell \\u0007\"\n\
             on = true\n\
             theme = \"onedark\"\n\
             tiny = 1e-7\n\
             zeta = -3\n\
             \n\
             [editor]\n\
             line-number = \"relative\"\n\
             mouse = false\n\
             \n\
             [editor.cursor-shape]\n\
             insert = \"bar\"\n\
             \n\
             [[servers]]\n\
             name = \"a\"\n\
             \n\
             [[servers]]\n\
             name = \"b\"\n"
        );
    }

    #[test]
    fn a_toml_document_reads_back_as_the_same_value() {
        let original = sample(false);
        let text = toml_document(&original);
        let back = crate::home::toml::parse(&text).expect("the writer's output parses");
        assert_eq!(back, original);
        // And writing what was read gives the same bytes again.
        assert_eq!(toml_document(&back), text);
    }

    #[test]
    fn a_table_with_only_sub_tables_gets_no_empty_header_and_an_empty_one_does() {
        let t = table(vec![
            (
                "a",
                Value::Table(table(vec![(
                    "b",
                    Value::Table(table(vec![("c", Value::Integer(1))])),
                )])),
            ),
            ("empty", Value::Table(Table::new())),
        ]);
        assert_eq!(toml_document(&t), "[a.b]\nc = 1\n\n[empty]\n");
        assert_eq!(toml_document(&Table::new()), "");
    }

    #[test]
    fn special_floats_are_written_as_toml_spells_them() {
        assert_eq!(float_text(f64::NAN), "nan");
        assert_eq!(float_text(f64::INFINITY), "inf");
        assert_eq!(float_text(f64::NEG_INFINITY), "-inf");
        assert_eq!(float_text(0.5), "0.5");
        assert_eq!(float_text(12.0), "12.0");
    }

    #[test]
    fn git_config_quotes_every_string_and_escapes_what_git_escapes() {
        let sections = vec![
            GitSection {
                name: "user".into(),
                subsection: None,
                keys: vec![
                    ("name".into(), Scalar::Str("Your Name".into())),
                    ("email".into(), Scalar::Str("you at example.invalid".into())),
                ],
            },
            GitSection {
                name: "alias".into(),
                subsection: None,
                keys: vec![(
                    "odd".into(),
                    Scalar::Str(" lead ; # \"q\" \\ tab\t nl\n".into()),
                )],
            },
            GitSection {
                name: "url".into(),
                subsection: Some("a \"b\" \\c".into()),
                keys: vec![
                    ("insteadOf".into(), Scalar::Str("x".into())),
                    ("n".into(), Scalar::Int(-3)),
                    ("b".into(), Scalar::Bool(true)),
                ],
            },
        ];
        let text = git_config(&sections);
        assert_eq!(
            text,
            "[user]\n\tname = \"Your Name\"\n\temail = \"you at example.invalid\"\n\
             [alias]\n\todd = \" lead ; # \\\"q\\\" \\\\ tab\\t nl\\n\"\n\
             [url \"a \\\"b\\\" \\\\c\"]\n\tinsteadOf = \"x\"\n\tn = -3\n\tb = true\n"
        );
        assert_eq!(text, git_config(&sections.clone()));
        assert_eq!(git_config(&[]), "");
        assert!(git_representable("tab\tnl\n"));
        assert!(!git_representable("nul\0"));
        assert!(!git_representable("esc\u{1b}"));
    }

    #[test]
    fn tmux_lines_quote_so_that_tmux_expands_nothing() {
        let options = vec![
            ("mouse".to_string(), Scalar::Bool(true)),
            ("base-index".to_string(), Scalar::Int(1)),
            ("status-left".to_string(), Scalar::Str("$HOME ~ #S".into())),
            (
                "default-shell".to_string(),
                Scalar::Str("it's $x ~ \\".into()),
            ),
        ];
        assert_eq!(
            tmux_lines(&options),
            "set -g mouse on\n\
             set -g base-index 1\n\
             set -g status-left '$HOME ~ #S'\n\
             set -g default-shell \"it's \\$x \\~ \\\\\"\n"
        );
    }

    #[test]
    fn lua_assignments_write_lua_literals() {
        let t = table(vec![
            ("b", Value::Integer(2)),
            ("a", Value::from("x")),
            ("not-ident", Value::Boolean(false)),
            ("end", Value::Float(0.5)),
        ]);
        let pairs = vec![
            ("vim.opt.number".to_string(), Value::Boolean(true)),
            ("vim.g.mapleader".to_string(), Value::from(" \"\\\n\u{1b}")),
            (
                "vim.opt.list".to_string(),
                Value::Array(vec![Value::Integer(80), Value::Float(1e-7)]),
            ),
            ("vim.g.t".to_string(), Value::Table(t)),
            ("vim.g.e".to_string(), Value::Table(Table::new())),
        ];
        assert_eq!(
            lua_assignments(&pairs),
            "vim.opt.number = true\n\
             vim.g.mapleader = \" \\\"\\\\\\n\\027\"\n\
             vim.opt.list = { 80, 1e-7 }\n\
             vim.g.t = { a = \"x\", b = 2, [\"end\"] = 0.5, [\"not-ident\"] = false }\n\
             vim.g.e = {}\n"
        );
        assert!(lua_identifier("mapleader") && !lua_identifier("end") && !lua_identifier("1a"));
    }

    #[test]
    fn kv_lines_joins_each_pair_with_the_separator() {
        let pairs = vec![
            ("font_size".to_string(), "11.5".to_string()),
            ("map ctrl+c".to_string(), "copy".to_string()),
        ];
        assert_eq!(kv_lines(&pairs, " "), "font_size 11.5\nmap ctrl+c copy\n");
        assert_eq!(kv_lines(&pairs, " "), kv_lines(&pairs.clone(), " "));
        assert_eq!(kv_lines(&[], "="), "");
    }

    #[test]
    fn a_merge_refuses_a_key_set_twice_and_merges_tables() {
        let mut base = table(vec![(
            "editor",
            Value::Table(table(vec![("mouse", Value::Boolean(true))])),
        )]);
        let extra = table(vec![(
            "editor",
            Value::Table(table(vec![("bufferline", Value::from("always"))])),
        )]);
        merge_toml(&mut base, extra).expect("disjoint keys merge");
        assert_eq!(
            toml_document(&base),
            "[editor]\nbufferline = \"always\"\nmouse = true\n"
        );
        let clash = table(vec![(
            "editor",
            Value::Table(table(vec![("mouse", Value::Boolean(false))])),
        )]);
        assert_eq!(
            merge_toml(&mut base, clash),
            Err("editor.mouse".to_string())
        );
    }
}
