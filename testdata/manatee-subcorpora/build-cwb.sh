#!/bin/sh
# The same self-authored corpus (subc.vert) as a CWB corpus SUBCCWB, for FQS's subcorpora on CWB
# (check-fqs.py URL subccwb). Needs cwb-encode and cwb-makeall on PATH.
#
#   sh build-cwb.sh [OUT]      (OUT default: ./out) → OUT/cwb/registry/subccwb
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(mkdir -p "${1:-$here/out}" && cd "${1:-$here/out}" && pwd)/cwb
rm -rf "$out/data"
mkdir -p "$out/registry" "$out/data"
cwb-encode -d "$out/data" -f "$here/subc.vert" -R "$out/registry/subccwb" -c utf8 -S doc:0+id+genre+year+title -S s
cwb-makeall -r "$out/registry" SUBCCWB >/dev/null
echo "built $out/registry/subccwb"
