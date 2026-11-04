#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d /tmp/gf-ln-XXXXXX)"
NET="gf-ln-regtest"
BTC_RPC_USER="gf"
BTC_RPC_PASS="gfpass9"
ALICE_REST=18081
BOB_REST=18082
WALLET_PASS="regtestpass1234"

cleanup() {
  docker rm -f gf-lnd-alice gf-lnd-bob gf-bitcoind >/dev/null 2>&1
  docker network rm "$NET" >/dev/null 2>&1
  rm -rf "$TMP" 2>/dev/null || docker run --rm -v "$TMP:/clean" alpine:3 rm -rf /clean >/dev/null 2>&1
}
trap cleanup EXIT

fail() {
  echo "LN REGTEST FAIL: $1"
  echo "--- bitcoind log ---"
  docker logs gf-bitcoind 2>&1 | tail -n 10
  echo "--- alice log ---"
  docker logs gf-lnd-alice 2>&1 | tail -n 10
  exit 1
}

echo "== gitforest Lightning regtest proof =="
docker rm -f gf-lnd-alice gf-lnd-bob gf-bitcoind >/dev/null 2>&1
docker network rm "$NET" >/dev/null 2>&1
docker network create "$NET" >/dev/null || fail "network create failed"

mkdir -p "$TMP/btc"
RPCAUTH="$(python3 -c "import hmac,os; salt=os.urandom(16).hex(); pw='$BTC_RPC_PASS'; print(f'$BTC_RPC_USER:{salt}\$' + hmac.new(salt.encode(), pw.encode(), 'sha256').hexdigest())")"
cat > "$TMP/btc/bitcoin.conf" <<EOF
regtest=1
server=1
rpcauth=$RPCAUTH

[regtest]
rpcbind=0.0.0.0
rpcallowip=0.0.0.0/0
zmqpubrawblock=tcp://0.0.0.0:28332
zmqpubrawtx=tcp://0.0.0.0:28333
fallbackfee=0.00001
EOF

docker run -d --name gf-bitcoind --network "$NET" -v "$TMP/btc:/home/bitcoin/.bitcoin" \
  ruimarinho/bitcoin-core:latest >/dev/null || fail "bitcoind start failed"

bcli() {
  docker exec -u bitcoin gf-bitcoind bitcoin-cli -regtest \
    -rpccookiefile=/home/bitcoin/.bitcoin/regtest/.cookie "$@"
}
for _ in $(seq 1 40); do
  if bcli getblockcount >/dev/null 2>&1; then break; fi
  sleep 2
done
bcli getblockcount >/dev/null 2>&1 || fail "bitcoind rpc never came up"
echo "bitcoind up"
bcli createwallet miner >/dev/null 2>&1
MINER_ADDR="$(bcli getnewaddress)"
[ -n "$MINER_ADDR" ] || fail "no miner address"
bcli generatetoaddress 101 "$MINER_ADDR" >/dev/null || fail "premine failed"
echo "premined 101 blocks (clears IBD so LND boots)"

ALICE_GRPC=11009
BOB_GRPC=11010
start_lnd() {
  local name="$1" alias="$2" rest="$3" p2p="$4" grpc="$5"
  docker run -d --name "$name" --network "$NET" -p "127.0.0.1:$rest:8080" -p "127.0.0.1:$p2p:9735" -p "127.0.0.1:$grpc:10009" \
    lightninglabs/lnd:v0.18.5-beta \
    --bitcoin.active --bitcoin.regtest --bitcoin.node=bitcoind \
    "--bitcoind.rpchost=gf-bitcoind:18443" "--bitcoind.rpcuser=$BTC_RPC_USER" "--bitcoind.rpcpass=$BTC_RPC_PASS" \
    --bitcoind.zmqpubrawblock=tcp://gf-bitcoind:28332 --bitcoind.zmqpubrawtx=tcp://gf-bitcoind:28333 \
    --restlisten=0.0.0.0:8080 --rpclisten=0.0.0.0:10009 --listen=0.0.0.0:9735 \
    --noseedbackup "--alias=$alias" >/dev/null || fail "$name start failed"
}
start_lnd gf-lnd-alice alice "$ALICE_REST" 19735 "$ALICE_GRPC"
start_lnd gf-lnd-bob bob "$BOB_REST" 19736 "$BOB_GRPC"

rest() {
  curl -s -k --noproxy '*' "$@"
}
for node in gf-lnd-alice gf-lnd-bob; do
  for _ in $(seq 1 60); do
    if docker exec "$node" test -f /root/.lnd/data/chain/bitcoin/regtest/admin.macaroon 2>/dev/null; then break; fi
    sleep 2
  done
done
ALICE_MAC="$(docker exec gf-lnd-alice cat /root/.lnd/data/chain/bitcoin/regtest/admin.macaroon | xxd -p -c 1000)"
BOB_MAC="$(docker exec gf-lnd-bob cat /root/.lnd/data/chain/bitcoin/regtest/admin.macaroon | xxd -p -c 1000)"
[ -n "$ALICE_MAC" ] && [ -n "$BOB_MAC" ] || fail "could not read macaroons"

for spec in "$ALICE_REST:$ALICE_MAC" "$BOB_REST:$BOB_MAC"; do
  port="${spec%%:*}"; mac="${spec##*:}"
  SYNCED=""
  for _ in $(seq 1 90); do
    if rest -H "Grpc-Metadata-macaroon: $mac" "https://127.0.0.1:$port/v1/getinfo" | grep -q '"synced_to_chain": *true'; then SYNCED="yes"; break; fi
    sleep 2
  done
  [ "$SYNCED" = "yes" ] || fail "lnd on $port never synced"
done
echo "lnd nodes synced"

newaddr() {
  rest -H "Grpc-Metadata-macaroon: $2" "https://127.0.0.1:$1/v1/newaddress?type=WITNESS_PUBKEY_HASH" \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['address'])"
}
ALICE_ADDR="$(newaddr "$ALICE_REST" "$ALICE_MAC")"
[ -n "$ALICE_ADDR" ] || fail "no alice address"
bcli generatetoaddress 110 "$ALICE_ADDR" >/dev/null || fail "mining failed"
echo "mined 110 blocks to alice"

for _ in $(seq 1 60); do
  BAL="$(rest -H "Grpc-Metadata-macaroon: $ALICE_MAC" "https://127.0.0.1:$ALICE_REST/v1/balance/blockchain" \
    | python3 -c "import json,sys; print(json.load(sys.stdin).get('confirmed_balance','0'))" 2>/dev/null || echo 0)"
  if [ "$BAL" != "0" ]; then break; fi
  sleep 3
done
[ "$BAL" != "0" ] || fail "alice never saw funds (balance $BAL)"
echo "alice on-chain balance: $BAL"

BOBKEY="$(rest -H "Grpc-Metadata-macaroon: $BOB_MAC" "https://127.0.0.1:$BOB_REST/v1/getinfo" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['identity_pubkey'])")"
[ -n "$BOBKEY" ] || fail "no bob pubkey"
printf '{"addr": {"pubkey": "%s", "host": "gf-lnd-bob:9735"}}' "$BOBKEY" > "$TMP/peer.json"
CONNECTED=""
PEER_RESP=""
for _ in $(seq 1 12); do
  PEER_RESP="$(rest -H "Grpc-Metadata-macaroon: $ALICE_MAC" -X POST "https://127.0.0.1:$ALICE_REST/v1/peers" -d @"$TMP/peer.json")"
  if rest -H "Grpc-Metadata-macaroon: $ALICE_MAC" "https://127.0.0.1:$ALICE_REST/v1/peers" | grep -q "$BOBKEY"; then CONNECTED="yes"; break; fi
  sleep 10
done
echo "peer connect: $PEER_RESP"
[ "$CONNECTED" = "yes" ] || fail "peer never connected: $PEER_RESP"
OPEN_RESP="$(rest -H "Grpc-Metadata-macaroon: $ALICE_MAC" -X POST "https://127.0.0.1:$ALICE_REST/v1/channels" \
  -d "{\"node_pubkey_string\": \"$BOBKEY\", \"local_funding_amount\": \"1000000\"}")"
echo "$OPEN_RESP" | grep -q "funding_txid" || fail "channel open failed: $OPEN_RESP"
bcli generatetoaddress 6 "$ALICE_ADDR" >/dev/null || fail "confirm mining failed"
echo "channel opened, confirming..."

ACTIVE=""
for _ in $(seq 1 60); do
  ACTIVE="$(rest -H "Grpc-Metadata-macaroon: $ALICE_MAC" "https://127.0.0.1:$ALICE_REST/v1/channels" \
    | python3 -c "import json,sys; cs=json.load(sys.stdin).get('channels',[]); print('yes' if cs and all(c.get('active') for c in cs) else 'no')" 2>/dev/null || echo no)"
  if [ "$ACTIVE" = "yes" ]; then break; fi
  sleep 3
done
[ "$ACTIVE" = "yes" ] || fail "channel never became active"
echo "channel active"

docker cp gf-lnd-alice:/root/.lnd/tls.cert "$TMP/alice-tls.cert" || fail "no alice tls cert"
docker cp gf-lnd-bob:/root/.lnd/tls.cert "$TMP/bob-tls.cert" || fail "no bob tls cert"
export GF_LN_ALICE_URL="https://127.0.0.1:$ALICE_GRPC" GF_LN_ALICE_MAC="$ALICE_MAC" GF_LN_ALICE_CERT="$TMP/alice-tls.cert"
export GF_LN_BOB_URL="https://127.0.0.1:$BOB_GRPC" GF_LN_BOB_MAC="$BOB_MAC" GF_LN_BOB_CERT="$TMP/bob-tls.cert"
(cd "$ROOT" && timeout 280 cargo test --offline -p gt-compute --test lightning_regtest -- --nocapture 2>&1 | tee /tmp/gf-ln-cargo.log | tail -n 12; test "${PIPESTATUS[0]}" -eq 0) || fail "regtest failed, cargo log kept at /tmp/gf-ln-cargo.log"

echo "LN REGTEST PASS"
