#!/bin/sh
# A small self-authored Manatee corpus with text types (doc: id, genre, year, title), for FQS's
# stored subcorpora on native Manatee (src/subcorpus/, check-fqs.py).
#
#   MANATEE_SRC=/path/to/manatee-open sh build.sh [OUT]      (OUT default: ./out)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(mkdir -p "${1:-$here/out}" && cd "${1:-$here/out}" && pwd)
: "${MANATEE_SRC:?set MANATEE_SRC to a built manatee-open tree}"
mkdir -p "$out/registry" "$out/data"
cat > "$out/registry/subctest" <<REG
NAME "subcorpus test (self-authored)"
PATH "$out/data/subctest"
ENCODING "UTF-8"
FULLREF "doc.id"
ATTRIBUTE word
STRUCTURE doc {
	ATTRIBUTE id
	ATTRIBUTE genre
	ATTRIBUTE year
	ATTRIBUTE title
}
STRUCTURE s
REG
rm -rf "$out/data/subctest"
MANATEE_REGISTRY="$out/registry" "$MANATEE_SRC/src/encodevert" -c "$out/registry/subctest" "$here/subc.vert"
echo "built $out/registry/subctest"
