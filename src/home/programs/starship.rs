//! `[programs.starship]` — `starship.toml` below the XDG configuration directory
//! (`docs/design/HOME_PROGRAMS.md` §6.10), and the `starship init` line in the shell files (§5).
//!
//! A TOML document, with a TOML `extra` merged canonically ([`super::compose`]). `shells` is not
//! written into it: it names the shell files of `[programs.bash]`, `[programs.zsh]` and
//! `[programs.fish]` that carry starship's `init` line, every declared one when it is absent. The
//! loader reads it ([`init_shells`]) before any module renders and hands the result to the shell
//! modules in [`RenderEnv::starship_init`]; each writes [`init_line`] after its typed lines and
//! before its snippets. The line is guarded by `command -v starship`, so a shell without starship
//! on `PATH` starts as before. `STARSHIP_CONFIG` is never set: the file is at starship's default
//! path (*unverified*).

use std::collections::BTreeSet;

use super::shell::Syntax;
use super::{Body, Fields, FileState, Program, RenderEnv, Rendered, set};
use crate::home::toml::{Table, Value};

/// The shell modules whose file can carry the `init` line, and the values `shells` takes.
pub const SHELLS: &[&str] = &["bash", "zsh", "fish"];

const INTEGERS: &[&str] = &["command_timeout", "scan_timeout"];
const CHARACTER: &[&str] = &["character.error_symbol", "character.success_symbol"];

#[derive(Debug, Clone, PartialEq)]
pub struct Starship {
    settings: Table,
}

impl Program for Starship {
    const NAME: &'static str = "starship";

    const STUB: &'static str = "\
# format = \"$all\"
# add_newline = true
# command_timeout = 500
# scan_timeout = 30
# character.success_symbol = \"[>](bold green)\"
# character.error_symbol = \"[>](bold red)\"
# shells = [\"bash\", \"zsh\", \"fish\"]
";

    fn read(fields: &mut Fields<'_>) -> Option<Starship> {
        let mut settings = Table::new();
        if let Some(format) = fields.string("format") {
            set(&mut settings, "format", Value::String(format));
        }
        if let Some(value) = fields.boolean("add_newline") {
            set(&mut settings, "add_newline", Value::Boolean(value));
        }
        for key in INTEGERS {
            if let Some(value) = fields.integer(key, 0..=i64::MAX) {
                set(&mut settings, key, Value::Integer(value));
            }
        }
        for key in CHARACTER {
            if let Some(value) = fields.string(key) {
                set(&mut settings, key, Value::String(value));
            }
        }
        // Validated here; what it selects is read by the loader through `init_shells`.
        fields.choices("shells", SHELLS);
        (!fields.failed()).then_some(Starship { settings })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        vec![Rendered {
            path: env.xdg_config_home.join("starship.toml"),
            body: Body::Toml(self.settings.clone()),
            state: FileState::Managed,
            takes_extra: true,
        }]
    }
}

/// The shells whose file carries the `init` line: those `shells` names, or — when it is absent —
/// every shell module the manifest declares. A name outside [`SHELLS`] is dropped here and
/// refused by [`Starship::read`].
pub fn init_shells(shells: Option<&[String]>, declared: &[&str]) -> BTreeSet<String> {
    let chosen = |name: &str| match shells {
        Some(list) => list.iter().any(|s| s == name),
        None => declared.contains(&name),
    };
    SHELLS
        .iter()
        .filter(|name| chosen(name))
        .map(|s| s.to_string())
        .collect()
}

/// The guarded line that starts starship in `shell`'s syntax, ending in a newline.
pub fn init_line(shell: &str, syntax: Syntax) -> String {
    match syntax {
        Syntax::Posix => format!(
            "if command -v starship >/dev/null 2>&1; then eval \"$(starship init {shell})\"; fi\n"
        ),
        Syntax::Fish => {
            "if command -v starship >/dev/null; starship init fish | source; end\n".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_default_to_every_declared_shell_module() {
        let names = |set: BTreeSet<String>| set.into_iter().collect::<Vec<_>>();
        assert_eq!(
            names(init_shells(None, &["zsh", "git", "bash"])),
            ["bash", "zsh"]
        );
        assert_eq!(names(init_shells(None, &["git"])), Vec::<String>::new());
        let fish = ["fish".to_string(), "tcsh".to_string()];
        assert_eq!(names(init_shells(Some(&fish), &["bash"])), ["fish"]);
        assert_eq!(
            names(init_shells(Some(&[]), &["bash"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_init_line_is_guarded_by_command_v() {
        assert_eq!(
            init_line("zsh", Syntax::Posix),
            "if command -v starship >/dev/null 2>&1; then eval \"$(starship init zsh)\"; fi\n"
        );
        assert!(init_line("fish", Syntax::Fish).starts_with("if command -v starship"));
    }
}
