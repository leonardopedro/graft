#!/usr/bin/env bash
# Cold-edge wire measurement for the CDC store against an S3-compatible remote.
#
# Runs an origin that pushes a 4 MiB random file, then a second, empty data
# directory that attaches and reads a 10 KiB range, counting the bytes that
# cross the wire in each step. Requires node/npm and a built `cdc` binary.
#
#   ./measure.sh [size_bytes]
#
# Knobs: DELAY_MS=<n> adds one-way proxy latency (modelling an RTT),
#        BLOB=<path> replays one exact input, CDC=<path> picks the binary.
#
# Everything it creates lives under graft/target/s3-wire-measure/.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
work="$repo/target/s3-wire-measure"
size=${1:-4194304}
cdc=${CDC:-$repo/target/debug/cdc}
bucket=graft-bucket
s3_port=9000
proxy_port=9001
pids=()

cleanup() {
  for pid in "${pids[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  pkill -f "node s3.js" 2>/dev/null || true
}
trap cleanup EXIT

start_s3() {
  npm install --prefix "$work" --silent s3rver
  NODE_PATH="$work/node_modules" S3_DIR="$work/s3-data" S3_BUCKET=$bucket \
    S3_PORT=$s3_port node "$here/s3.js" >"$work/s3.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 50); do
    curl -sf -o /dev/null "http://127.0.0.1:$s3_port/$bucket" && return 0
    sleep 0.2
  done
  echo "s3rver did not come up; see $work/s3.log" >&2
  return 1
}

# Start a fresh proxy, so the log contains only the step that follows.
# Run a command and report how long it took, in milliseconds.
timed() {
  local label=$1
  shift
  local start end
  start=$(date +%s%N)
  "$@"
  end=$(date +%s%N)
  echo "[time] $label: $(((end - start) / 1000000)) ms"
}

start_proxy() {
  node "$here/proxy.js" "$proxy_port" "$s3_port" "$work/$1.json" \
    "${DELAY_MS:-0}" >"$work/$1.proxy.log" 2>&1 &
  pids+=($!)
  sleep 0.5
}

stop_proxy() {
  pkill -f "proxy.js $proxy_port $s3_port" 2>/dev/null || true
  sleep 0.3
}

summary() {
  node -e '
    const fs = require("fs");
    if (!fs.existsSync(process.argv[1])) {
      console.log("0 requests, 0 B down, 0 B up");
      process.exit(0);
    }
    const l = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    const down = l.reduce((a, r) => a + r.responseBytes, 0);
    const up = l.reduce((a, r) => a + r.requestBytes, 0);
    const probes = l.filter((r) => r.status === 404).reduce((a, r) => a + r.responseBytes, 0);
    console.log(
      `${l.length} requests, ${down} B down, ${up} B up` +
        (probes ? ` (${probes} B of it 404 probes)` : ""),
    );
    for (const r of l) {
      const name = r.path.split("/").pop();
      console.log(`  ${r.method} ${r.status} ${name} ${r.range || ""} -> ${r.responseBytes} B`);
    }
  ' "$work/$1.json"
}

[ -x "$cdc" ] || (cd "$repo" && cargo build -p cdc-cli)
rm -rf "$work"
mkdir -p "$work"
start_s3

export AWS_ACCESS_KEY_ID=S3RVER AWS_SECRET_ACCESS_KEY=S3RVER AWS_REGION=us-east-1
export AWS_ENDPOINT_URL="http://127.0.0.1:$proxy_port"

offset=$((size / 2))
# BLOB=path replays one exact input, so a layout that misbehaves can be re-run.
if [[ -n ${BLOB:-} ]]; then
  cp "$BLOB" "$work/blob.bin"
else
  head -c "$size" /dev/urandom >"$work/blob.bin"
fi

start_proxy origin
"$cdc" --data "$work/origin" --remote "s3:$bucket" init >/dev/null
"$cdc" --data "$work/origin" put blob.bin "$work/blob.bin"
"$cdc" --data "$work/origin" sync
stop_proxy
echo "== origin push"; summary origin

vid=$("$cdc" --data "$work/origin" share | awk '/^vid:/{print $2}')
log=$("$cdc" --data "$work/origin" share | awk '/^log:/{print $2}')

mkdir -p "$work/edge"
cp "$work/origin/volume.key" "$work/edge/volume.key"

start_proxy attach
timed attach "$cdc" --data "$work/edge" attach --vid "$vid" --log "$log" \
  --remote "s3:$bucket" --key "$work/edge/volume.key"
stop_proxy
echo "== cold attach"; summary attach

start_proxy get
timed "cold range" "$cdc" --data "$work/edge" get blob.bin --offset "$offset" \
  --length 10240 -o "$work/range.bin"
stop_proxy
echo "== cold 10 KiB range read"; summary get

start_proxy warm
timed "warm range" "$cdc" --data "$work/edge" get blob.bin --offset "$offset" \
  --length 10240 -o "$work/range2.bin"
stop_proxy
echo "== warm 10 KiB range read"; summary warm
cmp "$work/range.bin" "$work/range2.bin"

echo
echo "logs and data: $work"
