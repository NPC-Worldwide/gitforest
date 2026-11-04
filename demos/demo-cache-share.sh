#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
API_A=17881
P2P_A=19421
API_B=17882
P2P_B=19423
TMP="$(mktemp -d /tmp/gf-cache-demo-XXXXXX)"
REAL_HOME="$HOME"
export HOME="$TMP/home" XDG_CONFIG_HOME="$TMP/home/.config" XDG_DATA_HOME="$TMP/home/.local/share" XDG_RUNTIME_DIR="$TMP" GITFOREST_SKIP_AUTH=1
mkdir -p "$HOME"

cleanup() {
  pkill -f "config $TMP/a.toml" 2>/dev/null
  pkill -f "config $TMP/b.toml" 2>/dev/null
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "DEMO FAIL: $1"
  echo "--- daemon A log tail ---"
  tail -n 20 "$TMP/a.log" 2>/dev/null
  echo "--- daemon B log tail ---"
  tail -n 20 "$TMP/b.log" 2>/dev/null
  exit 1
}

echo "== gitforest shared cache-key directory demo =="
if [ ! -x "$BIN" ]; then
  echo "building release binary"
  (cd "$ROOT" && env HOME="$REAL_HOME" cargo build --release -p gt-daemon) || fail "build failed"
fi

GITFOREST_IDENTITY="$TMP/a.key" "$BIN" init --name demo-a >/dev/null 2>&1 || fail "init A failed"
GITFOREST_IDENTITY="$TMP/b.key" "$BIN" init --name demo-b >/dev/null 2>&1 || fail "init B failed"

cat > "$TMP/a.toml" <<EOF
data_dir = "$TMP/data-a"
p2p_listen = "127.0.0.1"
p2p_port = $P2P_A
api_port = $API_A
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = ["/ip4/127.0.0.1/tcp/$P2P_B/api/$API_B"]
enable_mdns = false
EOF
cat > "$TMP/b.toml" <<EOF
data_dir = "$TMP/data-b"
p2p_listen = "127.0.0.1"
p2p_port = $P2P_B
api_port = $API_B
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = ["/ip4/127.0.0.1/tcp/$P2P_A/api/$API_A"]
enable_mdns = false
EOF

wait_for_api() {
  for _ in $(seq 1 30); do
    CODE="$(curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$1/api/status" || true)"
    if [ "$CODE" = "200" ]; then return 0; fi
    sleep 1
  done
  return 1
}

setsid -f "$BIN" --identity "$TMP/a.key" --config "$TMP/a.toml" daemon </dev/null >>"$TMP/a.log" 2>&1
wait_for_api $API_A || fail "daemon A did not start"
echo "daemon A (:$API_A) is up"

mkdir -p "$TMP/work-a/build"
echo "shared-cache-payload-$(date +%s)" > "$TMP/work-a/build/out.bin"
"$BIN" --config "$TMP/a.toml" cache save --key linux-cargo-abc123 --path "$TMP/work-a/build" || fail "save on A failed"
sleep 2
mkdir -p "$TMP/prefix-a/libs"
echo "prefix-payload" > "$TMP/prefix-a/libs/lib.bin"
"$BIN" --config "$TMP/a.toml" cache save --key linux-cargo-xyz999 --path "$TMP/prefix-a/libs" || fail "prefix save on A failed"

setsid -f "$BIN" --identity "$TMP/b.key" --config "$TMP/b.toml" daemon </dev/null >>"$TMP/b.log" 2>&1
wait_for_api $API_B || fail "daemon B did not start"
echo "daemon B (:$API_B) is up; waiting for gossip from A"
sleep 3

RESTORED=""
for _ in $(seq 1 12); do
  RESTORED="$("$BIN" --config "$TMP/b.toml" cache restore --key linux-cargo-abc123 --path "$TMP/work-b/build" 2>"$TMP/restore.err" || true)"
  if [ -f "$TMP/work-b/build/out.bin" ]; then break; fi
  sleep 2
done
[ -f "$TMP/work-b/build/out.bin" ] || fail "B never restored the key saved only on A"
cmp -s "$TMP/work-a/build/out.bin" "$TMP/work-b/build/out.bin" || fail "restored bytes differ"
echo "$RESTORED" | grep -q "restored-from-peer=true" || fail "restore did not report restored-from-peer (got: $RESTORED)"
echo "exact restore on B ok: $RESTORED"

RESTORED=""
for _ in $(seq 1 12); do
  RESTORED="$("$BIN" --config "$TMP/b.toml" cache restore --key linux-cargo-nope --restore-keys linux-cargo- --path "$TMP/prefix-b/libs" 2>>"$TMP/restore.err" || true)"
  if [ -f "$TMP/prefix-b/libs/lib.bin" ]; then break; fi
  sleep 2
done
[ -f "$TMP/prefix-b/libs/lib.bin" ] || fail "B never restored via prefix fallback"
cmp -s "$TMP/prefix-a/libs/lib.bin" "$TMP/prefix-b/libs/lib.bin" || fail "prefix-restored bytes differ"
echo "$RESTORED" | grep -q "cache-hit=false" || fail "prefix restore should report cache-hit=false (got: $RESTORED)"
echo "$RESTORED" | grep -q "restored-from-peer=true" || fail "prefix restore did not report restored-from-peer (got: $RESTORED)"
echo "prefix restore on B ok: $RESTORED"

"$BIN" --config "$TMP/a.toml" cache delete --key linux-cargo-xyz999 | grep -q "deleted linux-cargo-xyz999" || fail "delete on A failed"
curl -s --noproxy '*' "http://127.0.0.1:$API_A/api/caches/list" | grep -q xyz999 && fail "A still lists deleted key"
curl -s --noproxy '*' "http://127.0.0.1:$API_B/api/caches/list" | grep -q xyz999 || fail "B lost a key it holds locally"
echo "delete on A ok: key gone on A, kept on B"

echo "DEMO PASS"
