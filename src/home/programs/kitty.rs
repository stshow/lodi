//! `[programs.kitty]` — `kitty/kitty.conf` below the XDG configuration directory
//! (`docs/design/HOME_PROGRAMS.md` §6.8).
//!
//! kitty's `key value` lines ([`render::kv_lines`]): the typed keys in the order of §6.8's
//! table, then one `map <keys> <action>` line per mapping, sorted by keys, then `extra` verbatim.
//! A value is written as it is, so one that would not stay on its line — a line break or another
//! control character — is `E_TYPE`, and so is a key combination with a blank in it.

use std::collections::BTreeMap;

use super::{Body, Comment, Fields, FileState, Program, RenderEnv, Rendered, one_line};
use crate::home::render;

#[derive(Debug, Clone, PartialEq)]
pub struct Kitty {
    /// `(key, value)` in the order they are written, `map` lines last.
    lines: Vec<(String, String)>,
}

/// A number as kitty reads it: always with a fraction or an exponent (`12.0`, `0.5`).
fn number(value: f64) -> String {
    format!("{value:?}")
}

impl Program for Kitty {
    const NAME: &'static str = "kitty";

    const STUB: &'static str = "\
# font_family = \"monospace\"
# font_size = 11.0
# background_opacity = 1.0
# scrollback_lines = 10000
# enable_audio_bell = false
# confirm_os_window_close = 0
# map = { \"ctrl+shift+enter\" = \"new_window\" }
";

    fn read(fields: &mut Fields<'_>) -> Option<Kitty> {
        let mut lines = Vec::new();
        if let Some(family) = fields.string("font_family")
            && one_line(fields, "font_family", &family)
        {
            lines.push(("font_family".to_string(), family));
        }
        if let Some(size) = fields.number_in("font_size", 0.0, false, f64::INFINITY) {
            lines.push(("font_size".to_string(), number(size)));
        }
        if let Some(opacity) = fields.number_in("background_opacity", 0.0, true, 1.0) {
            lines.push(("background_opacity".to_string(), number(opacity)));
        }
        if let Some(n) = fields.integer("scrollback_lines", i64::MIN..=i64::MAX) {
            lines.push(("scrollback_lines".to_string(), n.to_string()));
        }
        if let Some(bell) = fields.boolean("enable_audio_bell") {
            let value = if bell { "yes" } else { "no" };
            lines.push(("enable_audio_bell".to_string(), value.to_string()));
        }
        if let Some(n) = fields.integer("confirm_os_window_close", i64::MIN..=i64::MAX) {
            lines.push(("confirm_os_window_close".to_string(), n.to_string()));
        }
        let map: BTreeMap<String, String> = fields.string_map("map").unwrap_or_default();
        for (keys, action) in map {
            let key = format!("map.{keys}");
            if keys.is_empty() || keys.chars().any(|c| c.is_whitespace() || c.is_control()) {
                fields.refuse(
                    &key,
                    "E_TYPE",
                    "is not a key combination",
                    "write the keys without blanks, such as `ctrl+shift+c`",
                );
                continue;
            }
            if one_line(fields, &key, &action) {
                lines.push(("map".to_string(), format!("{keys} {action}")));
            }
        }
        (!fields.failed()).then_some(Kitty { lines })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        vec![Rendered {
            path: env.xdg_config_home.join("kitty/kitty.conf"),
            body: Body::Text {
                comment: Comment::Hash,
                text: render::kv_lines(&self.lines, " "),
            },
            state: FileState::Managed,
            takes_extra: true,
        }]
    }
}
