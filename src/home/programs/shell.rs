//! What the three shell modules share (`docs/design/HOME_PROGRAMS.md` §5): `env`, `path`,
//! `aliases` and `snippets`, their validation, the quoting, and the fixed rendering order.
//!
//! A shell file is sourced by the user's shell; nothing in it runs at render time. Values are
//! single-quoted, and a leading `~/` in an `env` value or a `path` entry is the one expansion
//! offered (`"$HOME"/…`). Lodi never opens a shell's own start-up file: bash and zsh get a file in
//! the data root and one printed line, fish a `conf.d` drop-in it loads by itself (decision D3).
//!
//! When `[tools]` declares a tool, each file includes the generated tool profile first: bash and
//! zsh source `profile.sh`, fish sources its fish twin, `profile.fish`, which sets the same bin
//! directory and environment in fish syntax.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{Fields, RenderEnv};
use crate::home::profile::{PROFILE_FISH, PROFILE_SH};

/// Where the bash and zsh files live below the data root.
pub const SHELL_DIR: &str = "home-scope/shell";

/// The shell a file is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syntax {
    /// bash and zsh: the POSIX forms both read the same way.
    Posix,
    Fish,
}

/// `env`, `path`, `aliases` and the bytes of `snippets`, validated.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShellCommon {
    pub env: BTreeMap<String, String>,
    pub path: Vec<String>,
    pub aliases: BTreeMap<String, String>,
    /// Each `snippets` file's text, in declared order, read by the loader at load time.
    pub snippets: Vec<String>,
}

impl ShellCommon {
    /// Read the four shared keys, refusing a name outside its grammar or a value no shell word
    /// can hold with `E_TYPE`; every problem is recorded in `fields`.
    pub fn read(fields: &mut Fields<'_>) -> ShellCommon {
        let env = fields.string_map("env").unwrap_or_default();
        for (name, value) in &env {
            let key = format!("env.{name}");
            if !is_var_name(name) {
                fields.refuse(
                    &key,
                    "E_TYPE",
                    "is not a variable name",
                    "a variable name is letters, digits and `_`, not starting with a digit",
                );
            }
            no_nul(fields, &key, value);
        }
        let aliases = fields.string_map("aliases").unwrap_or_default();
        for (name, value) in &aliases {
            let key = format!("aliases.{name}");
            if !is_alias_name(name) {
                fields.refuse(
                    &key,
                    "E_TYPE",
                    "is not an alias name",
                    "an alias name has no whitespace, `=`, quote, `/` or shell syntax character",
                );
            }
            no_nul(fields, &key, value);
        }
        let path = fields.strings("path").unwrap_or_default();
        for entry in &path {
            if entry.is_empty() || entry.contains([':', '\n', '\r', '\0']) {
                fields.refuse(
                    "path",
                    "E_TYPE",
                    &format!("holds {entry:?}, which is not a directory name"),
                    "one directory per entry, without `:` or a line break",
                );
            }
        }
        let mut snippets = Vec::new();
        for bytes in fields.files("snippets").unwrap_or_default() {
            match String::from_utf8(bytes) {
                Ok(mut text) => {
                    // The next line of the file starts on a line of its own.
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    snippets.push(text);
                }
                Err(_) => fields.refuse(
                    "snippets",
                    "E_CONFIG",
                    "names a file that is not UTF-8 text",
                    "a snippet is shell text, inlined into the rendered file",
                ),
            }
        }
        ShellCommon {
            env,
            path,
            aliases,
            snippets,
        }
    }
}

fn no_nul(fields: &mut Fields<'_>, key: &str, value: &str) {
    if value.contains('\0') {
        fields.refuse(
            key,
            "E_TYPE",
            "holds a NUL character, which no shell word can carry",
            "remove the NUL character",
        );
    }
}

/// The body of `shell`'s file, in the fixed order of §5 after the header: the tools; `env`
/// (sorted); `path`; `aliases` (sorted); the shell's own typed lines; starship's guarded `init`
/// line when [`RenderEnv::starship_init`] names `shell` (§6.10); each snippet. The loader
/// appends `extra`.
///
/// `path` keeps its declared order as the lookup order: each entry is prepended once, so the
/// lines run last-declared first and the first declared directory ends up first on `PATH`.
/// Sourcing the file twice changes nothing the first time did not.
pub fn body(
    syntax: Syntax,
    shell: &str,
    env: &RenderEnv,
    common: &ShellCommon,
    typed: &[String],
) -> String {
    let mut text = String::new();
    if env.tools {
        match syntax {
            Syntax::Posix => {
                let profile = home_word(env, &env.lodi_data_root.join(PROFILE_SH), syntax);
                text.push_str(&format!("if [ -r {profile} ]; then . {profile}; fi\n"));
            }
            Syntax::Fish => {
                let profile = home_word(env, &env.lodi_data_root.join(PROFILE_FISH), syntax);
                text.push_str(&format!("if test -r {profile}; source {profile}; end\n"));
            }
        }
    }
    for (name, value) in &common.env {
        let value = word(value, syntax);
        match syntax {
            Syntax::Posix => text.push_str(&format!("export {name}={value}\n")),
            Syntax::Fish => text.push_str(&format!("set -gx {name} {value}\n")),
        }
    }
    for entry in common.path.iter().rev() {
        let dir = word(entry, syntax);
        match syntax {
            Syntax::Posix => text.push_str(&format!(
                "case \":${{PATH-}}:\" in *:{dir}:*) ;; *) PATH={dir}${{PATH:+:$PATH}} ;; esac\n"
            )),
            Syntax::Fish => text.push_str(&format!(
                "contains -- {dir} $PATH; or set -gx PATH {dir} $PATH\n"
            )),
        }
    }
    if syntax == Syntax::Posix && !common.path.is_empty() {
        text.push_str("export PATH\n");
    }
    for (name, value) in &common.aliases {
        match syntax {
            Syntax::Posix => text.push_str(&format!("alias {name}={}\n", sh_quote(value))),
            Syntax::Fish => text.push_str(&format!("alias {name} {}\n", fish_quote(value))),
        }
    }
    for line in typed {
        text.push_str(line);
        text.push('\n');
    }
    if env.starship_init.contains(shell) {
        text.push_str(&super::starship::init_line(shell, syntax));
    }
    for snippet in &common.snippets {
        text.push_str(snippet);
    }
    text
}

/// A bash or zsh file's path in the data root.
pub fn data_file(env: &RenderEnv, name: &str) -> PathBuf {
    env.lodi_data_root.join(SHELL_DIR).join(name)
}

/// POSIX single quoting: nothing inside is expanded.
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// fish single quoting: only `\` and `'` are special inside.
pub fn fish_quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn quote(text: &str, syntax: Syntax) -> String {
    match syntax {
        Syntax::Posix => sh_quote(text),
        Syntax::Fish => fish_quote(text),
    }
}

/// A value in which a leading `~/` is the one expansion offered (§5): `"$HOME"/'rest'`, the
/// same in both syntaxes.
pub fn word(text: &str, syntax: Syntax) -> String {
    match text.strip_prefix("~/") {
        Some("") => "\"$HOME\"/".to_string(),
        Some(rest) => format!("\"$HOME\"/{}", quote(rest, syntax)),
        None => quote(text, syntax),
    }
}

/// A path Lodi owns, as a shell word: `"$HOME"/'rest'` below the home directory, so the rendered
/// bytes name no absolute path (I3); quoted as it is otherwise.
fn home_word(env: &RenderEnv, path: &Path, syntax: Syntax) -> String {
    match path.strip_prefix(&env.home) {
        Ok(rest) => format!("\"$HOME\"/{}", quote(&rest.to_string_lossy(), syntax)),
        Err(_) => quote(&path.to_string_lossy(), syntax),
    }
}

/// A shell variable name: `[A-Za-z_][A-Za-z0-9_]*`.
pub fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// An alias or abbreviation name: no whitespace, `=`, quote or `/` (§5), and no character a
/// shell reads as syntax before it reads the name, and not an option.
pub fn is_alias_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.chars().all(|c| {
            !c.is_whitespace()
                && !c.is_control()
                && !matches!(
                    c,
                    '=' | '\''
                        | '"'
                        | '`'
                        | '/'
                        | '\\'
                        | '$'
                        | ';'
                        | '&'
                        | '|'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '#'
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '~'
                        | '!'
                )
        })
}

/// A list of option names from one grammar (`shopt`, `setopt`), each checked by `ok`.
pub fn option_names(
    fields: &mut Fields<'_>,
    key: &str,
    ok: fn(char) -> bool,
    grammar: &str,
) -> Vec<String> {
    let names = fields.strings(key).unwrap_or_default();
    for name in &names {
        if name.is_empty() || !name.chars().all(ok) {
            fields.refuse(
                key,
                "E_TYPE",
                &format!("holds {name:?}, which is not an option name"),
                grammar,
            );
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_quote_everything_but_a_leading_tilde() {
        assert_eq!(
            word("~/.local/bin", Syntax::Posix),
            "\"$HOME\"/'.local/bin'"
        );
        assert_eq!(word("it's $x", Syntax::Posix), "'it'\"'\"'s $x'");
        assert_eq!(word("a\\b'c", Syntax::Fish), "'a\\\\b\\'c'");
        assert_eq!(word("~user/x", Syntax::Posix), "'~user/x'");
        assert_eq!(word("~/", Syntax::Fish), "\"$HOME\"/");
    }

    #[test]
    fn names_are_checked() {
        assert!(is_var_name("EDITOR") && is_var_name("_x1"));
        assert!(!is_var_name("1x") && !is_var_name("A-B") && !is_var_name(""));
        assert!(is_alias_name("ll") && is_alias_name("g.") && is_alias_name("k8s"));
        for bad in ["", "a b", "a=b", "a'b", "a/b", "a;b", "-x", "a\"b", "!x"] {
            assert!(!is_alias_name(bad), "{bad}");
        }
    }
}
