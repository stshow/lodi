//! `[programs.helix]` — `helix/config.toml` below the XDG configuration directory
//! (`docs/design/HOME_PROGRAMS.md` §6.6).
//!
//! A TOML document: the typed keys as a table, with a TOML `extra` merged into it in canonical
//! key order, a key set both ways being `E_ATTR_CONFLICT` ([`super::compose`]). The executable
//! looked for on `PATH` is `hx`, the name the Helix project installs.

use super::{Body, Fields, FileState, Program, RenderEnv, Rendered, set};
use crate::home::toml::{Table, Value};

const BOOLS: &[&str] = &[
    "editor.auto-format",
    "editor.cursorline",
    "editor.mouse",
    "editor.true-color",
];
const SHAPES: &[&str] = &[
    "editor.cursor-shape.insert",
    "editor.cursor-shape.normal",
    "editor.cursor-shape.select",
];
const SHAPE_VALUES: &[&str] = &["block", "bar", "underline", "hidden"];

/// A ruler is a column number, which Helix keeps in 16 bits.
const RULER: std::ops::RangeInclusive<i64> = 1..=65_535;

#[derive(Debug, Clone, PartialEq)]
pub struct Helix {
    settings: Table,
}

impl Program for Helix {
    const NAME: &'static str = "helix";
    const EXECUTABLE: &'static str = "hx";

    const STUB: &'static str = "\
# theme = \"default\"
# editor.line-number = \"relative\"
# editor.auto-format = true
# editor.cursorline = true
# editor.mouse = true
# editor.true-color = true
# editor.rulers = [100]
# editor.cursor-shape.insert = \"bar\"
# editor.cursor-shape.normal = \"block\"
# editor.cursor-shape.select = \"underline\"
";

    fn read(fields: &mut Fields<'_>) -> Option<Helix> {
        let mut settings = Table::new();
        if let Some(theme) = fields.string("theme") {
            set(&mut settings, "theme", Value::String(theme));
        }
        if let Some(value) = fields.choice("editor.line-number", &["absolute", "relative"]) {
            set(&mut settings, "editor.line-number", Value::String(value));
        }
        for key in BOOLS {
            if let Some(value) = fields.boolean(key) {
                set(&mut settings, key, Value::Boolean(value));
            }
        }
        if let Some(rulers) = fields.integers("editor.rulers", RULER) {
            let rulers = rulers.into_iter().map(Value::Integer).collect();
            set(&mut settings, "editor.rulers", Value::Array(rulers));
        }
        for key in SHAPES {
            if let Some(value) = fields.choice(key, SHAPE_VALUES) {
                set(&mut settings, key, Value::String(value));
            }
        }
        (!fields.failed()).then_some(Helix { settings })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        vec![Rendered {
            path: env.xdg_config_home.join("helix/config.toml"),
            body: Body::Toml(self.settings.clone()),
            state: FileState::Managed,
            takes_extra: true,
        }]
    }
}
