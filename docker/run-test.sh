#!/bin/bash
set -e

CONFIG_DIR=/root/.config/gitforest
DATA_DIR=/root/.local/share/gitforest
P2P_PORT=9418
API_PORT=7878
BOOTSTRAP_PEERS=${GF_BOOTSTRAP_PEERS:-""}

# If a seeder peer ID was passed as an argument, use it; otherwise discover it.
SEEDER_PEER_ID=${1:-}

mkdir -p "$CONFIG_DIR" "$DATA_DIR"

gitforest init
PEER_ID=$(gitforest id)
echo "Leecher peer ID: $PEER_ID"

if [ -n "$BOOTSTRAP_PEERS" ]; then
    cat > "$CONFIG_DIR/config.toml" <<EOF
data_dir = "$DATA_DIR"
p2p_port = $P2P_PORT
api_port = $API_PORT
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = [$BOOTSTRAP_PEERS]
EOF
else
    cat > "$CONFIG_DIR/config.toml" <<EOF
data_dir = "$DATA_DIR"
p2p_port = $P2P_PORT
api_port = $API_PORT
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = []
EOF
fi

# Start daemon in background
gitforest --config "$CONFIG_DIR/config.toml" daemon &
DAEMON_PID=$!
echo "Daemon PID: $DAEMON_PID"

# Wait for API to be ready
for i in $(seq 1 30); do
    if curl -s http://127.0.0.1:7878/api/id >/dev/null 2>&1; then
        echo "Daemon API ready"
        break
    fi
    /usr/bin/sleep 1
done

# Discover peers if no seeder ID was provided
if [ -z "$SEEDER_PEER_ID" ]; then
    echo "Waiting for peer discovery..."
    for i in $(seq 1 60); do
        PEERS=$(curl -s http://127.0.0.1:7878/api/peers)
        SEEDER_PEER_ID=$(echo "$PEERS" | grep -oE '/gf/[a-f0-9]{64}' | head -1 | sed 's#/gf/##' || true)
        if [ -n "$SEEDER_PEER_ID" ]; then
            echo "Discovered seeder peer ID: $SEEDER_PEER_ID"
            break
        fi
        /usr/bin/sleep 2
    done
fi

if [ -z "$SEEDER_PEER_ID" ]; then
    echo "ERROR: Could not discover seeder peer ID"
    kill $DAEMON_PID || true
    exit 1
fi

# Wait until the daemon has learned a network route to the target seeder.
echo "Waiting for route to $SEEDER_PEER_ID ..."
for i in $(seq 1 60); do
    PEERS=$(curl -s http://127.0.0.1:7878/api/peers)
    if echo "$PEERS" | grep -q "/gf/$SEEDER_PEER_ID"; then
        echo "Route to seeder available"
        break
    fi
    /usr/bin/sleep 2
done

# Clone from GitForest
mkdir -p /root/clones
cd /root/clones
echo "Cloning gitforest://$SEEDER_PEER_ID/npcsh ..."
git clone "gitforest://$SEEDER_PEER_ID/npcsh" npcsh-clone

# Verify
cd npcsh-clone
COMMIT_COUNT=$(git rev-list --count HEAD)
echo "Cloned npcsh with $COMMIT_COUNT commits"

if [ "$COMMIT_COUNT" -lt 1 ]; then
    echo "ERROR: clone appears empty"
    kill $DAEMON_PID || true
    exit 1
fi

echo "SUCCESS: clone worked over GitForest network"
kill $DAEMON_PID || true
