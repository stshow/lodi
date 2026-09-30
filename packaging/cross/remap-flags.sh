#!/usr/bin/env sh
# The build-path remapping of every release build (LD-353).
#
#   sh packaging/cross/remap-flags.sh SRC
#
# prints the rustc flags that replace every absolute path of the build host rustc could bake
# into a binary (panic locations and other file!() strings of dependency crates, which live in
# the cargo registry below the builder's home) by a fixed neutral prefix:
#
#   the builder's home directory ($HOME)          -> /home/builder
#   the cargo home (${CARGO_HOME:-$HOME/.cargo})  -> /cargo
#   its registry directory                        -> /cargo/registry
#   SRC, the directory the build runs in           -> /src
#   the crate's own src/home/ module               -> src/lodi-home (relative, as cargo names it)
#                                  and SRC/src/home -> /src/src/lodi-home
#
# The last pair hides no host path: src/home/ is the home scope's module directory, not a home
# directory. It is mapped only so that no source location of the binary holds '/home/' at all,
# because the release build rejects every '/home/' that does not begin /home/builder and
# exempts none (LD-353).
#
# rustc applies the last --remap-path-prefix whose prefix matches, so the home mapping comes
# first and the more specific ones after it. A path is mapped as given and, when it differs, by
# its physical form too. The release build passes the output to both targets of both build
# passes; the prefixes are the same in every pass, which keeps the passes comparable byte for
# byte. The flags travel in a whitespace-separated cargo rustflags variable, so a path holding
# whitespace (or the '=' that separates a mapping) is refused rather than split.
set -u

[ "$#" -eq 1 ] || { echo "usage: sh packaging/cross/remap-flags.sh SRC" >&2; exit 2; }
src=$1
home=${HOME:?remap-flags: HOME is not set}
cargo_home=${CARGO_HOME:-$home/.cargo}

flags=
map() {
	case "$1" in
		/*) ;;
		*) echo "remap-flags: '$1' is not an absolute path" >&2; exit 1 ;;
	esac
	case "$1$2" in
		*[[:space:]=]*) echo "remap-flags: a mapped path holds whitespace or '='; it cannot be passed" >&2; exit 1 ;;
	esac
	flags="$flags --remap-path-prefix=$1=$2"
	physical=$(CDPATH= cd -- "$1" 2>/dev/null && pwd -P) || return 0
	[ "$physical" = "$1" ] || map "$physical" "$2"
}

# A home of / would map every path; there is nothing of the builder's to hide in it.
[ "$home" = / ] || map "$home" /home/builder
map "$cargo_home" /cargo
map "$cargo_home/registry" /cargo/registry
map "$src" /src
map "$src/src/home" /src/src/lodi-home
# cargo names a workspace member's files relative to the package root, so this one is relative.
flags="$flags --remap-path-prefix=src/home=src/lodi-home"
echo "${flags# }"
