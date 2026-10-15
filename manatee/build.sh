#!/bin/sh
# Build libfqs_manatee (native Manatee for FQS; dev/MANATEE-NATIVE.md) against the manatee-open
# version KonText uses:
#
#   MANATEE_SRC=/path/to/manatee-open sh manatee/build.sh        (recommended)
#       a source tree, configured with the options KonText's Manatee was built with and built
#       (./configure --with-pcre2 --disable-python && make; --disable-python avoids SWIG).
#       Manatee's internal library libbuiltinmanatee.a (everything: finlib, corp, concord, query,
#       fsa3, hat-trie, the built-in dynamic attribute functions; position-independent) goes into
#       the library, which then needs no Manatee at runtime.
#
#   MANATEE_PREFIX=/usr/local MANATEE_SRC=/path/to/configured/tree sh manatee/build.sh
#       linked against an installed libmanatee instead. Careful: an installed libmanatee.so can be
#       incomplete (it uses fsa3's fsa_init without containing it; KonText's Python module supplies
#       it): FQS then refuses the library at start-up. Prefer the first way.
#
# → manatee/libfqs_manatee.{dylib,so}; point FQS at it with FQS_MANATEE_LIB.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
case "$(uname -s)" in
	Darwin) out="$here/libfqs_manatee.dylib"; sys="-liconv -ldl" ;;
	*) out="$here/libfqs_manatee.so"; sys="-ldl -lpthread" ;;
esac
CXX=${CXX:-c++}
flags="-std=gnu++11 -O2 -fPIC -shared -DHAVE_CONFIG_H"
src_inc() { echo "-I$1 -I$1/finlib -I$1/corp -I$1/concord -I$1/query -I$1/hat-trie/src"; }
has_lib() { for f in "$1"/libmanatee.so* "$1"/libmanatee*.dylib; do [ -e "$f" ] && return 0; done; return 1; }   # a shared libmanatee in folder $1
# the regex library Manatee was configured with (config.hh: USE_PCRE2 / USE_PCRE)
pcre() { if grep -q "^#define USE_PCRE2 " "$1/config.hh"; then echo "-lpcre2-8"; elif grep -q "^#define USE_PCRE " "$1/config.hh"; then echo "-lpcre"; fi; }

: "${MANATEE_SRC:?set MANATEE_SRC to a manatee-open source tree (configured; built unless MANATEE_PREFIX is set)}"
S=$(cd "$MANATEE_SRC" && pwd)
[ -f "$S/config.hh" ] || { echo "missing $S/config.hh: run ./configure in $S (with the options Manatee was built with)" >&2; exit 1; }
inc=$(src_inc "$S")
regex=$(pcre "$S")

if [ -n "${MANATEE_PREFIX:-}" ]; then
	P=$(cd "$MANATEE_PREFIX" && pwd)
	has_lib "$P/lib" || { echo "no libmanatee in $P/lib" >&2; exit 1; }
	$CXX $flags $inc "$here/fqs_manatee.cc" -L"$P/lib" -lmanatee -Wl,-rpath,"$P/lib" $regex $sys -o "$out"
	echo "built $out (against the installed $P/lib/libmanatee; if FQS refuses it at start-up, use the source tree)"
	exit 0
fi

lib="$S/src/.libs"
[ -f "$lib/libbuiltinmanatee.a" ] || { echo "missing $lib/libbuiltinmanatee.a: run make in $S" >&2; exit 1; }
if $CXX $flags $inc "$here/fqs_manatee.cc" "$lib/libbuiltinmanatee.a" $regex $sys -o "$out" 2>"$here/.build-static.log"; then
	rm -f "$here/.build-static.log"
	echo "built $out (Manatee linked in: $lib/libbuiltinmanatee.a)"
	exit 0
fi
echo "linking libbuiltinmanatee.a failed (see manatee/.build-static.log); trying the tree's shared libmanatee" >&2
has_lib "$lib" || { echo "no shared libmanatee in $lib either" >&2; exit 1; }
$CXX $flags $inc "$here/fqs_manatee.cc" -L"$lib" -lmanatee -Wl,-rpath,"$lib" $regex $sys -o "$out"
echo "built $out (against $lib/libmanatee, rpath recorded)"
