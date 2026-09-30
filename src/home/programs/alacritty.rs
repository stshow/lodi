//! `[programs.alacritty]` — `alacritty/alacritty.toml` below the XDG configuration directory
//! (`docs/design/HOME_PROGRAMS.md` §6.7).
//!
//! A TOML document, with a TOML `extra` merged canonically ([`super::compose`]). A size or an
//! opacity is always written as a float (`12.0`), even when the manifest writes an integer. The
//! TOML format is the one alacritty reads from 0.13 on; an older alacritty reads YAML and ignores
//! this file (*unverified* per distribution; the release run records it per guest).

use super::{Body, Fields, FileState, Program, RenderEnv, Rendered, set};
use crate::home::toml::{Table, Value};

const PADDING: &[&str] = &["window.padding.x", "window.padding.y"];

#[derive(Debug, Clone, PartialEq)]
pub struct Alacritty {
    settings: Table,
}

impl Program for Alacritty {
    const NAME: &'static str = "alacritty";

    const STUB: &'static str = "\
# font.size = 11.0
# font.normal.family = \"monospace\"
# window.opacity = 1.0
# window.padding.x = 4
# window.padding.y = 4
# window.decorations = \"Full\"
# scrolling.history = 10000
";

    fn read(fields: &mut Fields<'_>) -> Option<Alacritty> {
        let mut settings = Table::new();
        if let Some(size) = fields.number_in("font.size", 0.0, false, f64::INFINITY) {
            set(&mut settings, "font.size", Value::Float(size));
        }
        if let Some(family) = fields.string("font.normal.family") {
            set(&mut settings, "font.normal.family", Value::String(family));
        }
        if let Some(opacity) = fields.number_in("window.opacity", 0.0, true, 1.0) {
            set(&mut settings, "window.opacity", Value::Float(opacity));
        }
        for key in PADDING {
            if let Some(value) = fields.integer(key, 0..=i64::from(i32::MAX)) {
                set(&mut settings, key, Value::Integer(value));
            }
        }
        if let Some(value) = fields.choice(
            "window.decorations",
            &["Full", "None", "Transparent", "Buttonless"],
        ) {
            set(&mut settings, "window.decorations", Value::String(value));
        }
        if let Some(value) = fields.integer("scrolling.history", 0..=100_000) {
            set(&mut settings, "scrolling.history", Value::Integer(value));
        }
        (!fields.failed()).then_some(Alacritty { settings })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        vec![Rendered {
            path: env.xdg_config_home.join("alacritty/alacritty.toml"),
            body: Body::Toml(self.settings.clone()),
            state: FileState::Managed,
            takes_extra: true,
        }]
    }
}
