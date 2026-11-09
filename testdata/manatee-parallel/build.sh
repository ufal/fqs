#!/bin/sh
# Build the small aligned Manatee test corpora (self-authored text; dev/MANATEE-NATIVE.md, "Parallel corpora"):
#
#   par_cs / par_en     5 sentences each, aligned 1:1 by number on the structure `s` (no ALIGNDEF)
#   parx_cs / parx_en   the same Czech, English re-segmented: cs s2 = en s2+s3, cs s4 has no English
#                       (ALIGNDEF, level files written by mkalign from parx_*.align)
#
#   sh testdata/manatee-parallel/build.sh [OUT]       (OUT default ~/flexitools/test/manatee)
#
# → OUT/registry/{par_cs,par_en,parx_cs,parx_en}, OUT/data/<name>/. Needs Manatee's encodevert and mkalign
# (on PATH, or MANATEE_BIN=folder: a built source tree's src/).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$HOME/flexitools/test/manatee}
bin=${MANATEE_BIN:-}
tool() { if [ -n "$bin" ]; then echo "$bin/$1"; else command -v "$1"; fi; }
encodevert=$(tool encodevert)
mkalign=$(tool mkalign)
mkdir -p "$out/registry" "$out/data"

registry() {   # name vertical partner [aligndef]
	cat >"$out/registry/$1" <<EOF
NAME "$1 (FQS aligned test corpus)"
PATH "$out/data/$1/"
VERTICAL "$here/$2"
ENCODING "UTF-8"
INFO "aligned test corpus for native Manatee in FQS (fqs/testdata/manatee-parallel)"
ATTRIBUTE word
ATTRIBUTE lemma
STRUCTURE doc {
	ATTRIBUTE id
}
STRUCTURE s {
	ATTRIBUTE id
}
ALIGNSTRUCT "s"
ALIGNED "$3"
FULLREF "doc.id"
EOF
	if [ -n "${4:-}" ]; then echo "ALIGNDEF \"$here/$4\"" >>"$out/registry/$1"; fi
	rm -rf "$out/data/$1"
	mkdir -p "$out/data/$1"
	"$encodevert" -c "$out/registry/$1" "$here/$2" >/dev/null 2>&1 || "$encodevert" -c "$out/registry/$1" "$here/$2"
}

registry par_cs par_cs.vert par_en
registry par_en par_en.vert par_cs
registry parx_cs par_cs.vert parx_en parx_cs-en.align
registry parx_en parx_en.vert parx_cs parx_en-cs.align
# the level files: PATH/align.<partner> (Manatee's Corpus::get_aligned_level)
"$mkalign" "$here/parx_cs-en.align" "$out/data/parx_cs/align.parx_en"
"$mkalign" "$here/parx_en-cs.align" "$out/data/parx_en/align.parx_cs"
echo "built par_cs, par_en, parx_cs, parx_en in $out (registry/, data/)"
