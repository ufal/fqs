#!/bin/sh
# Build libfqs_manatee (native Manatee for FQS; dev/MANATEE-NATIVE.md). Build it against the Manatee
# KonText uses (same version, same library). Two ways:
#
#   MANATEE_PREFIX=/usr/local sh manatee/build.sh
#       an installed manatee-open: headers in <prefix>/include/manatee, linked against the installed
#       <prefix>/lib/libmanatee (rpath recorded). If config.hh was not installed with the headers,
#       also set MANATEE_SRC to a manatee-open source tree of the same version, ./configure'd on this
#       machine (configure generates config.hh; no make needed).
#
#   MANATEE_SRC=/path/to/manatee-open-2.225.8 sh manatee/build.sh
#       a built source tree (configure && make): first statically (libmanatee.a and
#       libbuiltinmanatee.a into the library, nothing needed at runtime); where static archives are
#       not position-independent (Linux libtool builds), against the tree's shared libmanatee (rpath).
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

if [ -n "${MANATEE_PREFIX:-}" ]; then
	P=$(cd "$MANATEE_PREFIX" && pwd)
	hdr="$P/include/manatee"
	[ -f "$hdr/corpus.hh" ] || { echo "no Manatee headers in $hdr" >&2; exit 1; }
	ls "$P"/lib/libmanatee.so* "$P"/lib/libmanatee*.dylib >/dev/null 2>&1 || { echo "no libmanatee in $P/lib" >&2; exit 1; }
	inc="-I$hdr"
	if [ ! -f "$hdr/config.hh" ]; then
		[ -n "${MANATEE_SRC:-}" ] && [ -f "$MANATEE_SRC/config.hh" ] || {
			echo "config.hh was not installed in $hdr: set MANATEE_SRC to a manatee-open source tree of the same version, ./configure'd here" >&2
			exit 1
		}
		inc="-I$(cd "$MANATEE_SRC" && pwd) $inc"
	fi
	$CXX $flags $inc "$here/fqs_manatee.cc" -L"$P/lib" -lmanatee -Wl,-rpath,"$P/lib" $sys -o "$out"
	echo "built $out (against the installed $P/lib/libmanatee)"
	exit 0
fi

: "${MANATEE_SRC:?set MANATEE_PREFIX (an installed Manatee) or MANATEE_SRC (a built source tree)}"
M=$(cd "$MANATEE_SRC" && pwd)
[ -f "$M/config.hh" ] || { echo "missing $M/config.hh: build manatee-open first (configure && make)" >&2; exit 1; }
inc=$(src_inc "$M")
if [ -f "$M/src/.libs/libmanatee.a" ] && [ -f "$M/src/.libs/libbuiltinmanatee.a" ] &&
	$CXX $flags $inc "$here/fqs_manatee.cc" "$M/src/.libs/libmanatee.a" "$M/src/.libs/libbuiltinmanatee.a" $sys -o "$out" 2>"$here/.build-static.log"; then
	rm -f "$here/.build-static.log"
	echo "built $out (Manatee linked in statically)"
	exit 0
fi
echo "static link not possible here (see manatee/.build-static.log); linking the tree's shared libmanatee" >&2
lib="$M/src/.libs"
ls "$lib"/libmanatee.so* "$lib"/libmanatee*.dylib >/dev/null 2>&1 || { echo "no shared libmanatee in $lib either" >&2; exit 1; }
$CXX $flags $inc "$here/fqs_manatee.cc" -L"$lib" -lmanatee -Wl,-rpath,"$lib" $sys -o "$out"
echo "built $out (against $lib/libmanatee, rpath recorded)"
