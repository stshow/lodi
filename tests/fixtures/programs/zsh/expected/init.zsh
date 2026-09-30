# Managed by lodi from home.toml [programs.zsh]. Edit home.toml, not this file:
# a hand edit here is drift, and `lodi home apply` stops on it.

if [ -r "$HOME"/'.local/share/lodi/home-scope/profile.sh' ]; then . "$HOME"/'.local/share/lodi/home-scope/profile.sh'; fi
export EDITOR='nvim'
export GOPATH="$HOME"/'go'
case ":${PATH-}:" in *:"$HOME"/'.local/bin':*) ;; *) PATH="$HOME"/'.local/bin'${PATH:+:$PATH} ;; esac
export PATH
alias ll='ls -l'
HISTSIZE=10000
SAVEHIST=10000
setopt HIST_IGNORE_DUPS
setopt share_history
PROMPT='%n:%~%# '
bindkey -e
