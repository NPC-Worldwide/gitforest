# Payment settlement: Lightning + EVM L2

Both money tiers are implemented behind the existing `PaymentBackend` trait
(`crates/gt-compute/src/payment.rs`). Either, both, or neither can be
configured; with neither, the node runs mutual-credit / free mode only.

## Why these two

- **Bitcoin Lightning** is the micropayment rail: sub-cent, instant, and its
  hold invoices are purpose-built escrow (lock now, settle on job success,
  cancel on failure — no smart contract needed).
- **Ethereum has no live Lightning equivalent.** Raiden stalled and Connext
  pivoted to bridging, so channel networks are a dead end. The working
  equivalent is **EVM L2 transfers** (Base, Arbitrum, Optimism, Polygon):
  sub-cent fees and ~1s confirmation make plain per-job transfers the
  micropayment primitive, with signed receipts for accounting. Trustless
  on-chain escrow contracts are a later step; v1 moves funds on release.

## Architecture

```
PaymentManager ──► LndBackend ──gRPC──► external LND node (hold invoices)
               └─► EvmL2Backend ─RPC──► L2 endpoint + external signer process
```

Deliberately, the daemon holds **no chain keys** in v1:

- Lightning goes through an external LND node over gRPC (LND's
  hold-invoice API has no REST equivalent — verified against v0.18.5).
  The daemon only holds the admin macaroon (read from a file, never
  stored in config) and per-escrow preimages in `lnd_escrows.json`.
  TLS pins the exact `tls.cert` bytes: stock LND certs carry `CA:TRUE`,
  which chain-building verifiers reject, so fingerprint matching plus
  handshake-signature verification is the only correct mode — there is
  no skip-verify flag.
- EVM signing is delegated to an external command. The daemon spawns it with
  the unsigned transfer in env vars (`TX_TO`, `TX_VALUE_WEI`, `TX_NONCE`,
  `TX_GAS_PRICE_WEI`, `TX_GAS_LIMIT`, `TX_CHAIN_ID`) and reads the `0x`-hex
  signed raw transaction from stdout. Bring your own signer: `cast`, `clef`,
  a hardware-wallet wrapper, or a test echo.

Fail-closed behaviors: EVM creation checks `eth_chainId` against the
configured network before anything is signed; LND calls carry the macaroon
on every request; unknown escrow ids are rejected without any chain call;
a server cert that is not byte-identical to the pinned one aborts the
connection.

## Escrow lifecycle

Lightning escrows live on the **worker** side; EVM intents live on the
**submitter** side. Both flow through `PaymentManager`; all chain calls run
in `spawn_blocking` so the async runtime never stalls.

**Lightning (worker side):**

1. `remote_submit` accepts → worker opens a hold invoice for
   `max_payment_sats` and returns the invoice as `payment_details`.
2. The submit tick pays the invoice from the submitter's LND node
   (fee capped at 1%, min 1 sat) and returns as soon as the payment is
   `IN_FLIGHT` — the HTLC is locked and the invoice reads `ACCEPTED`.
   The payer then stops tracking; the payment persists server-side
   until settle/cancel/expiry (waiting for terminal state would fail
   every job longer than the payment timeout). No invoice, or no local
   LND backend, means the job runs on mutual credit.
3. The worker runs the job only after the invoice reads ACCEPTED/SETTLED;
   unpaid jobs are deferred, and foreign jobs still unpaid after 24h are
   dropped with the invoice canceled (payer made whole).
4. Job success → settle with the preimage. Failure → cancel.
5. Payer-protect cancel: when the submitter's tick requeues a stale
   match (30 min without a result) or resets a ghosted verifier, it
   POSTs a signed `JobCancelMessage` to the worker's
   `/api/compute/remote/cancel` (cluster auth + ed25519 submitter
   signature, verified against the job's recorded submitter). The
   worker refunds its Lightning escrow immediately, drops the job and
   its fetched repo copy, and answers `{cancelled: true}` — idempotent
   (`already terminal` for completed/failed/disputed jobs, 404 once
   dropped). Wrong-submitter cancels are rejected without touching
   anything. This shrinks the stuck-funds window from invoice expiry /
   the 24h sweep to ~30 min, and also frees worker slots held by
   abandoned mutual-credit jobs. Proven by `./demos/demo-cancel.sh` (live
   submit → impostor rejected → submitter cancel → job gone →
   repeat 404).

Known limitation (accepted): nothing couples a job's max duration to
its invoice's expiry. Defaults compose safely (1h job cap vs 24h
invoice), but an operator who raises job timeouts near or past the
invoice expiry can strand a worker on an expired hold. Fixing it
properly means threading per-job durations into escrow creation;
until someone needs >24h Lightning jobs, the safe defaults stand.

**EVM (submitter side):**

1. On match, the submitter opens a channel intent (chain-id checked, no
   funds move) addressed to the worker's wallet from the accept response.
2. First valid result wins: that worker's channel is released (transfer
   broadcast) and every other channel for the job is dropped.
3. Failure, stale-match requeue, or no result → the intent is dropped with
   no broadcast. Funds never leave the submitter until a result lands.

**Double-pay safety:** a re-matched job can hold one channel per worker;
settlement releases the winner and refunds the rest by payee id, so only
the worker whose result was accepted is ever paid.

## Configuration

```toml
[payments.lnd]
grpc_url = "https://127.0.0.1:10009"
macaroon_path = "/home/you/.lnd/data/chain/bitcoin/regtest/admin.macaroon"
tls_cert_path = "/home/you/.lnd/tls.cert"   # pinned exactly; no skip mode

[[payments.evm]]
network = "base"
rpc_url = "http://127.0.0.1:8545"
from_address = "0x..."
signer_command = "mysigner"
```

Backends that fail to load (missing macaroon, unknown network) log a warning
and are skipped; the daemon keeps running on the remaining tiers.

### EVM settlement semantics

Release means *mined*, not broadcast. The backend stores the tx hash on
first broadcast, waits for the receipt (45 attempts x 2s), and only then
drops the escrow. Retries re-check the known hash instead of
rebroadcasting, so a slow chain cannot double-pay; a reverted tx clears
the hash for a safe fresh retry (reverts move no funds). Refund after a
broadcast is refused with the hash — on-chain movement cannot be taken
back, so the entry stays visible for manual recovery. Pending escrows
persist per network (`evm_escrows_<network>.json`) and reconcile at
startup: mined entries drop, failed ones clear, unconfirmed ones wait.

## Proving it on regtest (host runbook)

Both tiers have one-command proofs (docker required):

- `./demos/demo-ln-regtest.sh` — boots bitcoind + two LND 0.18.5 nodes, funds
  them, opens a channel, then runs `lightning_regtest`: hold → pay →
  `ACCEPTED` → settle moves exactly the escrowed sats (within routing +
  commitment fee noise), cancel refunds. Asserts the invoice stays
  `ACCEPTED` after the payer stops tracking.
- `./demos/demo-evm-regtest.sh` — boots anvil as a Base-copy chain (8453) and
  runs `evm_regtest` with `scripts/evm_sign.py` (eth-account signer):
  release moves real wei to the worker, refund moves nothing. Note: the
  signer must checksum-encode the destination address; eth-account
  rejects bare-lowercase `to` fields.

## What is still missing

- Solana backend (trait slot reserved). Design note, not just work:
  Solana has no native hold-invoice equivalent, so it cannot mirror
  the Lightning escrow flow. The honest options are (a) direct
  pay-on-verified-result, which trusts the submitter to pay after
  delivery and inverts the current trust model (today the worker is
  paid from an already-locked hold), or (b) an escrow program owned
  by the network, which needs its own program audit before mainnet
  funds touch it. Either way it also needs the Agave SDK tree
  (hundreds of crates) plus a `solana-test-validator` regtest proof
  in the style of `demos/demo-ln-regtest.sh`. Do not start it as a filler
  task; it is a design decision plus a heavy dependency commitment.

The embedded signer exists (no longer missing): `EmbeddedSigner`
signs legacy EIP-155 transactions in-process (secp256k1 + keccak +
RLP, deliberately minimal instead of alloy) from a
chacha20poly1305/pbkdf2 keystore file. Configure with
`keystore_path` (+ optional `key_password_file`, else
`GITFOREST_EVM_KEY_PASSWORD`); it wins over `signer_command` when
both are set, and the backend refuses to load when the keystore
address differs from `from_address`. Create one with `gitforest
compute evm-keystore-create --path … (--key-file … | --generate)
--password-file …`. Threat analysis lives under T53; the anvil
regtest (`demos/demo-evm-regtest.sh`) exercises both external and
embedded signing.

## Mutual credit (the no-crypto tier)

With no money backend configured, jobs settle in mutual credit, which
is owed to the **network**, not to any one peer: compute you give to
anyone offsets compute you take from anyone. The unit is the
compute-second (`max(1, ceil(execution_time_ms / 1000))`), so metering
is uniform across chains.

Each node keeps a local `CreditLedger` (`mutual_credit.json` in the
data dir): per-peer given/taken seconds plus a settled-paid set. Only
**completed, unpaid** jobs accrue:

- Worker side: a finished foreign job records `given` unless a payment
  channel exists for it or a money backend covers its chain (Lightning
  pre-pay gates execution; EVM is trust-release v1, so configuring the
  backend opts that chain out of credit accounting).
- Submitter side: an accepted remote result records `taken` unless the
  job was paid — Lightning marks paid when the invoice pays, EVM when
  the channel releases. Verify jobs record both runs.
- Paid consumption never touches the ledger, and self-run jobs accrue
  nothing either way.

Enforcement is two-sided, both bypassed when a money backend covers
the job's chain:

- Workers refuse remote submits from peers whose net consumption from
  them exceeds `credit_allowance_secs` (`accepted: false`, reason
  names mutual credit).
- Our own daemon refuses local submits past our own net deficit with
  HTTP 402; the CLI translates that into the earn-or-pay hint.

Defaults: 3600s allowance (one free compute-hour per relationship and
globally). Set `credit_allowance_secs` very high for unconditional
free mode.

Visibility: `GET /api/compute/credit` (totals, deficit, allowance,
`may_spend`, per-peer table) and `gitforest compute credit`.

Sybil note: a fresh identity resets its deficit with strangers, so
credit is a spam throttle, not money — PoW-gated offers (T27),
submitter validity scoring (T29), and the money tiers remain the
answer for adversarial settings. Recorded as T58.

Live proof: two daemons, real cluster auth, worker allowance 3s. A
5s-sleep job completed with sub taken=6 / worker given=6 on both
APIs and both `mutual_credit.json` files; the next submit stayed
`pending` with both ledgers byte-unchanged (worker refusing).

Manual cancel also exists: `POST /api/compute/jobs/{id}/cancel`
(local auth, own jobs only — foreign jobs are 403) marks the job
`Cancelled`, refunds local channels, and notifies the matched
worker/verifier over the same signed remote message
(`remote_notified` in the response). `gitforest compute cancel
<id>` drives it from the CLI. Proven in `./demos/demo-cancel.sh`'s local
section and by endpoint tests (pending→cancelled+refunded,
matched→worker notified, terminal no-op, foreign forbidden).
