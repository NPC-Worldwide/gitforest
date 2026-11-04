# Implementation Plan: GitForest Production Product

**Feature**: 001-production-product | **Spec**: specs/001-production-product/spec.md
**Target**: Hackathon week → tag `v0.3.0` release + live bootstrap network + desktop client MVP

## Architecture Decision: Desktop Client Stack

**Decision**: Electron + Vite 6 + React 18 + Tailwind + `npcts`, following the established
workspace pattern (vixynt, scherzo, tezcat, taipa). App lives at `desktop/` (separate npm tree,
NOT a cargo workspace member). Scaffold implemented: ~1,470 LOC TypeScript, type-checked.

Rationale:
- Matches the team's four existing desktop apps exactly — same electron-builder config shape
  (deb/AppImage/dmg/nsis), same main/preload/renderer structure, same tailwind theme vars.
- `npcts` component library is in active local use (`^0.1.25`) — layout, primitives available
  without new dependencies.
- Electron main process owns `gitforest` daemon lifecycle (spawn/attach/SIGTERM on quit),
  reads the bearer token from `~/.local/share/gitforest/api.token`, and proxies API calls over
  IPC so the renderer never handles auth directly.
- The embedded web UI (`gt-web`) stays for headless/browser use unchanged.
- Alternative considered: Tauri 2 (Rust-native, ~10 MB) — rejected: breaks the team's
  established pattern, no npcts support. Electron costs binary size (~120 MB) but buys
  velocity during the hackathon.

## Workstreams (mapped to user stories)

### WS-A: Bootstrap network goes live (US-6) — Day 1 morning
1. Verify `our_api_multiaddr()` + `GF_ADVERTISE_ADDR` path from `.claude/plan.md` is complete.
2. `terraform apply` in deploy/gcloud (e2-medium x3, static IPs, persistent disks).
3. Run `generate-bootstrap-peers.sh` → commit bootstrap default list into gt-core config defaults.
4. Healthcheck: curl `/api/status` on each node; each sees >=2 peers.

### WS-B: Release hardening (US-6) — Day 1 afternoon
1. Add SHA256SUMS generation to release.yml; verify .deb installs on clean ubuntu:22.04 container.
2. Verify install.sh end-to-end on fresh VM; add `--version` smoke step to CI.
3. Versioning: workspace version bump to 0.3.0 in Cargo.toml; v0.3.0 tag at hackathon end.

### WS-C: Daemon API extensions (US-1,3,4,5) — Day 1–2
New endpoints in gt-daemon:
- `GET /api/first-run` — identity exists? key-backup-done flag?
- `POST /api/identity/export` (passphrase-encrypted, rage/age format) + `POST /api/identity/import` (explicit overwrite flag)
- `GET/POST /api/profile` — signed profile record, published/read via DHT key `profile/<peer_id>`
- `GET /api/analytics/summary`, `GET /api/analytics/history?window=24h`
- `POST /api/daemon/restart`, `GET /api/daemon/logs?tail=N`, `GET /api/version/latest` (GitHub release check, cached 1h)
- Metrics collector: sample transfer bytes/s, peer count, job durations every 5s into sqlite at data_dir/metrics.db (ring buffer capped 7 days, delete-on-corrupt)

### WS-D: Desktop client MVP (US-1,2,3,5) — Day 2–4  [SCAFFOLD DONE — desktop/]
Scaffold complete: Electron main/preload, daemon lifecycle IPC, first-run wizard, 6 pages
(Repos/Network/Analytics/Compute/Profile/Settings), themed to team standard. Remaining:
- Wire first-run clone step to a real `/api/repos/clone` endpoint (add it — not in current API)
- Wire identity export/import buttons to T2.2/T2.3 endpoints once they land
- Fill repo detail pages as API response shapes stabilize
- Package with electron-builder into the release workflow alongside CLI tarballs

### WS-E: Analytics UI (US-4) — Day 4–5
uPlot (tiny, canvas-based — no chart.js bloat): throughput sparklines, peer count over time, transfer history table, blob-store usage bar, compute job history table.

### WS-F: Payments gate (US-7) — Day 5 (half day)
`compute.payments_enabled=false` default; UI "Experimental" banner; write `specs/002-real-payments/spec.md` design stub (Lightning escrow flow + threat model).

### WS-G: Polish, tag, ship (US-6) — Day 5–6
1. README quickstart rewrite, desktop-first.
2. `cargo test`, clippy, fmt green; new tests: identity export/import roundtrip, metrics cap, first-run endpoint, profile sign/verify.
3. Tag v0.3.0 → verify release assets → install.sh on clean VM → two-laptop demo run-through.

## Daily schedule (6-day hackathon)

| Day | Focus | Exit criteria |
|-----|-------|---------------|
| 1 | WS-A + WS-B + WS-C start | Bootstrap nodes live; release pipeline verified; first-run endpoint works |
| 2 | WS-C done + WS-D scaffold | All new endpoints merged; Tauri shell renders sidebar from live API |
| 3 | WS-D core | Repo list/detail/add in client; MR inbox viewable |
| 4 | WS-D done + WS-E | First-run wizard E2E on fresh VM; analytics charts rendering |
| 5 | WS-F + WS-G start | Payments gated; full test suite green; README rewrite |
| 6 | Freeze, tag, verify | v0.3.0 tagged, assets verified, demo on 2 fresh machines |

## Risks & mitigations

| Risk | Mitigation |
|------|------------|
| Tauri learning curve burns a day | WS-D scaffold timeboxed 4h on Day 2; fallback = ship enhanced web UI as a PWA/desktop wrapper |
| GCE quota/permission issues | WS-A scheduled first; fallback = any 3 reachable VMs (terraform pattern is provider-agnostic) |
| Metrics sqlite bloat/corruption | Ring buffer capped 7 days; on open failure, delete + recreate |
| Identity export security bug | rage/age encryption, passphrase min 12 chars, zeroize buffers after write |
| MR accept/reject touches collab signing | GUI only calls EXISTING signed-MR API; no new crypto this week |
| Scope creep (real payments, Windows) | Constitution explicitly out-of-scopes both |

## Validation
- Automated: cargo test (existing + new), clippy -D warnings, fmt check, CI (no Windows)
- Manual: two-laptop demo — A hosts, B fresh-installs via install.sh + desktop app, clones, opens MR, A accepts in GUI
- Network: every bootstrap node `/api/peers` shows the other two; leecher clone with default config only
