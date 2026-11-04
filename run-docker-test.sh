#!/bin/bash
set -e

cd "$(dirname "$0")"

# Build images via compose (creates git-forest-seeder, git-forest-bootstrap, git-forest-leecher)
echo "Building gitforest Docker images..."
docker compose build

# Create network if needed (ignore "already exists" errors)
docker network create gitforest 2>/dev/null || true

# Clean up previous runs (ignore errors if containers don't exist)
docker rm -f gf-seeder gf-bootstrap gf-leecher 2>/dev/null || true

echo "Starting seeder..."
docker run -d --name gf-seeder --network gitforest --hostname gf-seeder \
    -e GF_ROLE=seeder \
    -e GF_P2P_PORT=9418 \
    -e GF_API_PORT=7878 \
    -e GF_REPO_NAME=npcsh \
    -e GF_REPO_SOURCE=https://github.com/NPC-Worldwide/npcsh.git \
    -e GITFOREST_SKIP_AUTH=1 \
    git-forest-seeder

for i in $(seq 1 60); do
    if docker exec gf-seeder curl -s http://127.0.0.1:7878/api/id >/dev/null 2>&1; then
        echo "Seeder daemon ready"
        break
    fi
    /usr/bin/sleep 1
done

SEEDER_PEER_ID=$(docker exec gf-seeder curl -s http://127.0.0.1:7878/api/id | grep -oE '"peer_id":"[a-f0-9]+"' | cut -d'"' -f4)
echo "Seeder peer ID: $SEEDER_PEER_ID"

for i in $(seq 1 60); do
    if docker exec gf-seeder curl -s http://127.0.0.1:7878/api/repos | grep -q npcsh; then
        echo "Seeder has npcsh repo"
        break
    fi
    /usr/bin/sleep 2
done

echo "Starting bootstrap..."
docker run -d --name gf-bootstrap --network gitforest --hostname gf-bootstrap \
    -e GF_ROLE=bootstrap \
    -e GF_P2P_PORT=9418 \
    -e GF_API_PORT=7878 \
    -e GF_BOOTSTRAP_PEERS="\"/dns4/gf-seeder/tcp/9418\"" \
    -e GITFOREST_SKIP_AUTH=1 \
    git-forest-bootstrap

for i in $(seq 1 60); do
    if docker exec gf-bootstrap curl -s http://127.0.0.1:7878/api/id >/dev/null 2>&1; then
        echo "Bootstrap daemon ready"
        break
    fi
    /usr/bin/sleep 1
done

BOOTSTRAP_PEER_ID=$(docker exec gf-bootstrap curl -s http://127.0.0.1:7878/api/id | grep -oE '"peer_id":"[a-f0-9]+"' | cut -d'"' -f4)
echo "Bootstrap peer ID: $BOOTSTRAP_PEER_ID"

echo "Waiting for bootstrap to discover seeder..."
for i in $(seq 1 60); do
    PEERS=$(docker exec gf-bootstrap curl -s http://127.0.0.1:7878/api/peers)
    if echo "$PEERS" | grep -q "$SEEDER_PEER_ID"; then
        echo "Bootstrap discovered seeder"
        break
    fi
    /usr/bin/sleep 2
done

echo "Starting leecher clone test..."
docker run --rm --name gf-leecher --network gitforest --hostname gf-leecher \
    --entrypoint /usr/local/bin/run-test.sh \
    -e GF_ROLE=leecher \
    -e GF_P2P_PORT=9418 \
    -e GF_API_PORT=7878 \
    -e GF_BOOTSTRAP_PEERS="\"/dns4/gf-bootstrap/tcp/9418\"" \
    -e GITFOREST_SKIP_AUTH=1 \
    git-forest-leecher \
    "$SEEDER_PEER_ID"

echo "Docker test complete."
