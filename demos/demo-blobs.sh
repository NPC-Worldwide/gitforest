#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
TMP="$(mktemp -d /tmp/gf-blobs-XXXXXX)"

SEED_API=17891; SEED_P2P=19431
LEECH_API=17892; LEECH_P2P=19433

cleanup() {
  pkill -f "gitforest --config $TMP/" 2>/dev/null
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "BLOBS FAIL: $1"
  for n in seed leech; do
    echo "--- $n log tail ---"
    tail -n 12 "$TMP/$n.log" 2>/dev/null
  done
  exit 1
}

echo "== gitforest live P2P blob sharing proof =="
(cd "$ROOT" && cargo build --release -p gt-daemon) || fail "build failed"

SEED_ADDR="/ip4/127.0.0.1/tcp/$SEED_P2P/api/$SEED_API"
LEECH_ADDR="/ip4/127.0.0.1/tcp/$LEECH_P2P/api/$LEECH_API"

mkconfig() {
  local name="$1" api="$2" p2p="$3" boots="$4"
  mkdir -p "$TMP/home-$name" "$TMP/$name"
  HOME="$TMP/home-$name" "$BIN" init --name "$name" >/dev/null 2>&1 || fail "init $name failed"
  cat > "$TMP/$name.toml" <<EOF
data_dir = "$TMP/$name/data"
p2p_port = $p2p
api_port = $api
advertise_addr = "/ip4/127.0.0.1/tcp/$p2p/api/$api"
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = [$boots]
EOF
}

mkconfig seed "$SEED_API" "$SEED_P2P" "\"$LEECH_ADDR\""
mkconfig leech "$LEECH_API" "$LEECH_P2P" "\"$SEED_ADDR\""

for n in seed leech; do
  env HOME="$TMP/home-$n" setsid -f "$BIN" --config "$TMP/$n.toml" daemon </dev/null >>"$TMP/$n.log" 2>&1
done

for spec in "seed:$SEED_API" "leech:$LEECH_API"; do
  n="${spec%%:*}"; port="${spec##*:}"
  READY=""
  for _ in $(seq 1 40); do
    CODE="$(curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$port/api/status" || true)"
    if [ "$CODE" = "200" ]; then READY="yes"; break; fi
    sleep 1
  done
  [ "$READY" = "yes" ] || fail "daemon $n did not start"
done
echo "two daemons up with real local auth"

SEED_TOKEN="$(cat "$TMP/home-seed/.local/share/gitforest/api.token")"
LEECH_TOKEN="$(cat "$TMP/home-leech/.local/share/gitforest/api.token")"
[ -n "$SEED_TOKEN" ] && [ -n "$LEECH_TOKEN" ] || fail "missing api tokens"

head -c 5242880 /dev/urandom > "$TMP/weights.bin"
SHA_LOCAL="$(sha256sum "$TMP/weights.bin" | cut -d' ' -f1)"
echo "test file: 5MB sha256 $SHA_LOCAL"

export GF_SEED="http://127.0.0.1:$SEED_API" GF_LEECH="http://127.0.0.1:$LEECH_API"
export GF_SEED_TOKEN="$SEED_TOKEN" GF_LEECH_TOKEN="$LEECH_TOKEN" GF_FILE="$TMP/weights.bin"
BLOB_ID="$(python3 - <<'PYEOF'
import json, urllib.request, os
def post(api, token, path, body):
    req = urllib.request.Request(api + path, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"},
        method="POST")
    return json.load(urllib.request.urlopen(req, timeout=30))
r = post(os.environ["GF_SEED"], os.environ["GF_SEED_TOKEN"], "/api/blobs", {"path": os.environ["GF_FILE"]})
print(r["blob_id"])
PYEOF
)" || fail "upload failed"
[ -n "$BLOB_ID" ] || fail "empty blob id"
echo "uploaded blob: $BLOB_ID"

export GF_BLOB="$BLOB_ID"
python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_LEECH']+'/api/blobs/'+os.environ['GF_BLOB']+'/fetch',data=b'{}',headers={'Content-Type':'application/json','Authorization':'Bearer '+os.environ['GF_LEECH_TOKEN']},method='POST'); print(json.load(urllib.request.urlopen(q,timeout=30)))" || fail "fetch kickoff failed"

echo "waiting for P2P transfer (20 pieces)..."
GOT=""
for _ in $(seq 1 90); do
  if curl -s --noproxy '*' -o "$TMP/got.bin" -w "%{http_code}" "http://127.0.0.1:$LEECH_API/api/blobs/$BLOB_ID/download" | grep -q 200; then
    if [ -s "$TMP/got.bin" ]; then GOT="yes"; break; fi
  fi
  sleep 2
done
[ "$GOT" = "yes" ] || fail "leecher never completed the blob"
SHA_GOT="$(sha256sum "$TMP/got.bin" | cut -d' ' -f1)"
[ "$SHA_GOT" = "$SHA_LOCAL" ] || fail "bytes differ: $SHA_GOT != $SHA_LOCAL"
echo "bytes identical after P2P transfer: $SHA_GOT"

SEEDERS="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_LEECH']+'/api/blobs/'+os.environ['GF_BLOB'],headers={'Authorization':'Bearer '+os.environ['GF_LEECH_TOKEN']}); print(json.load(urllib.request.urlopen(q,timeout=10))['seeders'])")"
echo "leecher sees seeders: $SEEDERS"
[ "$SEEDERS" -ge 1 ] 2>/dev/null || fail "no seeders tracked"

COLL_ID="$(python3 - <<'PYEOF'
import json, urllib.request, os
def api(base, token, path, body=None, method="GET"):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data,
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"},
        method=method)
    return json.load(urllib.request.urlopen(req, timeout=30))
c = api(os.environ["GF_SEED"], os.environ["GF_SEED_TOKEN"], "/api/collections",
        {"name": "tiny-llama", "description": "test model card",
         "files": [{"path": "weights.bin", "blob_id": os.environ["GF_BLOB"], "size": 5242880}]}, "POST")
print(c["id"])
PYEOF
)" || fail "collection create failed"
[ -n "$COLL_ID" ] || fail "empty collection id"
python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_LEECH']+'/api/collections/$COLL_ID/fetch',data=b'{}',headers={'Content-Type':'application/json','Authorization':'Bearer '+os.environ['GF_LEECH_TOKEN']},method='POST'); urllib.request.urlopen(q,timeout=30)" || fail "collection fetch failed"
MANIFEST=""
for _ in $(seq 1 45); do
  if python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_LEECH']+'/api/collections/$COLL_ID',headers={'Authorization':'Bearer '+os.environ['GF_LEECH_TOKEN']}); d=json.load(urllib.request.urlopen(q,timeout=10)); raise SystemExit(0 if d['name']=='tiny-llama' else 1)" 2>/dev/null; then MANIFEST="yes"; break; fi
  sleep 2
done
[ "$MANIFEST" = "yes" ] || fail "collection manifest never arrived"
echo "collection manifest replicated: tiny-llama"

echo "BLOBS PASS"
