#!/bin/sh
# A small self-authored Manatee corpus with MULTIVALUE attributes (lemma and feats, MULTISEP "|"),
# for the corpus check (fqsm_check): Manatee indexes each part of such a value on its own, so the
# index has "Gender=Neut" at a token whose text is "Case=Acc|Gender=Neut|…". Before fqs-manatee
# 0.6.1 the check refused every such corpus (UD 2.2/2.3, PARSEME, Summa on LINDAT).
#
#   MANATEE_SRC=/path/to/manatee-open sh build.sh [OUT]      (OUT default: ./out)
#   fqs manatee-export --registry OUT/registry/mvtest --out /dev/null   (runs the check first)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(mkdir -p "${1:-$here/out}" && cd "${1:-$here/out}" && pwd)
: "${MANATEE_SRC:?set MANATEE_SRC to a built manatee-open tree}"
mkdir -p "$out/registry" "$out/data"
cat > "$out/registry/mvtest" <<REG
NAME "multivalue test (self-authored)"
PATH "$out/data/mvtest"
ENCODING "UTF-8"
ATTRIBUTE word
ATTRIBUTE lemma {
	MULTIVALUE y
	MULTISEP "|"
}
ATTRIBUTE feats {
	MULTIVALUE yes
	MULTISEP "|"
}
STRUCTURE doc {
	ATTRIBUTE id
}
STRUCTURE s
REG
rm -rf "$out/data/mvtest"
MANATEE_REGISTRY="$out/registry" "$MANATEE_SRC/src/encodevert" -c "$out/registry/mvtest" "$here/mv.vert"
echo "built $out/registry/mvtest"
