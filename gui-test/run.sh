#!/bin/bash
set -u
set -m
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${GUI_OUT:-$ROOT/gui-test/out}"
CDP_PORT="${GUI_CDP_PORT:-9222}"
APP_PORT="${GUI_APP_PORT:-7878}"
P2P_PORT="${GUI_P2P_PORT:-9418}"
TMP="$(mktemp -d /tmp/gf-gui-XXXX)"
export no_proxy='*' NO_PROXY='*'

fail() { echo "GUI SMOKE FAIL: $1"; exit 1; }
cleanup() {
  if [ -n "${APP_PID:-}" ]; then kill -- "-$APP_PID" 2>/dev/null; fi
  for pid in $(pgrep -f -- "user-data-dir=$TMP/udata" 2>/dev/null | grep -v "^$$\$" || true); do
    kill "$pid" 2>/dev/null || true
  done
  for pid in $(pgrep -f -- "gitforest --config $TMP/" 2>/dev/null | grep -v "^$$\$" || true); do
    kill "$pid" 2>/dev/null || true
  done
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

[ -f "$ROOT/desktop/dist-electron/main.js" ] || fail "desktop not built (run npm build steps in desktop/ first)"
[ -x "$ROOT/target/release/gitforest" ] || fail "missing $ROOT/target/release/gitforest (cargo build --release first)"
if [ -z "${DISPLAY:-}" ]; then
  command -v Xvfb >/dev/null 2>&1 || fail "no DISPLAY and no Xvfb installed"
  /usr/bin/Xvfb :42 -screen 0 1400x900x24 >/tmp/gf-gui-xvfb.log 2>&1 &
  XVFB_PID=$!
  export DISPLAY=:42
  sleep 3
else
  XVFB_PID=""
fi
if curl -s --noproxy '*' -o /dev/null --max-time 3 "http://127.0.0.1:$APP_PORT/api/status"; then
  fail "port $APP_PORT already in use (stop your daemon first)"
fi

export HOME="$TMP/home"
mkdir -p "$HOME" "$OUT"
HOME="$HOME" "$ROOT/target/release/gitforest" init --name guitest >"$OUT/init.log" 2>&1 || fail "gitforest init failed ($(cat "$OUT/init.log"))"
mkdir -p "$HOME/.local/share/gitforest" "$HOME/.config/gitforest"
touch "$HOME/.local/share/gitforest/.onboarded"
cat > "$HOME/.config/gitforest/config.toml" <<EOF
data_dir = "$HOME/.local/share/gitforest"
p2p_port = $P2P_PORT
p2p_listen = "127.0.0.1"
api_port = $APP_PORT
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = []
enable_mdns = false
EOF

FIX="$TMP/fixture-repo"
git init -q --initial-branch=main "$FIX"
cat > "$FIX/.gitforest-ci.yml" <<'EOF'
name: ci
docker_image: alpine:3
timeout_secs: 60
steps:
  - name: t
    run: echo hi
EOF
git -C "$FIX" add . && git -C "$FIX" -c user.email=t@t -c user.name=t commit -qm init
export GUI_COMMIT_SHA="$(git -C "$FIX" rev-parse HEAD)"

cd "$ROOT/desktop"
DISPLAY="$DISPLAY" HOME="$HOME" XDG_RUNTIME_DIR="$TMP" GF_API_PORT="$APP_PORT" GF_P2P_PORT="$P2P_PORT" GF_GITFOREST_BIN="$ROOT/target/release/gitforest" \
  ./node_modules/electron/dist/electron . --no-sandbox \
  --remote-debugging-port="$CDP_PORT" --user-data-dir="$TMP/udata" \
  </dev/null >>"$OUT/app.log" 2>&1 &
APP_PID=$!

READY=""
for _ in $(seq 1 90); do
  if curl -s --noproxy '*' -o /dev/null --max-time 3 "http://127.0.0.1:$APP_PORT/api/status"; then READY="yes"; break; fi
  sleep 2
done
[ "$READY" = "yes" ] || fail "app-spawned daemon never came up (see $OUT/app.log)"
TOKEN="$(cat "$HOME/.local/share/gitforest/api.token")"
[ -n "$TOKEN" ] || fail "no daemon api token"
curl -s --noproxy '*' --max-time 10 -X POST "http://127.0.0.1:$APP_PORT/api/repos" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d "{\"path\": \"$FIX\", \"name\": \"fixture\"}" | grep -q fixture || fail "fixture repo seed failed"

PAGE=""
for _ in $(seq 1 60); do
  if curl -s --noproxy '*' --max-time 3 "http://127.0.0.1:$CDP_PORT/json/list" | grep -q '"type": "page"'; then PAGE="yes"; break; fi
  sleep 2
done
[ "$PAGE" = "yes" ] || fail "no electron page target on CDP $CDP_PORT"

export GUI_CDP="http://127.0.0.1:$CDP_PORT" GUI_OUT="$OUT"
node "$ROOT/gui-test/smoke.js"
RC=$?
if [ -n "$XVFB_PID" ]; then kill "$XVFB_PID" 2>/dev/null; fi
exit $RC
