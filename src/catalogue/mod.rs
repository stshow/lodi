//! The built-in catalogue (design OD-08, ADR-020, LD-18, LD-76, LD-78): TOML recipes that say
//! where upstream publishes a tool, and the distro base definitions. No recipe lists versions or
//! packages; versions are discovered at lock time and the resolved result is copied into
//! `lodi.lock`.
//!
//! The data itself lives in the `catalogue/` directory at the repository root — under this
//! repository's own GPL-3.0-or-later licence like everything else — and is embedded by `build.rs`,
//! which walks `catalogue/tools/` and `catalogue/bases/` and generates [`BUILTIN_TOOLS`] and
//! [`BUILTIN_BASES`]. **No Rust file names a recipe**: adding one is adding a file. The embedded
//! text is the file itself, so the digest recorded in a lock is the digest of the file
//! (`catalogue/README.md`, LD-135).
//!
//! The recipe language is the subset `catalogue/README.md` describes normatively. Any unknown
//! key, unknown strategy or unknown template variable is `E_RECIPE_INVALID`, naming the file.
//! Parsing lives beside the shape it reads: [`recipe`] for tool recipes, [`base`] for base
//! definitions, and the version-discovery strategies in [`crate::upstream::strategy`].

pub mod base;
pub mod recipe;
pub mod user;

use std::collections::BTreeMap;

use toml_edit::{ImDocument, Item, TableLike};

use crate::diag::Diagnostic;

pub use base::{
    BaseDefinition, ReleaseDefinition, RepositoryDefinition, RootfsDefinition, parse_base,
};
pub use recipe::{Asset, ChecksumSource, Recipe, parse_recipe, parse_user_recipe};

include!(concat!(env!("OUT_DIR"), "/catalogue_builtin.rs"));

/// The built-in tool recipe `name`, if there is one.
pub fn builtin_recipe(name: &str) -> Option<Result<Recipe, Diagnostic>> {
    let file = format!("{name}.toml");
    BUILTIN_TOOLS
        .iter()
        .find(|(f, _)| *f == file)
        .map(|(f, text)| parse_recipe(text, f))
}

/// The built-in base definition for `distro`.
pub fn builtin_base(distro: &str) -> Option<Result<BaseDefinition, Diagnostic>> {
    let file = format!("{distro}.toml");
    BUILTIN_BASES
        .iter()
        .find(|(f, _)| *f == file)
        .map(|(f, text)| parse_base(text, f))
}

/// Every built-in tool recipe, parsed, sorted by name. `lodi search` and `lodi info` read the
/// whole catalogue rather than one name, so they take it through here (M-0.4 T-6).
pub fn builtin_recipes() -> Result<Vec<Recipe>, Diagnostic> {
    let mut recipes: Vec<Recipe> = BUILTIN_TOOLS
        .iter()
        .map(|(file, text)| parse_recipe(text, file))
        .collect::<Result<_, _>>()?;
    recipes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(recipes)
}

/// The tool names the built-in catalogue carries, sorted.
pub fn builtin_tool_names() -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_TOOLS
        .iter()
        .filter_map(|(file, _)| file.strip_suffix(".toml"))
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

/// Replace `${name}` with its value from `vars`; any other `${...}` is an error.
pub fn render(template: &str, vars: &[(&str, &str)]) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated `${{` in `{template}`"))?;
        let name = &after[..end];
        let value = vars
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
            .ok_or_else(|| format!("unknown variable `${{{name}}}` in `{template}`"))?;
        out.push_str(value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Template variables of a tool recipe for one architecture.
pub fn arch_vars(
    recipe_arch_names: &BTreeMap<String, String>,
    arch: &str,
) -> Vec<(String, String)> {
    let mut vars = vec![("arch".to_string(), arch.to_string())];
    let (alt, rust) = match arch {
        "x86_64" => ("amd64", "x86_64-unknown-linux-gnu"),
        "aarch64" => ("arm64", "aarch64-unknown-linux-gnu"),
        _ => ("", ""),
    };
    vars.push(("arch_alt".into(), alt.into()));
    vars.push(("arch_rust".into(), rust.into()));
    if let Some(name) = recipe_arch_names.get(arch) {
        vars.push(("arch_name".into(), name.clone()));
    }
    vars
}

/// A recipe file as a message names it: `catalogue recipe jq.toml` for a built-in one, `recipe
/// recipes/jq.toml` for one of the person's own, by its path from the repository's root.
pub fn described(file: &str) -> String {
    if file.starts_with(&format!("{}/", user::DIR)) {
        format!("recipe {file}")
    } else {
        format!("catalogue recipe {file}")
    }
}

/// The shared reader over one catalogue file: every refusal it makes is `E_RECIPE_INVALID`
/// naming that file. The strategy modules use it to parse their own `[versions]` keys.
pub struct Reader<'a> {
    file: &'a str,
}

impl<'a> Reader<'a> {
    pub fn new(file: &'a str) -> Self {
        Self { file }
    }

    pub fn invalid(&self, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(
            "E_RECIPE_INVALID",
            format!("{}: {}", described(self.file), message.into()),
        )
    }

    /// Refuse any key of `table` that is not in `allowed`.
    pub fn keys(
        &self,
        table: &dyn TableLike,
        at: &str,
        allowed: &[&str],
    ) -> Result<(), Diagnostic> {
        for (key, _) in table.iter() {
            if !allowed.contains(&key) {
                return Err(self.invalid(format!("unknown key `{key}` in [{at}]")));
            }
        }
        Ok(())
    }

    pub fn table<'t>(
        &self,
        table: &'t dyn TableLike,
        key: &str,
    ) -> Result<&'t dyn TableLike, Diagnostic> {
        table
            .get(key)
            .and_then(Item::as_table_like)
            .ok_or_else(|| self.invalid(format!("missing table [{key}]")))
    }

    pub fn string(&self, table: &dyn TableLike, key: &str, at: &str) -> Result<String, Diagnostic> {
        self.opt_string(table, key, at)?
            .ok_or_else(|| self.invalid(format!("missing `{key}` in [{at}]")))
    }

    pub fn opt_string(
        &self,
        table: &dyn TableLike,
        key: &str,
        at: &str,
    ) -> Result<Option<String>, Diagnostic> {
        match table.get(key) {
            None => Ok(None),
            Some(item) => item
                .as_str()
                .map(|s| Some(s.to_string()))
                .ok_or_else(|| self.invalid(format!("`{key}` in [{at}] must be a string"))),
        }
    }

    pub fn strings(
        &self,
        table: &dyn TableLike,
        key: &str,
        at: &str,
    ) -> Result<Vec<String>, Diagnostic> {
        let array = table.get(key).and_then(Item::as_array).ok_or_else(|| {
            self.invalid(format!("`{key}` in [{at}] must be an array of strings"))
        })?;
        array
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    self.invalid(format!("`{key}` in [{at}] must be an array of strings"))
                })
            })
            .collect()
    }

    pub fn integer(
        &self,
        table: &dyn TableLike,
        key: &str,
        at: &str,
        max: i64,
    ) -> Result<u32, Diagnostic> {
        match table.get(key).and_then(Item::as_integer) {
            Some(n) if (0..=max).contains(&n) => Ok(n as u32),
            _ => Err(self.invalid(format!(
                "`{key}` in [{at}] must be an integer from 0 to {max}"
            ))),
        }
    }

    /// Refuse a template that uses a `${...}` name outside `vars`.
    pub fn check_template(&self, template: &str, vars: &[&str]) -> Result<(), Diagnostic> {
        let dummy: Vec<(&str, &str)> = vars.iter().map(|v| (*v, "x")).collect();
        render(template, &dummy)
            .map(|_| ())
            .map_err(|e| self.invalid(e))
    }
}

pub(crate) fn parse_document<'t>(
    text: &'t str,
    reader: &Reader,
) -> Result<ImDocument<&'t str>, Diagnostic> {
    ImDocument::parse(text)
        .map_err(|e| reader.invalid(format!("TOML syntax: {}", e.message().trim())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// aarch64's alternate spellings (LD-409), and x86_64's unchanged.
    #[test]
    fn architecture_spellings() {
        let vars = |arch| {
            let names = BTreeMap::from([("x86_64".to_string(), "x64".to_string())]);
            arch_vars(&names, arch)
                .into_iter()
                .collect::<BTreeMap<String, String>>()
        };
        let x86 = vars("x86_64");
        assert_eq!(x86["arch_alt"], "amd64");
        assert_eq!(x86["arch_rust"], "x86_64-unknown-linux-gnu");
        assert_eq!(x86["arch_name"], "x64");
        assert_eq!(x86.len(), 4);
        let arm = vars("aarch64");
        assert_eq!(arm["arch"], "aarch64");
        assert_eq!(arm["arch_alt"], "arm64");
        assert_eq!(arm["arch_rust"], "aarch64-unknown-linux-gnu");
        assert!(!arm.contains_key("arch_name"));
    }

    #[test]
    fn templates() {
        assert_eq!(
            render("a${x}b${y}", &[("x", "1"), ("y", "2")]).unwrap(),
            "a1b2"
        );
        assert!(render("${z}", &[]).is_err());
        assert!(render("${x", &[("x", "1")]).is_err());
    }
}
