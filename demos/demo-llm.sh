#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
MCP="$ROOT/target/debug/gitforest-mcp"
TMP="$(mktemp -d /tmp/gf-llm-XXXXXX)"
CLUSTER="llm-live-proof"

SEED_API=17898; SEED_P2P=19452
LEECH_API=17899; LEECH_P2P=19454
MARK="FIXTURE-COMPLETION"

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
  echo "LLM FAIL: $1"
  for n in seed leech; do
    echo "--- $n log tail ---"
    tail -n 15 "$TMP/$n.log" 2>/dev/null
  done
  exit 1
}

need() {
  echo "$2" | grep -q -- "$3" || fail "$1: missing [$3] in $2"
}

echo "== gitforest live federated-LLM proof =="
(cd "$ROOT" && cargo build --release --offline -p gt-daemon) || fail "build failed"
(cd "$ROOT" && cargo build --offline -p gt-mcp) || fail "mcp build failed"

printf '%s\n' '#!/bin/bash' 'echo "$@" >> "$0.argv.log"' 'P=""' 'while [ $# -gt 0 ]; do case "$1" in -f) P="$2"; shift 2;; *) shift;; esac; done' "echo \"$MARK for: \$(head -c 120 \"\$P\")\"" > "$TMP/fake-llama.sh"
chmod +x "$TMP/fake-llama.sh"
head -c 262144 /dev/urandom > "$TMP/tiny.gguf"
SHA="$(sha256sum "$TMP/tiny.gguf" | cut -d' ' -f1)"

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
llm_backend = "$TMP/fake-llama.sh"
EOF
}

mkconfig seed "$SEED_API" "$SEED_P2P" ""
mkconfig leech "$LEECH_API" "$LEECH_P2P" "\"/ip4/127.0.0.1/tcp/$SEED_P2P/api/$SEED_API\""
cat >> "$TMP/seed.toml" <<EOF

[compute]
worker_enabled = true
max_cpu_cores = 2
max_ram_mb = 4096
max_disk_mb = 8192
docker_socket = "/var/run/docker.sock"
hourly_rate_sats = 0
accepted_chains = []
wallets = {}
max_concurrent_jobs = 2
allowed_images = []
execution_mode = "local"
gpu_enabled = false
EOF

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

BLOB="$(curl -s --noproxy '*' -H "Authorization: Bearer $SEED_TOKEN" -H 'Content-Type: application/json' -d "{\"path\":\"$TMP/tiny.gguf\"}" "$S/api/blobs" | python3 -c 'import json,sys; print(json.load(sys.stdin)["blob_id"])')"
[ "$BLOB" = "$SHA" ] || fail "blob id $BLOB != sha $SHA"
MID="$(curl -s --noproxy '*' -H "Authorization: Bearer $SEED_TOKEN" -H 'Content-Type: application/json' -d "{\"name\":\"fixture-tiny\",\"description\":\"fixture model\",\"files\":[{\"blob_id\":\"$BLOB\",\"path\":\"tiny.gguf\",\"size\":262144}],\"metadata\":{\"architecture\":\"fixture-gpt\",\"parameter_count\":\"1000000\",\"format\":\"gguf\",\"quantization\":\"Q8_0\",\"kind\":\"llm\"}}" "$S/api/collections" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
[ -n "$MID" ] || fail "no collection id"
PUB="$(curl -s --noproxy '*' -X POST -H "Authorization: Bearer $SEED_TOKEN" -H 'Content-Type: application/json' -d "{\"collection_id\":\"$MID\"}" "$S/api/models/publish")"
need publish "$PUB" '"published"'
SEARCH="$(curl -s --noproxy '*' -H "Authorization: Bearer $SEED_TOKEN" "$S/api/models/search?text=fixture")"
need search "$SEARCH" "$MID"
PULL="$(curl -s --noproxy '*' -X POST -H "Authorization: Bearer $SEED_TOKEN" "$S/api/models/$MID/pull")"
need pull "$PULL" '"complete"'
INFER="$(curl -s --noproxy '*' -X POST -H "Authorization: Bearer $SEED_TOKEN" -H 'Content-Type: application/json' -d '{"prompt":"Say hello","max_tokens":16}' "$S/api/models/$MID/infer")"
need infer "$INFER" "$MARK"
need infer-prompt "$INFER" "Say hello"
ARGV="$(cat "$TMP/fake-llama.sh.argv.log")"
need argv "$ARGV" "\-m "
need argv "$ARGV" "\-f "
need argv "$ARGV" "\-n 16"
need argv "$ARGV" "--temp 0.7"
need argv "$ARGV" "--no-display-prompt"
echo "seeder: publish/search/pull/infer green, llama-cli contract honored"

export GITFOREST_DAEMON_URL="$S" GITFOREST_API_TOKEN="$SEED_TOKEN"
MSEARCH="$(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_models","arguments":{"text":"fixture"}}}' | "$MCP" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["content"][0]["text"])')"
need mcp-search "$MSEARCH" "$MID"
MINFER="$(printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"infer_model\",\"arguments\":{\"id\":\"$MID\",\"prompt\":\"Via MCP\",\"max_tokens\":16}}}" | "$MCP" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["content"][0]["text"])')"
need mcp-infer "$MINFER" "$MARK"
echo "mcp: search_models/infer_model green"

LPULL=""
for _ in $(seq 1 18); do
  LPULL="$(curl -s --noproxy '*' -X POST -H "Authorization: Bearer $LEECH_TOKEN" "$L/api/models/$MID/pull")"
  echo "$LPULL" | grep -q '"complete"' && break
  sleep 10
done
need leech-pull "$LPULL" '"complete"'
LINFER="$(curl -s --noproxy '*' -X POST -H "Authorization: Bearer $LEECH_TOKEN" -H 'Content-Type: application/json' -d '{"prompt":"Prove federation","max_tokens":16}' "$L/api/models/$MID/infer")"
need leech-infer "$LINFER" "$MARK"
GOT="$(sha256sum "$TMP/leech/data/models/$MID/tiny.gguf" | cut -d' ' -f1)"
[ "$GOT" = "$SHA" ] || fail "leecher weights $GOT != $SHA"
echo "leecher: DHT pull byte-identical, infer green"

SEEDERS=""
for _ in $(seq 1 6); do
  SEEDERS="$(curl -s --noproxy '*' -H "Authorization: Bearer $LEECH_TOKEN" "$L/api/models/search?text=fixture")"
  echo "$SEEDERS" | grep -q '"seeders":2' && break
  sleep 10
done
echo "$SEEDERS" | grep -q '"seeders":2' && echo "mesh: 2 seeders visible" || echo "WARN: seeder count did not reach 2 (non-fatal)"

mkdir -p "$TMP/repo/.gitforest/workflows"
cat > "$TMP/repo/.gitforest/workflows/llm-step.yml" <<EOF
name: LLMStep
on:
  workflow_dispatch:
jobs:
  ask:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    outputs:
      summary: \${{ steps.ask.outputs.completion }}
    steps:
      - id: ask
        uses: gitforest/llm@v1
        with:
          model: $MID
          prompt: "Say the word quasar"
          max-tokens: "16"
EOF
git -C "$TMP/repo" init -q --initial-branch=main
git -C "$TMP/repo" config user.email t@t; git -C "$TMP/repo" config user.name t
git -C "$TMP/repo" add .; git -C "$TMP/repo" commit -qm init
LSHA="$(git -C "$TMP/repo" rev-parse HEAD)"
curl -s --noproxy '*' -H "Authorization: Bearer $SEED_TOKEN" -H 'Content-Type: application/json' -X POST "$S/api/repos" -d '{"path":"'"$TMP/repo"'","name":"llmdemo"}' | grep -q '"llmdemo"' || fail "repo add failed"
LRUN="$(curl -s --noproxy '*' -H "Authorization: Bearer $SEED_TOKEN" -H 'Content-Type: application/json' -X POST "$S/api/repos/llmdemo/workflows/dispatch" -d '{"workflow":"llm-step.yml","commit_sha":"'"$LSHA"'"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["runs"][0]["id"])')"
[ -n "$LRUN" ] || fail "llm-step dispatch failed"
LDONE=""
for _ in $(seq 1 45); do
  SUM="$(curl -s --noproxy '*' -H "Authorization: Bearer $SEED_TOKEN" "$S/api/workflows/runs/$LRUN" | python3 -c "import json,sys; s=json.load(sys.stdin)['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  [ "$SUM" = "1/1" ] && { LDONE="yes"; break; }
  sleep 2
done
[ "$LDONE" = "yes" ] || fail "llm-step run never completed (last: $SUM)"
LOUT="$(curl -s --noproxy '*' -H "Authorization: Bearer $SEED_TOKEN" "$S/api/compute/jobs/$(curl -s --noproxy '*' -H "Authorization: Bearer $SEED_TOKEN" "$S/api/workflows/runs/$LRUN" | python3 -c 'import json,sys; print(json.load(sys.stdin)["jobs"][0]["id"])')")"
echo "$LOUT" | grep -q "$MARK" || fail "llm-step output lacks marker: $LOUT"
echo "$LOUT" | grep -q "quasar" || fail "llm-step output lacks prompt echo"
echo "workflow llm step ran inference and captured the completion as a job output"

echo "LLM PASS"
