//! `[programs.fish]` — `fish/conf.d/lodi.fish` below the XDG configuration directory
//! (`docs/design/HOME_PROGRAMS.md` §5, §6.4, decision D3).
//!
//! fish loads every `conf.d/*.fish` by itself, so this is an ordinary owned file — state record,
//! hash, drift, backup once, `on_remove` — and there is no line to print. It is not fish's own
//! start-up file, which Lodi never opens. Nothing here writes a universal variable: variables
//! are set with `-g`, and abbreviations are added with `--global`, which the fish releases that
//! kept abbreviations in universal variables took as the request not to.

use std::collections::BTreeMap;

use super::shell::{self, ShellCommon, Syntax};
use super::{Body, Comment, Fields, FileState, Program, RenderEnv, Rendered};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fish {
    shell: ShellCommon,
    abbrs: BTreeMap<String, String>,
    greeting: Option<String>,
}

impl Program for Fish {
    const NAME: &'static str = "fish";

    const STUB: &'static str = "\
# env = { EDITOR = \"vi\" }
# path = [\"~/.local/bin\"]
# aliases = { ll = \"ls -l\" }
# abbrs = { gs = \"git status\" }
# greeting = \"\"
# snippets = []
";

    fn read(fields: &mut Fields<'_>) -> Option<Fish> {
        let shell = ShellCommon::read(fields);
        let abbrs = fields.string_map("abbrs").unwrap_or_default();
        for (name, value) in &abbrs {
            let key = format!("abbrs.{name}");
            if !shell::is_alias_name(name) {
                fields.refuse(
                    &key,
                    "E_TYPE",
                    "is not an abbreviation name",
                    "an abbreviation name has no whitespace, `=`, quote, `/` or shell syntax \
                     character",
                );
            }
            if value.contains('\0') {
                fields.refuse(
                    &key,
                    "E_TYPE",
                    "holds a NUL character, which no shell word can carry",
                    "remove the NUL character",
                );
            }
        }
        let greeting = fields.string("greeting");
        if greeting.as_deref().is_some_and(|g| g.contains('\0')) {
            fields.refuse(
                "greeting",
                "E_TYPE",
                "holds a NUL character, which no shell word can carry",
                "remove the NUL character",
            );
        }
        (!fields.failed()).then_some(Fish {
            shell,
            abbrs,
            greeting,
        })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        let mut typed = Vec::new();
        for (name, value) in &self.abbrs {
            typed.push(format!(
                "abbr --add --global {name} {}",
                shell::fish_quote(value)
            ));
        }
        if let Some(greeting) = &self.greeting {
            typed.push(format!(
                "set -g fish_greeting {}",
                shell::fish_quote(greeting)
            ));
        }
        vec![Rendered {
            path: env.xdg_config_home.join("fish/conf.d/lodi.fish"),
            body: Body::Text {
                comment: Comment::Hash,
                text: shell::body(Syntax::Fish, Self::NAME, env, &self.shell, &typed),
            },
            state: FileState::Managed,
            takes_extra: true,
        }]
    }
}
