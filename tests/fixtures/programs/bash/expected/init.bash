# Managed by lodi from home.toml [programs.bash]. Edit home.toml, not this file:
# a hand edit here is drift, and `lodi home apply` stops on it.

if [ -r "$HOME"/'.local/share/lodi/home-scope/profile.sh' ]; then . "$HOME"/'.local/share/lodi/home-scope/profile.sh'; fi
export EDITOR='nvim'
export GOPATH="$HOME"/'go'
export PAGER='less'
export QUOTED='it'"'"'s $literal'
case ":${PATH-}:" in *:'/opt/example/bin':*) ;; *) PATH='/opt/example/bin'${PATH:+:$PATH} ;; esac
case ":${PATH-}:" in *:"$HOME"/'go/bin':*) ;; *) PATH="$HOME"/'go/bin'${PATH:+:$PATH} ;; esac
case ":${PATH-}:" in *:"$HOME"/'.local/bin':*) ;; *) PATH="$HOME"/'.local/bin'${PATH:+:$PATH} ;; esac
export PATH
alias gs='git status'
alias la='ls -la'
alias ll='ls -l'
HISTSIZE=10000
HISTFILESIZE=20000
HISTCONTROL=ignorespace:erasedups
shopt -s histappend
shopt -s globstar
PS1='\u:\w\$ '
echo no trailing newline
bind '"\e[A": history-search-backward'
