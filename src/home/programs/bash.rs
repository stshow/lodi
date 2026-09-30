//! `[programs.bash]` — the data-root file `home-scope/shell/init.bash`
//! (`docs/design/HOME_PROGRAMS.md` §5, §6.2), reached through the one line the user adds.

use std::path::PathBuf;

use super::shell::{self, ShellCommon, Syntax};
use super::{Body, Comment, Fields, FileState, Program, RenderEnv, Rendered};

const FILE: &str = "init.bash";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bash {
    shell: ShellCommon,
    histsize: Option<i64>,
    histfilesize: Option<i64>,
    histcontrol: Vec<String>,
    shopt: Vec<String>,
}

impl Program for Bash {
    const NAME: &'static str = "bash";

    const STUB: &'static str = "\
# env = { EDITOR = \"vi\" }
# path = [\"~/.local/bin\"]
# aliases = { ll = \"ls -l\" }
# histsize = 10000
# histfilesize = 20000
# histcontrol = [\"ignoredups\"]
# shopt = [\"histappend\"]
# snippets = []
";

    fn read(fields: &mut Fields<'_>) -> Option<Bash> {
        let shell = ShellCommon::read(fields);
        let histsize = fields.integer("histsize", 0..=i64::MAX);
        let histfilesize = fields.integer("histfilesize", 0..=i64::MAX);
        let histcontrol = fields.strings("histcontrol").unwrap_or_default();
        const CONTROL: &[&str] = &["ignorespace", "ignoredups", "erasedups"];
        for value in &histcontrol {
            if !CONTROL.contains(&value.as_str()) {
                fields.refuse(
                    "histcontrol",
                    "E_TYPE",
                    &format!("holds \"{value}\""),
                    &format!("each value must be one of: {}", CONTROL.join(", ")),
                );
            }
        }
        let shopt = shell::option_names(
            fields,
            "shopt",
            |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_',
            "a shopt option name is lower-case letters, digits and `_`",
        );
        (!fields.failed()).then_some(Bash {
            shell,
            histsize,
            histfilesize,
            histcontrol,
            shopt,
        })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        let mut typed = Vec::new();
        if let Some(n) = self.histsize {
            typed.push(format!("HISTSIZE={n}"));
        }
        if let Some(n) = self.histfilesize {
            typed.push(format!("HISTFILESIZE={n}"));
        }
        if !self.histcontrol.is_empty() {
            typed.push(format!("HISTCONTROL={}", self.histcontrol.join(":")));
        }
        for name in &self.shopt {
            typed.push(format!("shopt -s {name}"));
        }
        vec![Rendered {
            path: shell::data_file(env, FILE),
            body: Body::Text {
                comment: Comment::Hash,
                text: shell::body(Syntax::Posix, Self::NAME, env, &self.shell, &typed),
            },
            state: FileState::Managed,
            takes_extra: true,
        }]
    }

    fn hook(&self, env: &RenderEnv) -> Option<PathBuf> {
        Some(shell::data_file(env, FILE))
    }
}
