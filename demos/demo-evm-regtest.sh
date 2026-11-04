#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ANVIL_PORT=18545
TMP="$(mktemp -d /tmp/gf-evm-XXXXXX)"

cleanup() {
  docker rm -f gf-anvil-regtest >/dev/null 2>&1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "EVM REGTEST FAIL: $1"
  exit 1
}

echo "== gitforest EVM L2 regtest proof =="
docker rm -f gf-anvil-regtest >/dev/null 2>&1
docker run -d --name gf-anvil-regtest -p "127.0.0.1:$ANVIL_PORT:8545" \
  ghcr.io/foundry-rs/foundry:nightly "anvil --chain-id 8453 --host 0.0.0.0" >/dev/null || fail "anvil start failed"

READY=""
for _ in $(seq 1 30); do
  CHAIN="$(curl -s --noproxy '*' -X POST "http://127.0.0.1:$ANVIL_PORT" -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' || true)"
  if echo "$CHAIN" | grep -q "0x2105"; then READY="yes"; break; fi
  sleep 1
done
[ "$READY" = "yes" ] || fail "anvil did not come up"

KEY="$(docker logs gf-anvil-regtest 2>&1 | grep -o "0x[0-9a-f]\{64\}" | head -n 1)"
[ -n "$KEY" ] || fail "could not read anvil key"
ADDR="$(GF_EVM_SIGNER_KEY="$KEY" python3 -c "import os; from eth_account import Account; print(Account.from_key(os.environ['GF_EVM_SIGNER_KEY']).address)")"
[ -n "$ADDR" ] || fail "could not derive payer address"
echo "anvil up on chain 8453, payer $ADDR"

python3 -c "import eth_account" 2>/dev/null || fail "python3 eth_account missing (pip install eth-account)"

export GF_EVM_REGTEST_URL="http://127.0.0.1:$ANVIL_PORT" GF_EVM_REGTEST_FROM="$ADDR" GF_EVM_SIGNER_KEY="$KEY"
(cd "$ROOT" && timeout 200 cargo test --offline -p gt-compute --test evm_regtest -- --skip evm_regtest_embedded 2>&1 | tee /tmp/gf-evm-cargo.log | tail -n 4; test "${PIPESTATUS[0]}" -eq 0) || fail "external-signer regtest failed"

echo "external-signer run done; now proving the embedded keystore signer..."
(cd "$ROOT" && cargo build --offline -p gt-daemon 2>&1 | tail -n 1) || fail "daemon build failed"
install -m 600 /dev/null "$TMP/key.txt"
echo -n "$KEY" > "$TMP/key.txt"
install -m 600 /dev/null "$TMP/pw.txt"
echo -n "regtest-pw" > "$TMP/pw.txt"
(cd "$ROOT" && ./target/debug/gitforest compute evm-keystore-create --path "$TMP/keystore.json" \
  --key-file "$TMP/key.txt" --password-file "$TMP/pw.txt") || fail "keystore create failed"
rm -f "$TMP/key.txt"
[ "$(stat -c %a "$TMP/keystore.json")" = "600" ] || fail "keystore not 0600"
export GF_EVM_KEYSTORE_PATH="$TMP/keystore.json" GITFOREST_EVM_KEY_PASSWORD="regtest-pw"
(cd "$ROOT" && timeout 200 cargo test --offline -p gt-compute --test evm_regtest evm_regtest_embedded -- --test-threads=1 2>&1 | tee /tmp/gf-evm-emb.log | tail -n 4; test "${PIPESTATUS[0]}" -eq 0) || fail "embedded regtest failed"

echo "EVM REGTEST PASS"
