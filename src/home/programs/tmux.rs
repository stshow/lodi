//! `[programs.tmux]` — `tmux/tmux.conf` below the XDG configuration directory
//! (`docs/design/HOME_PROGRAMS.md` §6.9).
//!
//! One `set -g` line per typed key in the order of §6.9's table ([`render::tmux_lines`], which
//! quotes a string so that tmux expands nothing in it), `setw -g mode-keys` last, then `extra`
//! verbatim. A string with a line break or another control character is `E_TYPE`. The XDG path
//! needs tmux 3.1 or later (*unverified* per guest), and `~/.tmux.conf`, which tmux then loads
//! instead, is `W_SHADOWED` (§4.6).

use super::{Body, Comment, Fields, FileState, Program, RenderEnv, Rendered, Shadow, one_line};
use crate::home::render::{self, Scalar};

const BOOLS: &[&str] = &["mouse", "status"];
const INTEGERS: &[&str] = &["base-index", "escape-time", "history-limit"];

/// tmux keeps a number option in a C `int`.
const INT_MAX: i64 = i32::MAX as i64;

#[derive(Debug, Clone, PartialEq)]
pub struct Tmux {
    /// The `set -g` options, in the order they are written.
    options: Vec<(String, Scalar)>,
    mode_keys: Option<String>,
}

impl Program for Tmux {
    const NAME: &'static str = "tmux";

    const STUB: &'static str = "\
# prefix = \"C-a\"
# mouse = true
# status = true
# base-index = 1
# escape-time = 10
# history-limit = 10000
# default-terminal = \"tmux-256color\"
# mode-keys = \"vi\"
";

    fn read(fields: &mut Fields<'_>) -> Option<Tmux> {
        let mut options = Vec::new();
        let string = |fields: &mut Fields<'_>, key: &str| {
            let value = fields.string(key)?;
            if value.is_empty() {
                fields.refuse(key, "E_TYPE", "is empty", "name a key or a terminal");
                return None;
            }
            one_line(fields, key, &value).then_some(value)
        };
        if let Some(prefix) = string(fields, "prefix") {
            options.push(("prefix".to_string(), Scalar::Str(prefix)));
        }
        for key in BOOLS {
            if let Some(value) = fields.boolean(key) {
                options.push((key.to_string(), Scalar::Bool(value)));
            }
        }
        for key in INTEGERS {
            if let Some(value) = fields.integer(key, 0..=INT_MAX) {
                options.push((key.to_string(), Scalar::Int(value)));
            }
        }
        if let Some(terminal) = string(fields, "default-terminal") {
            options.push(("default-terminal".to_string(), Scalar::Str(terminal)));
        }
        let mode_keys = fields.choice("mode-keys", &["vi", "emacs"]);
        (!fields.failed()).then_some(Tmux { options, mode_keys })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        let mut text = render::tmux_lines(&self.options);
        if let Some(keys) = &self.mode_keys {
            text.push_str(&format!("setw -g mode-keys {keys}\n"));
        }
        vec![Rendered {
            path: env.xdg_config_home.join("tmux/tmux.conf"),
            body: Body::Text {
                comment: Comment::Hash,
                text,
            },
            state: FileState::Managed,
            takes_extra: true,
        }]
    }

    fn shadowed_by(&self, env: &RenderEnv) -> Vec<Shadow> {
        vec![Shadow {
            by: env.home.join(".tmux.conf"),
            of: env.xdg_config_home.join("tmux/tmux.conf"),
        }]
    }
}
