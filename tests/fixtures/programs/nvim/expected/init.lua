-- Managed by lodi from home.toml [programs.nvim]. Edit home.toml, not this file:
-- a hand edit here is drift, and `lodi home apply` stops on it.

vim.g.mapleader = " "
vim.opt.clipboard = "unnamedplus"
vim.opt.expandtab = true
vim.opt.ignorecase = true
vim.opt.mouse = "a"
vim.opt.number = true
vim.opt.relativenumber = true
vim.opt.shiftwidth = 4
vim.opt.smartcase = true
vim.opt.tabstop = 4
vim.opt.termguicolors = true
pcall(vim.cmd, "colorscheme " .. vim.fn.fnameescape("habamax"))
vim.keymap.set('n', '<leader>w', '<cmd>write<cr>')
