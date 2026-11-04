#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
TMP="$(mktemp -d /tmp/gf-llmreal-XXXXXX)"
CLUSTER="llm-real-proof"
CACHE="$HOME/.cache/gitforest/llm-real"
LLAMA="$CACHE/llama.cpp/build/bin/llama-cli"
WEIGHTS="$CACHE/qwen2.5-0.5b-instruct-q8_0.gguf"
WEIGHTS_URL="https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q8_0.gguf"

SEED_API=17900; SEED_P2P=19460
LEECH_API=17901; LEECH_P2P=19462
PROMPT="Reply with exactly this word and nothing else: pineapple"

cleanup() {
  for cfg in "$TMP/seed.toml" "$TMP/leech.toml"; do
    for pid in $(pgrep -f -- "gitforest --config $cfg" 2>/dev/null | grep -v "^$$\$" || true); do
      kill "$pid" 2>/dev/null || true
    done
  done
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "LLM-REAL FAIL: $1"
  for n in seed leech; do
    echo "--- $n log tail ---"
    tail -n 15 "$TMP/$n.log" 2>/dev/null
  done
  exit 1
}

need() {
  echo "$2" | grep -q -- "$3" || fail "$1: missing [$3] in $2"
}

echo "== gitforest live real-LLM proof =="
(cd "$ROOT" && cargo build --release --offline -p gt-daemon) || fail "build failed"

mkdir -p "$CACHE"
if [ ! -x "$LLAMA" ]; then
  echo "building llama.cpp backend (one-time)..."
  [ -d "$CACHE/llama.cpp" ] || git clone --depth 1 https://github.com/ggerganov/llama.cpp.git "$CACHE/llama.cpp" || fail "llama.cpp clone failed"
  cmake -S "$CACHE/llama.cpp" -B "$CACHE/llama.cpp/build" -DCMAKE_BUILD_TYPE=Release -DGGML_NATIVE=ON -DGGML_CUDA=OFF -DLLAMA_CURL=OFF >/dev/null 2>&1 || fail "llama.cpp cmake failed"
  cmake --build "$CACHE/llama.cpp/build" --config Release -j"$(nproc)" --target llama-cli >/dev/null 2>&1 || fail "llama.cpp build failed"
fi
[ -x "$LLAMA" ] || fail "llama-cli missing"
if [ ! -f "$WEIGHTS" ]; then
  echo "downloading real weights (one-time)..."
  curl -fSL -o "$WEIGHTS" "$WEIGHTS_URL" || fail "weights download failed"
fi
[ "$(head -c 4 "$WEIGHTS")" = "GGUF" ] || fail "weights are not GGUF"
WSIZE="$(stat -c %s "$WEIGHTS")"
SHA="$(sha256sum "$WEIGHTS" | cut -d' ' -f1)"
echo "backend: $LLAMA"
echo "weights: $WEIGHTS ($WSIZE bytes, sha256 $SHA)"

mkconfig() {
  local name="$1" api="$2" p2p="$3" boots="$4"
  mkdir -p "$TMP/home-$name" "$TMP/$name"
  HOME="$TMP/home-$name" "$BIN" init --name "$name" >/dev/null 2>&1 || fail "init $name failed"
  cat > "$TMP/$name.toml" <<EOF
data_dir = "$TMP/$name/data"
p2p_port = $p2p
api_port = $api
cluster_token = "$CLUSTER"
advertise_addr = "/ip4/127.0.0.1/tcp/$p2p/api/$api"
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = [$boots]
enable_mdns = false
llm_backend = "$LLAMA"
EOF
}

mkconfig seed "$SEED_API" "$SEED_P2P" ""
mkconfig leech "$LEECH_API" "$LEECH_P2P" "\"/ip4/127.0.0.1/tcp/$SEED_P2P/api/$SEED_API\""

for n in seed leech; do
  env HOME="$TMP/home-$n" setsid -f "$BIN" --config "$TMP/$n.toml" daemon </dev/null >>"$TMP/$n.log" 2>&1
done

for spec in "seed:$SEED_API" "leech:$LEECH_API"; do
  n="${spec%%:*}"; port="${spec##*:}"; ready=no
  for _ in $(seq 1 30); do
    if curl -s --noproxy '*' "http://127.0.0.1:$port/api/status" | grep -q dht_alive; then ready=yes; break; fi
    sleep 1
  done
  [ "$ready" = "yes" ] || fail "daemon $n did not start"
done
echo "two daemons up (seeder + leecher)"

SEED_TOKEN="$(cat "$TMP/home-seed/.local/share/gitforest/api.token")"
LEECH_TOKEN="$(cat "$TMP/home-leech/.local/share/gitforest/api.token")"
[ -n "$SEED_TOKEN" ] && [ -n "$LEECH_TOKEN" ] || fail "missing api tokens"
S="http://127.0.0.1:$SEED_API"
L="http://127.0.0.1:$LEECH_API"

BLOB="$(curl -s --noproxy '*' -H "Authorization: Bearer $SEED_TOKEN" -H 'Content-Type: application/json' -d "{\"path\":\"$WEIGHTS\"}" "$S/api/blobs" | python3 -c 'import json,sys; print(json.load(sys.stdin)["blob_id"])')"
[ "$BLOB" = "$SHA" ] || fail "blob id $BLOB != sha $SHA"
MID="$(curl -s --noproxy '*' -H "Authorization: Bearer $SEED_TOKEN" -H 'Content-Type: application/json' -d "{\"name\":\"qwen2.5-0.5b-real\",\"description\":\"real qwen weights\",\"files\":[{\"blob_id\":\"$BLOB\",\"path\":\"qwen.gguf\",\"size\":$WSIZE}],\"metadata\":{\"architecture\":\"qwen2\",\"parameter_count\":\"500000000\",\"format\":\"gguf\",\"quantization\":\"Q8_0\",\"kind\":\"llm\"}}" "$S/api/collections" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
[ -n "$MID" ] || fail "no collection id"
PUB="$(curl -s --noproxy '*' -X POST -H "Authorization: Bearer $SEED_TOKEN" -H 'Content-Type: application/json' -d "{\"collection_id\":\"$MID\"}" "$S/api/models/publish")"
need publish "$PUB" '"published"'
echo "real 644MB weights published on DHT"

SINFER="$(curl -s --noproxy '*' -X POST -H "Authorization: Bearer $SEED_TOKEN" -H 'Content-Type: application/json' -d "{\"prompt\":\"$PROMPT\",\"max_tokens\":32,\"temperature\":0}" "$S/api/models/$MID/infer")"
echo "$SINFER" | grep -qi "pineapple" || fail "seeder inference missed: $SINFER"
echo "$SINFER" | grep -q "FIXTURE-COMPLETION" && fail "seeder served the stub backend"
echo "seeder: real inference green"

LPULL=""
for _ in $(seq 1 30); do
  LPULL="$(curl -s --noproxy '*' -X POST -H "Authorization: Bearer $LEECH_TOKEN" "$L/api/models/$MID/pull")"
  echo "$LPULL" | grep -q '"complete"' && break
  sleep 10
done
need leech-pull "$LPULL" '"complete"'
LINFER="$(curl -s --noproxy '*' -X POST -H "Authorization: Bearer $LEECH_TOKEN" -H 'Content-Type: application/json' -d "{\"prompt\":\"$PROMPT\",\"max_tokens\":32,\"temperature\":0}" "$L/api/models/$MID/infer")"
echo "$LINFER" | grep -qi "pineapple" || fail "leecher inference missed: $LINFER"
echo "$LINFER" | grep -q "FIXTURE-COMPLETION" && fail "leecher served the stub backend"
echo "leecher: real inference over P2P-fetched weights green"
GOT="$(sha256sum "$TMP/leech/data/models/$MID/qwen.gguf" | cut -d' ' -f1)"
[ "$GOT" = "$SHA" ] || fail "leecher weights $GOT != $SHA"
echo "leecher: 644MB weights byte-identical after P2P transfer"

echo "LLM-REAL PASS"
