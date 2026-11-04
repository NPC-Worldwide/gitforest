# gitforest Product Constitution

**Version**: 1.0 | **Ratified**: 2026-10-23 | **Product Stage**: Pre-production → Production (hackathon push)

## Mission
gitforest ships as a real product: a decentralized P2P git hosting network that any developer can
install in one step and use through a polished desktop app. No company controls the network;
the client is the product.

## Core Principles

### I. Binary-first distribution
Every release must produce a downloadable raw binary (Linux x64, macOS arm64, .deb) published as
a gitforest release asset, installable via `install.sh` one-liner. No "clone and cargo build" for end users.

### II. Daemon + native-feel desktop client
The `gitforest` daemon owns the network; the desktop client owns the experience. Client talks to the
daemon ONLY through the local HTTP API (127.0.0.1:7878) using its bearer token. No business logic
lives in the client that isn't in the API first.

### III. Security is non-negotiable
Ed25519 identity = the account. API token never leaves the local machine. All pushes signed.
Compute jobs sandboxed in Docker. NO real crypto payments until escrow flows are audited — stub
backend stays behind a feature flag labeled "experimental".

### IV. Every feature is API-complete and tested
A feature is not done until (a) the HTTP API exposes it, (b) the CLI can drive it, (c) the desktop
client renders it, and (d) an automated test or documented example exercises it. Workspace must
stay green: `cargo test`, `cargo clippy`, `cargo fmt --check`.

### V. One-command onboarding
First-run flow: install → launch desktop app → auto-start daemon → identity generated →
guided tour → user can clone or host a repo within 5 minutes.

### VI. Network reality
The network must genuinely boot: ≥3 persistent bootstrap nodes running current release, and a
fresh install must be able to clone a repo from the DHT without manual peer configuration.

## Out of Scope (for production v0.1)
- Windows support (deliberately dropped; revisit post-hackathon)
- Real on-chain payments (stub backend only, clearly labeled)
- Mobile clients
- Multi-user/repo hosting server mode (each user runs their own node)
