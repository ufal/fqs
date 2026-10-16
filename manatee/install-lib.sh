#!/bin/sh
# Build libfqs_manatee (native Manatee for FQS; dev/MANATEE-NATIVE.md) from a manatee-open source
# tree, test it, and install it for FQS:
#
#   sh manatee/install-lib.sh --manatee-src DIR [--prefix /usr/local] [--fqs BIN] [--env FILE]
#
#   --manatee-src DIR  a manatee-open tree configured with the options KonText's Manatee was built
#                      with (./configure --with-pcre2 --disable-python); when its internal library
#                      (src/.libs/libbuiltinmanatee.a) is not built yet, the parts it needs are made
#   --prefix DIR       installs PREFIX/lib/libfqs_manatee.{so,dylib}       (default /usr/local)
#   --fqs BIN          the FQS that tests the new library                 (default PREFIX/bin/fqs)
#   --env FILE         FQS's environment file: FQS_MANATEE_LIB is added when not set
#                      (default /etc/fqs/fqs.env, macOS /usr/local/etc/fqs/fqs.env, if it exists)
#
# The library is built in a temporary folder and tested the way FQS uses it (a Manatee worker
# loads it and answers): only a library that loads completely and has the corpus check replaces the
# installed one (atomically). FQS uses it after a restart. install-stack.pl runs this script.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
src='' prefix=/usr/local fqs='' envf=''
while [ $# -gt 0 ]; do
	case "$1" in
		--manatee-src) src=$2; shift 2 ;;
		--prefix) prefix=$2; shift 2 ;;
		--fqs) fqs=$2; shift 2 ;;
		--env) envf=$2; shift 2 ;;
		-h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
		*) echo "unknown option $1 (--help)" >&2; exit 2 ;;
	esac
done
[ -n "$src" ] || { echo "--manatee-src DIR is required" >&2; exit 2; }
src=$(cd "$src" && pwd)
[ -f "$src/config.hh" ] || { echo "$src is not a configured manatee-open tree (no config.hh): run ./configure --with-pcre2 --disable-python there" >&2; exit 1; }
fqs=${fqs:-$prefix/bin/fqs}
[ -x "$fqs" ] || { echo "no FQS at $fqs (--fqs)" >&2; exit 1; }
case "$(uname -s)" in
	Darwin) ext=dylib; [ -n "$envf" ] || envf=/usr/local/etc/fqs/fqs.env
		command -v brew >/dev/null 2>&1 && LDFLAGS="${LDFLAGS:-} -L$(brew --prefix pcre2 2>/dev/null)/lib" && export LDFLAGS ;;
	*) ext=so; [ -n "$envf" ] || envf=/etc/fqs/fqs.env ;;
esac
grep -q "^#define USE_PCRE2 \|^#define USE_PCRE \|^#define USE_ICU " "$src/config.hh" \
	|| echo "warning: $src was configured without a regex library: wildcard queries ([lemma=\"un.*\"]) will match nothing (configure --with-pcre2)" >&2

# Manatee's internal library: only the parts libfqs_manatee needs (never api/, the Python wrapper)
if [ ! -f "$src/src/.libs/libbuiltinmanatee.a" ]; then
	echo "building Manatee's library in $src (a few minutes)"
	for d in hat-trie fsa3 finlib corp concord query src; do
		[ -d "$src/$d" ] && make -s -C "$src/$d" >/dev/null
	done
	[ -f "$src/src/.libs/libbuiltinmanatee.a" ] || { echo "make in $src did not produce src/.libs/libbuiltinmanatee.a" >&2; exit 1; }
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/fqs-manatee.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
cp "$here/build.sh" "$here/fqs_manatee.cc" "$tmp/"
MANATEE_SRC=$src sh "$tmp/build.sh" >"$tmp/build.log" 2>&1 || { cat "$tmp/build.log" >&2; echo "building libfqs_manatee failed" >&2; exit 1; }
new="$tmp/libfqs_manatee.$ext"
[ -f "$new" ] || { cat "$tmp/build.log" >&2; exit 1; }

# the test: a worker of this FQS loads it (all symbols resolved) and answers
hello=$(printf '{"op":"hello"}\n' | FQS_MANATEE_LIB="$new" "$fqs" manatee-worker 2>&1 || true)
case "$hello" in
	*'"ok":true'*'"check":true'*|*'"check":true'*'"ok":true'*) ;;
	*'"ok":true'*) echo "the new library loads but has no corpus check (an old fqs_manatee.cc?): not installed" >&2; exit 1 ;;
	*) echo "the new library does not load in an FQS worker: not installed" >&2; echo "$hello" >&2; exit 1 ;;
esac
version=$(printf '%s' "$hello" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')

dest="$prefix/lib/libfqs_manatee.$ext"
mkdir -p "$prefix/lib"
cp "$new" "$dest.new.$$" && chmod 755 "$dest.new.$$" && mv -f "$dest.new.$$" "$dest"
echo "installed: $version ($dest; Manatee from $src)"

if [ -f "$envf" ] && ! grep -q '^[[:space:]]*FQS_MANATEE_LIB=' "$envf"; then
	printf '\n# native Manatee (fqs/manatee/install-lib.sh)\nFQS_MANATEE_LIB=%s\n' "$dest" >>"$envf"
	echo "$envf: FQS_MANATEE_LIB added"
fi
