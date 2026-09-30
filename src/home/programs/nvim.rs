//! `[programs.nvim]` — `nvim/init.lua` below the XDG configuration directory
//! (`docs/design/HOME_PROGRAMS.md` §6.5).
//!
//! Lua, with a `--` header. The typed keys render in a fixed order: the leader first, so that a
//! mapping in `extra` sees it; then the `vim.opt` settings sorted by name; then the colour scheme
//! through `pcall`, so that a scheme that is not installed does not break start-up. The scheme is
//! run as `vim.cmd("colorscheme " .. vim.fn.fnameescape(…))`, not `vim.cmd.colorscheme(…)`: the
//! indexable `vim.cmd` is Neovim 0.8, and Debian 12 ships 0.7.2, where the indexed form fails the
//! whole file (observed on a guest, LD-343); `fnameescape` keeps a `|` in the name from ending the
//! command. `extra` is Lua text, appended verbatim. Every string is a Lua literal ([`render::lua_value`]), so no value can
//! end the string it is written in. `nvim/init.vim` beside the rendered file is `W_SHADOWED`:
//! Neovim does not start cleanly with both (§4.6, *unverified*).

use super::{Body, Comment, Fields, FileState, Program, RenderEnv, Rendered, Shadow};
use crate::home::render;
use crate::home::toml::Value;

const BOOLS: &[&str] = &[
    "expandtab",
    "ignorecase",
    "number",
    "relativenumber",
    "smartcase",
    "termguicolors",
];
const INTEGERS: &[&str] = &["shiftwidth", "tabstop"];
const STRINGS: &[&str] = &["clipboard", "mouse"];

/// The largest `shiftwidth` or `tabstop` written: Vim keeps a number option in a C `int`.
const INT_MAX: i64 = i32::MAX as i64;

#[derive(Debug, Clone, PartialEq)]
pub struct Nvim {
    leader: Option<String>,
    /// `vim.opt.<name>` settings, sorted by name.
    opt: Vec<(String, Value)>,
    colorscheme: Option<String>,
}

impl Program for Nvim {
    const NAME: &'static str = "nvim";

    const STUB: &'static str = "\
# leader = \" \"
# colorscheme = \"habamax\"
# opt.number = true
# opt.relativenumber = false
# opt.expandtab = true
# opt.shiftwidth = 4
# opt.tabstop = 4
# opt.ignorecase = true
# opt.smartcase = true
# opt.termguicolors = true
# opt.clipboard = \"unnamedplus\"
# opt.mouse = \"a\"
";

    fn read(fields: &mut Fields<'_>) -> Option<Nvim> {
        let leader = fields.string("leader");
        let mut opt = Vec::new();
        for name in BOOLS {
            if let Some(value) = fields.boolean(&format!("opt.{name}")) {
                opt.push((name.to_string(), Value::Boolean(value)));
            }
        }
        for name in INTEGERS {
            if let Some(value) = fields.integer(&format!("opt.{name}"), 0..=INT_MAX) {
                opt.push((name.to_string(), Value::Integer(value)));
            }
        }
        for name in STRINGS {
            if let Some(value) = fields.string(&format!("opt.{name}")) {
                opt.push((name.to_string(), Value::String(value)));
            }
        }
        opt.sort_by(|a, b| a.0.cmp(&b.0));
        let colorscheme = fields.string("colorscheme");
        (!fields.failed()).then_some(Nvim {
            leader,
            opt,
            colorscheme,
        })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        let mut pairs = Vec::new();
        if let Some(leader) = &self.leader {
            pairs.push(("vim.g.mapleader".to_string(), Value::String(leader.clone())));
        }
        for (name, value) in &self.opt {
            pairs.push((format!("vim.opt.{name}"), value.clone()));
        }
        let mut text = render::lua_assignments(&pairs);
        if let Some(scheme) = &self.colorscheme {
            text.push_str(&format!(
                "pcall(vim.cmd, \"colorscheme \" .. vim.fn.fnameescape({}))\n",
                render::lua_value(&Value::String(scheme.clone()))
            ));
        }
        vec![Rendered {
            path: env.xdg_config_home.join("nvim/init.lua"),
            body: Body::Text {
                comment: Comment::DoubleDash,
                text,
            },
            state: FileState::Managed,
            takes_extra: true,
        }]
    }

    fn shadowed_by(&self, env: &RenderEnv) -> Vec<Shadow> {
        vec![Shadow {
            by: env.xdg_config_home.join("nvim/init.vim"),
            of: env.xdg_config_home.join("nvim/init.lua"),
        }]
    }
}
