# Threat Model: GitForest Pre-Production Security Pass

**Feature**: 002-threat-model
**Created**: 2026-10-28
**Status**: Living
**Input**: Systematically enumerate attack surfaces across the daemon, P2P stack,
compute runner, transfer pipeline, and desktop client; fix what is fixable;
record the rest with mitigations and residuals.

---

## 1. Scope and adversaries

In scope: HTTP API, git protocol server, libp2p DHT/gossip handling,
Docker-based job runner, blob/collection transfer pipeline, remote compute
transport, Electron desktop IPC shell.

Adversaries considered:

1. Network attacker: LAN observer, DNS/MITM where TLS is absent.
2. Malicious peer: valid self-generated identity, arbitrary protocol messages.
3. Malicious job submitter: attacks the worker that runs their job.
4. Malicious worker: attacks the submitter (bad results, snooping on job data).
5. Local attacker: different uid on the same machine.
6. Malicious website: rendered in the operator's browser while the daemon runs.

Out of scope for this pass: Linux kernel / Docker escape primitives, git binary
CVEs, libp2p internals, chain payment backends (not yet built), social
engineering.

---

## 2. Trust boundaries

- Local HTTP API trusts the Bearer token, nothing else. Token must never leave
  the machine (constitution III); the cluster token is the deliberate,
  narrow exception for multi-machine deployments.
- Anything arriving over P2P, gossip, DHT records, or peer HTTP is
  attacker-controlled until a signature check or content-hash check passes.
- Job submitters are untrusted by workers: every job field (requirements,
  timeout, image, env, artifacts, timeout) is hostile input.
- Workers are untrusted by submitters for privacy (they see job inputs) and
  for correctness (only integrity is verifiable, see T24).

---

## 3. Findings

Severity: Critical / High / Medium / Low. Status: Fixed / Mitigated / Open /
Accepted / Recorded.

### 3.1 Network API and auth

- T1 (High, Fixed): `api.token` was written world-readable (0644), so any
  local user gained full daemon control. Fix: `write_secret_file()` creates
  0600 and tightens existing files on every daemon start
  (`crates/gt-daemon/src/api.rs`). Test: secret-permission roundtrip incl.
  tightening.
- T2 (High, Fixed): permissive CORS let any website scrape unauthenticated
  endpoints through the operator's browser. Fix: CORS removed entirely;
  nothing needs it (desktop uses IPC, machine clients are non-browser).
- T3 (Medium, Audited clean): all 12 mutating endpoints require the local
  Bearer token (remote push additionally accepts the cluster token). The one
  unauthenticated POST (`remote fetch`) is read-only by design.
- T4 (High, Mitigated by design): raw git protocol over TCP refuses
  `receive-pack`; pushes go through the authenticated API path with ACL
  checks (`crates/gt-daemon/src/network.rs`).
- T5 (Medium, Fixed): the API had no rate limiting, so brute-force and
  spam were possible. Fix: per-IP fixed-window limiter
  (`crates/gt-daemon/src/ratelimit.rs`), 600 reads/min and 60 writes/min,
  429 + Retry-After on exceed, keyed on socket address only (no spoofable
  X-Forwarded-For). Proven by a live localhost flood test.
- T6 (Low, Fixed): git protocol reads had no timeout (slowloris held a
  task). Fix: 30s per-read timeouts, request-line cap of 4KB, and a
  64-connection semaphore that sheds load when full. Proven by timeout,
  oversize, and unknown-repo tests.
- T56 (Medium, Fixed): the libp2p swarm had no connection limits, so an
  attacker could exhaust memory/tasks with slowloris handshakes or sheer
  connection count (120s idle timeout only bounds established idlers).
  Fix: connection-limits behaviour — 32 max pending incoming, 128 max
  established incoming, 3 per peer; outbound left uncapped for Kademlia.
  Proven by a live flood test (40 held handshakes, capped at 32) with a
  no-caps negative control, plus an under-cap liveness test. The
  established/per-peer caps share the same enforced behaviour but are
  not live-flooded (needs 128+ nodes); the values are compile-time
  constants, raisable without coordination since they are local policy.
- T7 (Medium, Accepted): the API binds 0.0.0.0 because peer-to-peer fetch and
  remote compute require it. Compensating controls: auth on mutation (T3),
  no CORS (T2), strict parsing. Operators who run single-node should still
  firewall 7878 to localhost.
- T52 (High, Fixed): the daemon web UI could not call any authenticated
  endpoint (no token in the browser), so every UI mutation 401'd and the
  submit form had no auth story. Fix: loopback-gated session cookie.
  `serve_ui` mints `gitforest_session=<api_token>` with HttpOnly +
  SameSite=Strict, but only on HTML responses served to loopback — LAN
  clients get the page with no cookie, so the token never crosses the
  network despite the 0.0.0.0 bind (T7). `check_auth` accepts the
  cookie exactly like the header token (exact-name match across all
  Cookie headers, verified before the cluster-token fallback). CSRF:
  SameSite=Strict keeps evil-site form/fetch POSTs cookie-less so they
  401; no CORS (T2) blocks reads; DNS rebinding to 127.0.0.1 stays
  cross-origin so the cookie is withheld there too. Residuals: on
  multi-user hosts any local user can load the loopback UI and mint a
  session (the 0600 token file never had this exposure) — the assumed
  model is a single-user laptop; shared-machine operators should
  firewall 7878. A compromised browser profile also equals token
  theft, same as the desktop keychain. Proven by cookie
  set/parse/accept/loopback-gate tests plus a live LAN-IP negative
  and a live cookie-authenticated job submit.

### 3.2 Compute runner (untrusted-job containment)

- T8 (Critical, Fixed): container CPU/RAM limits were taken from the
  submitter-controlled job requirements, so a malicious job could claim 1TB
  RAM and starve the host. Fix: `ResourceCaps` from worker config clamps
  every request (`effective_limits`, floors of 1 core / 64MB so a zero
  misconfiguration cannot mean unlimited).
- T9 (High, Fixed): job timeout came from the job (up to u64::MAX), pinning a
  worker slot forever. Fix: clamped to `max_job_timeout_secs` (default 1h,
  configurable, backward-compatible serde default).
- T10 (High, Fixed): any job could request host-networked containers.
  Fix: `allow_network_jobs` worker gate, default deny; enforced in matching
  so networked jobs never even get claimed.
- T11 (High, Fixed): stdout/stderr/artifacts were read fully into RAM, so a
  `yes`-loop job OOMed the worker. Fix: 8MB tail reads for logs, 64MB cap
  with count limit (16) for artifacts; oversized artifacts are skipped.
- T12 (Low, Fixed): container names derived from job ids. Was fail-closed
  (Docker rejects bad names); now sanitized to the Docker-safe alphabet.
- T13 (Low, Fixed): env keys were interpolated unvalidated. Now restricted
  to `[A-Za-z_][A-Za-z0-9_]*`; invalid keys are dropped.
- T14 (Medium, Fixed): jobs had no disk quota. Fix: per-job quota from
  `max_disk_mb`, enforced three ways — oversized repos are refused before
  running, a 5s watchdog kills jobs that fill past quota mid-run (exit
  -3), and job dirs are always cleaned up. Proven by a live local run
  that fills 3MB against a 1MB quota and dies with -3. Residual: Docker
  image pulls are bounded only by the `allowed_images` gate; XFS project
  quotas remain the structural extra.
- T15 (Medium, Accepted): jobs run as root inside the container, matching
  standard CI runners. Contained by cap-drop ALL, no-new-privileges, PID
  limits, and read-only repo mounts. User-namespace remapping on the host
  Docker daemon is the recommended extra layer.
- T40 (Medium, Fixed): cap-drop ALL without DAC_OVERRIDE made the job
  bind-mount unwritable (container root could not write host-owned files),
  so every Docker job failed. Fix: re-add only DAC_OVERRIDE, which permits
  writes inside the job mount but grants nothing outside the container's
  mounts. Proven by a live Docker test.
- T41 (High, Fixed): Docker jobs always reported exit 0 because the exit code
  was read from the container wait response, whose command ends in a trailing
  `echo` that always succeeds. A failing build looked green. Fix: read the
  real code from the `exit_code` file the command writes. Regression test
  asserts exit 3 propagates.
- T42 (Medium, Fixed): Docker artifacts were collected from a directory that
  was never populated, and steps ran directly on the seeded repo. Fix: the
  repo is copied to per-job scratch mounted read-write; artifacts collect
  from there and the seeded repo is never touched.
- T16 (High, Accepted): Docker socket access is root-equivalent by nature.
  Inherent to a Docker-based runner; documented, not fixable in-app.
- T17 (High, Fixed in earlier pass): the `allowed_images` setting was never
  enforced. Now enforced; empty still means allow-all with a loud warning.
- T18 (High, Mitigated): job env vars and inputs are visible to the worker by
  construction. Mitigation: the submit API now returns a `warning` field
  whenever a job carries env vars on a daemon that runs jobs remotely
  (worker mode off), so every client can surface it; local-only
  submitters see no warning. Follow-ups mostly closed: the CLI
  `compute submit` now POSTs to the daemon and prints the warning
  (401 without the token file), and the daemon web UI gained a submit
  form — repo select, yaml prefilled from the new authed `GET
  /api/repos/{name}/ci-config` (authed because CI env may hold
  secrets), warning rendered in orange — both proven live against an
  auth-enforcing daemon. The Electron UI has the same form now
  (`ComputePage.tsx`, authenticated through the existing IPC proxy
  which already sends the token file's Bearer [REDACTED] typechecked clean and
  its full call sequence proven live. All T18 follow-ups closed.
- T60 (Critical, Fixed): container-job `volumes` and service/container
  `ports` were trusted from the submit message. Volume sources went
  straight into the Docker bind string with no validation outside the
  workflow parser, so a direct API or remote submit of
  `{source: "/etc", target: "/host"}` (or `/var/run/docker.sock`)
  would bind-mount host paths into the attacker's container; the
  workflow parser's named-volume check never ran on that path.
  Likewise `ports` with `host:`/`ip:` published arbitrary worker-host
  ports on any interface. Fix at the trust boundary (the runner, so
  workflow, direct, and remote submits are all covered): volume
  sources must be named volumes (alnum start, `[alnum_.-]`, 128
  max), targets must be absolute container paths without `..`,
  and every published host port is pinned to 127.0.0.1
  (GitHub-runner semantics; the workflow `ip` field is now
  ignored). Violations fail the job before any container exists.
  Proven by four unit tests plus a live two-daemon proof: a hostile
  `/etc` submit failed with no container created while an identical
  named-volume control completed exit 0. Residual: refusal happens
  at run time rather than submit-accept time, so hostile jobs
  briefly occupy a worker slot; reputation floors are the backstop.

### 3.3 Transfer pipeline (blobs/models)

- T19 (Critical, Fixed): `store_from_download` never checked assembled bytes
  against the blob id, so a lying seeder's metadata poisoned the store with
  garbage that would then be re-served. Fix: whole-content re-hash with
  delete-on-mismatch, plus a lying-meta regression test.
- T20 (Medium, Audited clean): all DHT record parsing is `if let Ok`
  (serde/json), no panics on attacker bytes; piece indexes are bounds-checked
  before use.
- T21 (Low, Mitigated): gossip-supplied peer addresses drive outbound HTTP
  (SSRF-shaped). Impact analysis: requests carry no credentials and responses
  have no return path — bytes must survive meta-id match, per-piece hash,
  and whole-content hash to be stored. Defense in depth: cloud metadata IP
  (169.254.169.254, dotted and decimal forms) is blocked at dial time.
  Residual: DNS-named internal hosts are not blocked; impact stays at
  network probing.
- T22 (Medium, Fixed): `known_peers` grew unbounded from gossip (memory
  exhaustion). Fix: 1024-entry cap with oldest-first eviction.

### 3.4 Remote compute transport

- T23 (High, Fixed): remote push trusted a self-asserted pusher id behind
  a shared cluster token, so cluster members could impersonate each other
  (including the repo owner). Fix: the pusher signs
  `push_signable_payload` (repo + refs header + bundle hash) and the
  daemon verifies against the claimed id before any ACL check; missing or
  bad signatures get 401, mixed-pusher requests get 400. Push proxies
  forward the signature for end-to-end verification. Protocol change:
  unsigned pushes are now rejected; the remote helper signs automatically
  and fails closed without a local key.
- T24 (High, Partially mitigated): result *integrity* is verified (worker
  signature, byte-identical reconstruction) but result *correctness* is
  not — a worker can lie about the exit code. Now live: every result
  records worker reputation (success/failure/dispute/ghost), bad
  signatures dispute immediately, ghosts decay matching priority, and
  workers below 0.15 stop receiving jobs. Replay is now live for jobs with
  `verify: true`: the first result is held as a candidate in `Verifying`,
  the tick matches an independent worker (proven excluded from the first
  by a trap-endpoint test), the second run must produce an identical
  exit code and log hashes, a match completes the job and pays both
  workers, a mismatch disputes the job, fails both reputations, and
  refunds; verifier ghosts reset to no-verifier so the tick retries.
  Live two-worker replay is proven outside the harness by
  `demos/demo-verify.sh` (3 daemons, real cluster auth). Operators who want
  verification on everything set `[compute] require_verification = true`,
  which forces `verify` on every submitted job even when the request
  opts out. Residual: the policy defaults off for backwards
  compatibility, so a submitter that never opts in still trusts first
  results (reputation remains the backstop).
- T25 (Medium, Mitigated): result replay and double-completion are blocked
  because only `Matched`-state jobs accept results and only from the matched
  worker (`Verifying`-state jobs accept exactly one more result, only from a
  different worker, and reject the first worker with 409); job ids are
  unguessable.
- T26 (Medium, Fixed): a worker that accepts then ghosts left the job stuck
  in `Matched`. Fix: `matched_at` timestamp plus a 30-minute timeout in the
  submitter tick that requeues stale matches to `Pending` for another worker.
- T38 (Medium, Fixed): collection-manifest spoofing (a seeder serving a forged
  model card). Fix: fetched manifests must id-match the request and pass
  owner-signature verification before storage; member blobs enter the normal
  hash-verified fetch pipeline.
- T39 (High, Accepted with guardrails): local execution mode runs job code
  directly on the host with no container isolation. Contained by: Docker is
  the default and local is explicit opt-in (`--local` / config); the repo is
  copied to scratch so seeded repos are never polluted; timeouts, log and
  artifact caps, env sanitization, and job-dir cleanup still apply. Intended
  only for trusted clusters (own machines, friends, CI runners without
  Docker). Never enable for open-network jobs.
- T27 (High, Fixed): identities were free and started at neutral
  reputation, so Sybil workers could burn-and-replace. Fix: compute PoW —
  every worker offer carries a nonce proving ~2^16 hashes over its peer
  id (milliseconds to mine once, one hash to verify, cached in
  `identity.pow`). Offers without valid PoW are dropped at ingest, and
  the offer signature now covers all fields plus the nonce (previously it
  covered only a static string, so fields were malleable). The upgrade
  path then landed: 16 bits is now a genesis floor, miners aim at a
  configurable `pow_target_bits` (cache re-mines once on raise), and
  verifiers enforce a configurable `min_pow_bits` that can never be set
  below the floor. Raises need no flag day — stronger proofs always
  pass weaker checks — and registry entries stamp measured strength
  so persisted offers that no longer qualify are evicted on load
  (legacy entries load as floor-strength). Residual: determined GPU
  farms price out at ~millions of ids, not zero.
- T28 (Medium, Mitigated by design): receipt collusion farming (fake
  counterparties minting credits) needs bilateral signatures and repo-bound
  receipts; community ratio floors bound the payoff. The money tiers remain
  the answer for adversarial settings.
- T29 (Medium, Fixed): only workers had reputation; submitter griefing
  was untracked. Fix: per-submitter validity scoring (runnable vs junk),
  persisted across restarts; submitters below 0.2 are refused at submit
  time. Queue-flooding stays bounded at 32 active jobs per submitter.
  Junk signals: unparseable configs and unfetchable repos; every executed
  job confirms validity.
- T43 (Medium, Fixed): API handlers had no time bound; a stalled handler
  (slow body, hung downstream) held a connection forever. Fix: 60s
  request-timeout middleware returning 408. Streaming bodies are unaffected
  (the timeout covers handler completion, and downloads build their
  response headers immediately).
- T44 (Medium, Fixed): LND hold-invoice preimages lived in daemon memory,
  so a restart between lock and settle stranded the escrow. Fix: the
  preimage map persists to a 0600 `lnd_escrows.json` on every mutation and
  reloads on startup, restoring settle/cancel capability. Failed
  settle/cancel calls keep the entry for retry instead of dropping it.
  Restored records are reconciled against LND on startup (terminal
  invoices dropped, the rest kept); if LND is unreachable everything is
  kept for retry.
- T45 (Medium, Mitigated by design): EVM signing has two paths. The
  external-signer path keeps private keys out of the daemon entirely:
  the signer command receives the unsigned transfer over env vars and
  prints the signed raw tx (its command line is visible in the process
  table, so secrets go by file/env, never argv). The embedded path
  (see T53) seals the key in an encrypted keystore instead, with
  rotation via `compute evm-keystore-rotate` — it verifies the old
  keystore unlocks and matches its filed address, refuses to
  overwrite, seals a fresh key 0600, and prints the fund/repoint/
  restart/drain checklist. Both backends fail closed when the RPC
  chain id does not match the configured network.
- T46 (Low, Fixed): payment backends use blocking clients. Audited
  every call site: all six blocking trait methods
  (create/release/refund/escrow_funded/pay_invoice/check_balance) are
  invoked from async code only inside `tokio::task::spawn_blocking`
  (dispatch settle/open/funding/pay paths plus the remote-submit
  escrow creation); `check_balance` is additionally only called from
  sync regtest code. Everything else the daemon touches on the
  manager is in-memory reads or startup-time registration.
- T47 (High, Fixed): the LND backend spoke REST (`/v1/invoices/hold`,
  `/v2/router/send`), but LND's hold-invoice API is gRPC-only — those
  endpoints 404, so Lightning escrow could never have worked. Fix: native
  gRPC client (hand-rolled prost messages against the v0.18.5 protos, no
  protoc needed). Proven on regtest: hold → pay → accept → settle moves
  exactly the escrowed sats, cancel refunds.
- T48 (Medium, Fixed): stock LND TLS certificates carry `CA:TRUE`, which
  modern TLS verifiers reject as end-entity certs — CA-pinning can never
  work against a real LND node. Fix: the gRPC transport pins the exact
  `tls.cert` bytes (fingerprint match) and still verifies handshake
  signatures, with no skip-verify mode anywhere. A cert rotation requires
  updating `tls_cert_path`, by design.
- T49 (Medium, Fixed): payer tracking waited for terminal payment state,
  which hold invoices never reach until the worker settles — so every job
  longer than the payment timeout failed its HTLC and stranded the job.
  Fix: `pay_invoice` returns once the payment is `IN_FLIGHT` (funds
  locked, invoice `ACCEPTED`); the stream is dropped and the HTLC
  persists server-side until settle/cancel/expiry. Regtest asserts the
  invoice stays `ACCEPTED` after the payer stops tracking.
- T50 (Medium, Accepted with expiry): `cargo audit` reports 7
  vulnerabilities pinned by upstream majors: h2 0.3 via bollard 0.18
  (localhost docker socket peer only), hickory-proto/ring-0.16/
  rustls-webpki-0.101 via libp2p 0.54. Ignored in `.cargo/audit.toml`
  with per-advisory reasons; re-check on every libp2p/bollard bump.
  A full `cargo update` cleared the other 16. Unused `bincode` and
  unmaintained `rustls-pemfile` were removed from our own deps.
- T51 (High, Fixed): blob and collection DHT announcements signed
  static strings (`b"blob-announce"`, `b"collection-announce"`), so a
  signature transplanted to any blob/manifest id and any size — free
  tracker poisoning (attribute fake seeders to victims) and storage-less
  Sybil seeding. Fix: `signable_payload()` binds peer, id, size, and
  counts on both messages, signed at all 4 announce sites and verified
  in both DHT consumers; transplant/wrong-signer tests added. Live
  2-daemon proof (`demos/demo-blobs.sh`): 5MB/20-piece blob transfers with
  byte-identical sha256 and the collection manifest replicates.
- T57 (Medium, Fixed): result resource usage was neither collected nor
  covered by any signature — `ResourceUsage` was hardcoded zeros, the
  remote-submit path rebuilt it as zeros, and neither the inner result
  signature nor the result message bound it, so billing-grade metering
  was absent and any future usage field would have been malleable in
  transit. Fix: the Docker runner samples one-shot container stats at
  2Hz during execution (peak CPU by the standard cpu/system-delta
  formula, peak RSS, job-dir bytes measured before cleanup) and both
  `signable_payload()`s now bind all three usage fields; the ship path
  copies them from the signed result and the submitter rebuilds from the
  message. Covered by peak-math, tamper-evidence (inner + message), and
  API-passthrough tests. Live proof: a Docker job burning CPU/RAM/disk
  reported `101.9% CPU, 21 MB RAM, 20 MB disk` end to end through the
  daemon API and CLI. Residual: usage is worker-measured, so a worker
  can still under-report its own consumption — the signatures prove the
  numbers were not altered in transit, not that they are honest;
  replay verification (T24) deliberately excludes usage since it
  legitimately varies between runs.
- T58 (Medium, Mitigated): mutual credit was a log line — free-mode
  jobs ran untracked, so one leecher could burn every worker
  indefinitely. Fix: the `CreditLedger` meters completed unpaid jobs
  in compute-seconds, persisted per peer; workers refuse submitters
  past `credit_allowance_secs` of net consumption and our own daemon
  402s local submits past our own net deficit, with money-covered
  jobs bypassing both gates and paid consumption excluded via
  invoice-paid marks and released-channel checks. Residual: ledgers
  are local, so a Sybil rebirth resets deficit with strangers —
  this bounds casual leeching, not a determined farm; T27 PoW,
  T29 submitter scoring, and the money tiers are the backstops.
  Live 2-daemon proof: 6s accrued symmetrically, next submit
  refused (pending, ledgers unchanged).
- T59 (High, Partially mitigated): a production daemon's DHT event
  loop died silently — no panic, no shutdown log — while the HTTP
  API kept serving 200s, so the node looked healthy while all
  dials, announces, and lookups were down; `/api/peers` even
  reported the failure as `connected: 0` with no alarm. Observed
  after a rolling restart of all bootstrap peers (273 stale
  known-peers, zero live swarm use). Fix so far: `DhtNodeSender`
  exposes liveness, `/api/status` reports `dht_alive`, and
  `/api/peers` warns when the command channel is dead, so
  monitoring and heartbeats catch it. Residual: root cause of the
  loop death is unknown (no trace in logs) and there is no
  supervisor to rebuild the swarm — a recurrence still needs a
  daemon restart. Next step on recurrence: capture a live
  `tokio-console`/backtrace before restarting instead of
  restarting first.
  Update after a second recurrence (local daemon, P2P listener
  gone, API alive, `dht_alive: false`): audit found no Shutdown
  senders, no reachable `break`, and no fallible `unwrap` in the
  loop — the only remaining in-code exit is a task panic, whose
  payload previously went to stderr only. Fix: the loop's
  `JoinHandle` is now awaited by a watchdog that logs panics at
  error with the payload string (plus a loud warn on clean exit
  and on channel-close-without-shutdown); the daemon installs a
  panic hook that logs every panic with a forced backtrace and
  appends it to `<data_dir>/panics.log`. Proven by a payload
  formatter unit test, a real-node shutdown→`!is_alive`
  integration test, and a clean boot/shutdown cycle with no
  spurious artifacts. Residual: no auto-restart yet (deliberate —
  rebuilding the swarm means replacing `state.dht` and its event
  consumer; operator restarts for now), and the original panic
  cause is still unknown — the next recurrence leaves a
  backtrace instead of silence.

### 3.5 Secrets and desktop

- T30 (High, Fixed): identity key is 0600 from creation, no
  write-then-chmod window.
- T31 (Medium, Fixed): `openExternal` accepted any scheme from a
  server-provided URL. Now restricted to http(s).
- T32 (Medium, Fixed): there was no Content-Security-Policy on
  web/Electron renderers; renderer XSS would reach full daemon control via
  IPC. Fix: CSP header on every daemon-served static response and a CSP
  meta tag in the Electron entry HTML (object-src none, no framing, no
  external hosts). Inline scripts are now gone: the daemon UI's script
  moved to `/app.js` with click delegation (no `onclick` anywhere,
  enforced by test), and the Electron source CSP bans both script and
  style inline. The style-src residual then closed the same way: the
  319-line `<style>` block moved to `/app.css`, all 38 inline
  `style=` attributes (8 static, 30 JS-generated) became utility
  classes with byte-identical declarations, and the one dynamic
  width (transfer progress bar) is set via CSSOM after render —
  `style-src 'self'` with no unsafe-inline, enforced by test on
  both the header and the embedded bytes, and verified on the
  live-served header. The desktop build residual closed too: the
  app was rebuilt (deb + AppImage verified to ship the CSP meta
  tag, submit/cancel UI, and fresh binaries). No residuals left.
- T55 (High, Fixed): EVM release marked the worker paid on
  transaction *broadcast*, before any confirmation. A dropped,
  reverted, or reorged transaction silently stiffed the worker,
  pending escrows lived only in memory (a restart forgot them),
  and a settle retry after a slow broadcast would have paid
  twice. Fix: the backend records the tx hash, waits for a
  successful receipt before dropping the escrow, retries check
  the known hash instead of rebroadcasting, reverted hashes
  clear for a safe fresh attempt, post-broadcast refunds are
  refused with the hash, and per-network escrow stores
  reconcile at startup. Proven by six mock-chain tests
  (mine/drop/timeout/retry/refuse/reconcile) plus the live
  anvil regtest through both signer paths.
- T33 (Low, Fixed): `.env` was tracked in git. Untracked and added to
  `.gitignore` (with `.env.*`, keeping `.env.example` allowed).
- T34 (Low, Fixed): `GITFOREST_SKIP_AUTH` disabled auth when merely set, so
  an empty value (e.g. from compose interpolation) silently opened the API.
  Fix: only the exact values `1`/`true` skip auth; the compose file is
  labeled test-only and the container entrypoint already unsets the var in
  daemon mode.
- T53 (High, Fixed): the embedded EVM signer needed somewhere to keep the
  raw secp256k1 key. Fix: encrypted keystore file
  (`gt_compute::keystore`): chacha20poly1305 + pbkdf2-sha256/210k
  with random salt/nonce per file, written 0600 with tighten-on-write,
  wrong-password and tamper sharing one error string. The password
  comes from a 0600-enforced file or `GITFOREST_EVM_KEY_PASSWORD`,
  never argv or config; the CLI refuses over-permissive secret files
  and refuses to overwrite. The filed address is recomputed from the
  key and matched against `from_address` at backend load (fail
  closed). Residuals: transient in-memory key copies during signing
  (the secp API takes ownership; only the at-rest copy is zeroized),
  no mlock against swap, legacy type-0 transactions only, and no key
  rotation story yet. Proven by roundtrip/wrong-password/tamper/0600
  tests, an eth-account byte-identical signature vector, and an anvil
  release/refund regtest through the embedded path.
- T54 (High, Fixed): peer API resolution silently failed or misrouted
  whenever `advertise_addr` was unset or the API port was non-default.
  A node with no `advertise_addr` shared no self address over peer
  exchange, so bootstrapped peers could only resolve it via DHT
  identify — which carried no API port (resolver guessed 7878) and
  routinely lost the startup race. Worse, the resolver scored
  non-loopback hosts above loopback, so a DHT entry could beat the
  known-good bootstrap address and route signed pushes/ref
  fetches at an attacker's host:port or hang the API with no
  timeout. Fix: exchange responses now carry the responder's peer
  ID and the exchanger tags the bootstrap address it just reached
  (legacy bare-list responses still parse, minus the tag);
  identify agent versions advertise `api/{port}` (parsed
  strictly, port 0 and garbage ignored); explicit `/api/`
  entries outrank guessed-default ones; all three git proxy
  dials share the 30s-timeout no-proxy client; the git
  resolver skips metadata-IP hosts like the compute path
  already did. Residuals: `/api/remote/*` stays
  unauthenticated by design (pushes authenticate via
  signature + ACL, not the proxy hop), so anyone who
  knows a peer ID can make a daemon dial the address
  that peer ID maps to — only 169.254.169.254 is
  blocked, since blocking LAN/loopback would break
  legitimate deployments; firewall 7878 per the
  operator guidance. Proven by five unit tests
  (tagging, explicit-wins, metadata skip, exchange
  shape, agent parse) and the four multi-machine
  integration tests going from 0/4 to 4/4.

### 3.6 Supply chain and operations

- T35 (Medium, Fixed): there was no `cargo audit` in CI. Fix: audit-check
  job added; clippy gate widened to `--all-targets` and the build to match.
- T36 (Medium, Fixed): Kademlia MemoryStore had no eviction; record
  spam could grow memory. Fix: `BoundedStore` (value cap with
  oldest-first eviction, provider cap with ignore-on-overflow so a
  flood cannot flush legitimate providers) wired into the node
  behaviour and covered by eviction/refresh/oversize/provider tests.
- T37 (Low, Fixed): peer-controlled strings reached logs unsanitized
  (terminal escape injection). Audit found the live vectors: raw git
  protocol command/repo (unauthenticated TCP), `remote_*` path params
  (percent-decoded, on unauthenticated reads), proxied peer URLs,
  libp2p identify agent strings, mDNS/dial multiaddrs, DHT record
  decode errors echoing record bytes, and reqwest/serde errors
  echoing peer bodies. Fix: `gt_core::sanitize::sanitize_log`
  (control chars → `?`) applied at every Display-interpolated
  remote-influenced value and error across network/api/dispatch/
  fetch/evm/lightning/dht; `{:?}` sites were already safe (Debug
  escapes) and provably-safe types (uuids, hex hashes, enums,
  validated ids, operator config) were left alone. Proven by
  sanitizer unit tests plus a live poisoned-input test (escape-laden
  git pkt-line and `%1b`/`%0a` HTTP path): zero raw ESC bytes in the
  log, payloads visible but inert.

---

## 4. Experiments run this pass

Hostile inputs were probed and then frozen as regression tests rather than
left as notes:

- Greedy requirements (1024 cores, 2^40 RAM, u64::MAX timeout) clamp to
  worker ceilings (`test_effective_limits_clamp_to_caps`).
- Traversal and hostile container names (`../../x`) sanitize safely.
- Oversized logs/artifacts (100KB file vs 1KB cap) truncate or reject.
- Metadata-IP dial forms (dotted, decimal) blocked; loopback/LAN/DNS pass.
- Gossip flood (1034 peers) holds the 1024 cap with oldest evicted.
- Old `[compute]` config sections without the new keys parse with defaults.
- Lying seeder metadata (valid piece hashes, wrong content id) is rejected
  and purged (prior pass, re-verified green).
- Write flood (65 rapid POSTs) trips the per-IP limiter with 429 and
  Retry-After; read/write buckets stay independent.
- Silent git client is dropped at the read timeout; 64KB request line and
  unknown repo are rejected without touching any repo.
- 33rd concurrent job from one submitter is refused on both submit paths.
- 6 ghosted matches sink a worker below the 0.15 match floor; 4 junk
  submits sink a submitter below the 0.20 validity floor and get refused.
- 3MB fill against a 1MB quota is killed live with exit -3; 3MB repo
  against a 1MB quota is refused before running.

---

## 5. Operator guidance (until opens close)

- Firewall 7878 to localhost unless running a real cluster.
- Set `allowed_images` on every worker; leave `allow_network_jobs` off.
- Put `data_dir` on its own partition; enable Docker userns-remap.
- Never place secrets in jobs for workers you would not ssh into.
- Run `chmod 600 ~/.local/share/gitforest/api.token` once on machines
  provisioned before this pass (new starts self-heal).
