#!/bin/sh
# Build libfqs_manatee (native Manatee for FQS; dev/MANATEE-NATIVE.md) against a built manatee-open tree:
#
#   MANATEE_SRC=/path/to/manatee-open-2.225.8 sh manatee/build.sh      → manatee/libfqs_manatee.{dylib,so}
#
# First statically: libmanatee.a and libbuiltinmanatee.a (the built-in dynamic attribute functions,
# e.g. utf8lowercase) go into the library, which then needs no Manatee at runtime. Where the static
# archives are not position-independent (Linux libtool builds), that fails, and it links Manatee's
# shared library instead, with its folder recorded (rpath): keep that manatee-open tree in place, or
# install Manatee. Point FQS at the result with FQS_MANATEE_LIB. Build against the manatee-open
# KonText uses.
set -eu
: "${MANATEE_SRC:?set MANATEE_SRC to a built manatee-open tree (configure && make)}"
here=$(cd "$(dirname "$0")" && pwd)
M=$(cd "$MANATEE_SRC" && pwd)
case "$(uname -s)" in
	Darwin) out="$here/libfqs_manatee.dylib"; sys="-liconv -ldl" ;;
	*) out="$here/libfqs_manatee.so"; sys="-ldl -lpthread" ;;
esac
[ -f "$M/config.hh" ] || { echo "missing $M/config.hh: build manatee-open first (configure && make)" >&2; exit 1; }
inc="-I$M -I$M/finlib -I$M/corp -I$M/concord -I$M/query -I$M/hat-trie/src"
CXX=${CXX:-c++}
flags="-std=gnu++11 -O2 -fPIC -shared -DHAVE_CONFIG_H"

if [ -f "$M/src/.libs/libmanatee.a" ] && [ -f "$M/src/.libs/libbuiltinmanatee.a" ] &&
	$CXX $flags $inc "$here/fqs_manatee.cc" "$M/src/.libs/libmanatee.a" "$M/src/.libs/libbuiltinmanatee.a" $sys -o "$out" 2>"$here/.build-static.log"; then
	rm -f "$here/.build-static.log"
	echo "built $out (Manatee linked in statically)"
	exit 0
fi
echo "static link not possible here (see manatee/.build-static.log); linking Manatee's shared library" >&2
lib="$M/src/.libs"
ls "$lib"/libmanatee.so* "$lib"/libmanatee*.dylib >/dev/null 2>&1 || { echo "no shared libmanatee in $lib either" >&2; exit 1; }
builtin=""
[ -f "$lib/libbuiltinmanatee.a" ] && builtin="-Wl,--whole-archive $lib/libbuiltinmanatee.a -Wl,--no-whole-archive"
case "$(uname -s)" in Darwin) builtin="" ;; esac
$CXX $flags $inc "$here/fqs_manatee.cc" -L"$lib" -lmanatee $builtin -Wl,-rpath,"$lib" $sys -o "$out"
echo "built $out (against $lib/libmanatee, rpath recorded)"
