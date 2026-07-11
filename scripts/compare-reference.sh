#!/bin/sh
set -eu

usage() {
  cat >&2 <<'EOF'
Usage: compare-reference.sh CONFIG STATUS_JSON [NATIVE_BINARY] [WIDTH ...]

Compare raw output from ccstatusline-native with ccstatusline 2.2.23.
The reference is selected in this order: ccstatusline, bunx, npx.
EOF
}

if [ "$#" -lt 2 ]; then
  usage
  exit 2
fi

config=$1
status_json=$2
native=${3:-target/release/ccstatusline-native}
if [ "$#" -ge 3 ]; then
  shift 3
else
  shift 2
fi
widths=${*:-60 70 80 100 120 160}

if [ ! -r "$config" ] || [ ! -r "$status_json" ]; then
  echo "Config and status JSON must both be readable." >&2
  exit 2
fi
if [ ! -x "$native" ]; then
  echo "Native binary is not executable: $native" >&2
  exit 2
fi

temporary=$(mktemp -d "${TMPDIR:-/tmp}/ccstatusline-native-compare.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

run_reference() {
  width=$1
  if command -v ccstatusline >/dev/null 2>&1 \
    && ccstatusline --version 2>/dev/null | grep -q '2\.2\.23'; then
    CCSTATUSLINE_WIDTH=$width ccstatusline --config "$config"
  elif command -v bunx >/dev/null 2>&1; then
    CCSTATUSLINE_WIDTH=$width bunx -y ccstatusline@2.2.23 --config "$config"
  elif command -v npx >/dev/null 2>&1; then
    CCSTATUSLINE_WIDTH=$width npx --yes ccstatusline@2.2.23 --config "$config"
  else
    echo "Install ccstatusline 2.2.23, bunx, or npx to run the oracle." >&2
    return 127
  fi
}

for width in $widths; do
  run_reference "$width" < "$status_json" > "$temporary/reference-$width.bin"
  CCSTATUSLINE_WIDTH=$width "$native" --config "$config" \
    < "$status_json" > "$temporary/native-$width.bin"

  if cmp -s "$temporary/reference-$width.bin" "$temporary/native-$width.bin"; then
    hash=$(shasum -a 256 "$temporary/native-$width.bin" | awk '{ print $1 }')
    echo "$width MATCH $hash"
  else
    reference_hash=$(shasum -a 256 "$temporary/reference-$width.bin" | awk '{ print $1 }')
    native_hash=$(shasum -a 256 "$temporary/native-$width.bin" | awk '{ print $1 }')
    echo "$width MISMATCH reference=$reference_hash native=$native_hash" >&2
    exit 1
  fi
done
