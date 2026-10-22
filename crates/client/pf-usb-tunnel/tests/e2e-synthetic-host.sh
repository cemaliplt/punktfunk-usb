#!/bin/bash
# Real handshake through the tunnel, by hand (not part of `cargo test`):
#
#   punktfunk-probe -> pf-usb-relay -> TCP -> punktfunk-usb-bridge -> punktfunk-host punktfunk1-host
#                      (127.0.0.1:29777)      (127.0.0.1:29779)       (synthetic-abr, :19777)
#
#   tests/e2e-synthetic-host.sh [tunnel|direct|restart]
#     tunnel   probe talks to the relay (default)
#     direct   probe talks straight to the host (baseline for the latency comparison)
#     restart  like tunnel, but the bridge is kill -9'd 6 s into a 16 s session and restarted
#              1 s later: the QUIC session must survive (relay keeps its flows for `grace`)
#
# Needs: cargo build -p pf-usb-tunnel -p punktfunk-probe, and a punktfunk-host binary
# (HOST_BIN, default the 0.44 trial copy). Everything runs with a scratch HOME and
# PUNKTFUNK_NV_PROFILE=0 (a plain `punktfunk-host --help` otherwise installs an NVIDIA driver
# profile into ~/.nv). Output in $OUT (default a mktemp dir).
set -u
MODE=${1:-tunnel}
ROOT=$(cd "$(dirname "$0")/../../../.." && pwd)
T=${TARGET_DIR:-$ROOT/target/debug}
HOST_BIN=${HOST_BIN:-$HOME/Work/punktfunk-ipad-trial-20261009/bin/punktfunk-host}
OUT=${OUT:-$(mktemp -d)}
export HOME=$OUT/home XDG_CONFIG_HOME=$OUT/home/.config XDG_DATA_HOME=$OUT/home/.local/share
export XDG_STATE_HOME=$OUT/home/.state XDG_CACHE_HOME=$OUT/home/.cache PUNKTFUNK_NV_PROFILE=0 PUNKTFUNK_MDNS=0
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"
SECS=8; [ "$MODE" = restart ] && SECS=16

"$HOST_BIN" punktfunk1-host --source synthetic-abr --port 19777 --allow-tofu --no-mdns --seconds 30 >"$OUT/host.log" 2>&1 &
HP=$!
sleep 2
TARGET=127.0.0.1:19777
if [ "$MODE" != direct ]; then
  "$T/pf-usb-relay" --tcp 127.0.0.1:29779 --udp 127.0.0.1:29777 >"$OUT/relay.log" 2>&1 & RP=$!
  "$T/punktfunk-usb-bridge" --tcp 127.0.0.1:29779 --host 127.0.0.1:19777 >"$OUT/bridge.log" 2>&1 & BP=$!
  sleep 1
  TARGET=127.0.0.1:29777
fi
"$T/punktfunk-probe" --connect $TARGET --seconds $SECS --mode 1280x720x60 --link-kbps 150000 >"$OUT/probe.log" 2>&1 &
PP=$!
if [ "$MODE" = restart ]; then
  sleep 6; kill -9 $BP; sleep 1
  "$T/punktfunk-usb-bridge" --tcp 127.0.0.1:29779 --host 127.0.0.1:19777 >"$OUT/bridge2.log" 2>&1 & BP=$!
fi
wait $PP; echo "probe exit=$?"
sed 's/\x1b\[[0-9;]*m//g' "$OUT/probe.log" | grep -E "stream complete|host/network"
kill ${RP:-} ${BP:-} $HP 2>/dev/null
echo "logs in $OUT"
