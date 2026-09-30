# Managed by lodi from home.toml [programs.fish]. Edit home.toml, not this file:
# a hand edit here is drift, and `lodi home apply` stops on it.

if test -r "$HOME"/'.local/share/lodi/home-scope/profile.fish'; source "$HOME"/'.local/share/lodi/home-scope/profile.fish'; end
set -gx EDITOR 'nvim'
set -gx GOPATH "$HOME"/'go'
set -gx QUOTED 'it\'s a \\ backslash'
contains -- '/opt/example/bin' $PATH; or set -gx PATH '/opt/example/bin' $PATH
contains -- "$HOME"/'.local/bin' $PATH; or set -gx PATH "$HOME"/'.local/bin' $PATH
alias ll 'ls -l'
abbr --add --global gco 'git checkout'
abbr --add --global gs 'git status'
set -g fish_greeting ''
function fish_prompt
    echo (prompt_pwd) "> "
end
set -g fish_key_bindings fish_default_key_bindings
