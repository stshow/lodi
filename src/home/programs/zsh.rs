//! `[programs.zsh]` — the data-root file `home-scope/shell/init.zsh`
//! (`docs/design/HOME_PROGRAMS.md` §5, §6.3), reached through the one line the user adds.

use std::path::PathBuf;

use super::shell::{self, ShellCommon, Syntax};
use super::{Body, Comment, Fields, FileState, Program, RenderEnv, Rendered};

const FILE: &str = "init.zsh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zsh {
    shell: ShellCommon,
    histsize: Option<i64>,
    savehist: Option<i64>,
    setopt: Vec<String>,
}

impl Program for Zsh {
    const NAME: &'static str = "zsh";

    const STUB: &'static str = "\
# env = { EDITOR = \"vi\" }
# path = [\"~/.local/bin\"]
# aliases = { ll = \"ls -l\" }
# histsize = 10000
# savehist = 10000
# setopt = [\"share_history\"]
# snippets = []
";

    fn read(fields: &mut Fields<'_>) -> Option<Zsh> {
        let shell = ShellCommon::read(fields);
        let histsize = fields.integer("histsize", 0..=i64::MAX);
        let savehist = fields.integer("savehist", 0..=i64::MAX);
        let setopt = shell::option_names(
            fields,
            "setopt",
            |c| c.is_ascii_alphanumeric() || c == '_',
            "a zsh option name is letters, digits and `_`",
        );
        (!fields.failed()).then_some(Zsh {
            shell,
            histsize,
            savehist,
            setopt,
        })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        let mut typed = Vec::new();
        if let Some(n) = self.histsize {
            typed.push(format!("HISTSIZE={n}"));
        }
        if let Some(n) = self.savehist {
            typed.push(format!("SAVEHIST={n}"));
        }
        for name in &self.setopt {
            typed.push(format!("setopt {name}"));
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
