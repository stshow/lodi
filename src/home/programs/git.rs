//! `[programs.git]` — `git/config` and `git/ignore` below the XDG configuration directory
//! (`docs/design/HOME_PROGRAMS.md` §6.1).
//!
//! Every string is written double-quoted and escaped as git-config requires
//! ([`render::git_config`]); a value holding a newline, a NUL or another character git-config
//! has no escape for is `E_TYPE`. `user.email` is an ordinary typed string: the import never
//! fills it (§7), and nothing here reads one from anywhere else. `~/.gitconfig`, which git reads
//! after the XDG file so that its keys win, is `W_SHADOWED` (§4.6).

use std::collections::BTreeMap;

use super::{Body, Comment, Fields, FileState, Program, RenderEnv, Rendered, Shadow};
use crate::home::render::{self, GitSection, Scalar};

/// The typed string keys, in the order they are written: `(section, key)`.
const STRINGS: &[(&str, &str)] = &[
    ("user", "name"),
    ("user", "email"),
    ("init", "defaultBranch"),
    ("core", "editor"),
    ("core", "pager"),
];

/// The typed boolean keys.
const BOOLS: &[(&str, &str)] = &[("pull", "rebase"), ("push", "autoSetupRemote")];

/// The sections in the order they are written.
const SECTIONS: &[&str] = &["user", "init", "core", "pull", "push", "merge", "alias"];

#[derive(Debug, Clone, PartialEq)]
pub struct Git {
    /// `(section, key, value)` for every typed key that is set, `alias` included.
    keys: Vec<(&'static str, String, Scalar)>,
    ignores: Vec<String>,
}

/// Refuse a value git-config cannot carry on one line: a newline, a NUL, or a control character
/// it has no escape for.
fn one_line(fields: &mut Fields<'_>, key: &str, value: &str) -> bool {
    if value.contains(['\n', '\0']) || !render::git_representable(value) {
        fields.refuse(
            key,
            "E_TYPE",
            "holds a line break, a NUL or a control character, which git-config cannot carry",
            "write the value on one line, without control characters",
        );
        return false;
    }
    true
}

/// A git-config variable name: a letter, then letters, digits and `-`.
fn is_git_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

impl Program for Git {
    const NAME: &'static str = "git";

    // `user.email` is literally the line §7 gives: the import never fills an address.
    const STUB: &'static str = "\
# user.name = \"\"
# user.email = \"\"  # your address; lodi never imports it
# init.defaultBranch = \"main\"
# core.editor = \"vi\"
# core.pager = \"less\"
# pull.rebase = true
# push.autoSetupRemote = true
# merge.conflictStyle = \"zdiff3\"
# alias = { st = \"status\" }
# ignores = [\"*.swp\"]
";

    fn read(fields: &mut Fields<'_>) -> Option<Git> {
        let mut keys = Vec::new();
        for (section, key) in STRINGS {
            let dotted = format!("{section}.{key}");
            if let Some(value) = fields.string(&dotted)
                && one_line(fields, &dotted, &value)
            {
                keys.push((*section, (*key).to_string(), Scalar::Str(value)));
            }
        }
        for (section, key) in BOOLS {
            if let Some(value) = fields.boolean(&format!("{section}.{key}")) {
                keys.push((*section, (*key).to_string(), Scalar::Bool(value)));
            }
        }
        if let Some(style) = fields.choice("merge.conflictStyle", &["merge", "diff3", "zdiff3"]) {
            keys.push(("merge", "conflictStyle".into(), Scalar::Str(style)));
        }
        let alias: BTreeMap<String, String> = fields.string_map("alias").unwrap_or_default();
        for (name, value) in alias {
            let dotted = format!("alias.{name}");
            if !is_git_name(&name) {
                fields.refuse(
                    &dotted,
                    "E_TYPE",
                    "is not a git alias name",
                    "start with a letter; use only letters, digits and `-`",
                );
                continue;
            }
            if one_line(fields, &dotted, &value) {
                keys.push(("alias", name, Scalar::Str(value)));
            }
        }
        let ignores = fields.strings("ignores").unwrap_or_default();
        for pattern in &ignores {
            one_line(fields, "ignores", pattern);
        }
        (!fields.failed()).then_some(Git { keys, ignores })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        let sections: Vec<GitSection> = SECTIONS
            .iter()
            .map(|section| GitSection {
                name: (*section).to_string(),
                subsection: None,
                keys: self
                    .keys
                    .iter()
                    .filter(|(s, _, _)| s == section)
                    .map(|(_, key, value)| (key.clone(), value.clone()))
                    .collect(),
            })
            .filter(|section| !section.keys.is_empty())
            .collect();
        let dir = env.xdg_config_home.join("git");
        let mut out = vec![Rendered {
            path: dir.join("config"),
            body: Body::Text {
                comment: Comment::Hash,
                text: render::git_config(&sections),
            },
            state: FileState::Managed,
            takes_extra: true,
        }];
        if !self.ignores.is_empty() {
            let mut text = String::new();
            for pattern in &self.ignores {
                text.push_str(pattern);
                text.push('\n');
            }
            out.push(Rendered {
                path: dir.join("ignore"),
                body: Body::Text {
                    comment: Comment::Hash,
                    text,
                },
                state: FileState::Managed,
                takes_extra: false,
            });
        }
        out
    }

    fn shadowed_by(&self, env: &RenderEnv) -> Vec<Shadow> {
        vec![Shadow {
            by: env.home.join(".gitconfig"),
            of: env.xdg_config_home.join("git/config"),
        }]
    }
}
