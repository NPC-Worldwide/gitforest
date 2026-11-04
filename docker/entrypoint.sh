#!/bin/bash
set -e

MODE=${GF_MODE:-daemon}
ROLE=${GF_ROLE:-node}
CONFIG_DIR=${GF_CONFIG_DIR:-/root/.config/gitforest}
DATA_DIR=${GF_DATA_DIR:-/root/.local/share/gitforest}
REPO_DIR=${GF_REPO_DIR:-/root/repos}
P2P_PORT=${GF_P2P_PORT:-9418}
API_PORT=${GF_API_PORT:-7878}
BOOTSTRAP_PEERS=${GF_BOOTSTRAP_PEERS:-}
ADVERTISE_ADDR=${GF_ADVERTISE_ADDR:-}
CLUSTER_TOKEN=${GF_CLUSTER_TOKEN:-}
REPO_NAME=${GF_REPO_NAME:-npcsh}
REPO_SOURCE=${GF_REPO_SOURCE:-}

mkdir -p "$CONFIG_DIR" "$DATA_DIR" "$REPO_DIR"

# Generate identity if absent
if [ ! -f "$CONFIG_DIR/identity.key" ]; then
    gitforest init
fi

PEER_ID=$(gitforest id)
echo "Peer ID: $PEER_ID"

# Build config.toml. The bootstrap_peers env var is expected to already be a
# comma-separated list of quoted multiaddrs, e.g.:
#   "/ip4/10.0.0.2/tcp/9418/p2p/abc.../api/7878"
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

if [ -n "$ADVERTISE_ADDR" ]; then
    printf 'advertise_addr = "%s"\n' "$ADVERTISE_ADDR" >> "$CONFIG_DIR/config.toml"
fi

if [ -n "$CLUSTER_TOKEN" ]; then
    printf 'cluster_token = "%s"\n' "$CLUSTER_TOKEN" >> "$CONFIG_DIR/config.toml"
fi

# If a repo source is provided, clone it once and add it to the network
if [ -n "$REPO_SOURCE" ] && [ ! -d "$REPO_DIR/$REPO_NAME/.git" ]; then
    echo "Cloning $REPO_SOURCE into $REPO_DIR/$REPO_NAME ..."
    git clone "$REPO_SOURCE" "$REPO_DIR/$REPO_NAME"
fi

# If the repo exists, add it to gitforest
if [ -d "$REPO_DIR/$REPO_NAME/.git" ]; then
    if ! grep -q "name = \"$REPO_NAME\"" "$CONFIG_DIR/config.toml"; then
        gitforest --config "$CONFIG_DIR/config.toml" add "$REPO_DIR/$REPO_NAME" --name "$REPO_NAME" --description "Test repo"
    fi
fi

# Seed multiple repos from a newline-separated list: name=url
# Example: GF_REPO_LIST="npcsh=https://github.com/NPC-Worldwide/npcsh.git\nnpcpy=https://..."
process_repo_list() {
    local list="${GF_REPO_LIST:-}"
    [ -z "$list" ] && return

    echo "$list" | tr ';' '\n' | while IFS='=' read -r repo_name repo_url; do
        [ -z "$repo_name" ] && continue
        [ -z "$repo_url" ] && continue
        repo_name=$(echo "$repo_name" | xargs)
        repo_url=$(echo "$repo_url" | xargs)
        if [ ! -d "$REPO_DIR/$repo_name/.git" ]; then
            echo "Cloning $repo_url into $REPO_DIR/$repo_name ..."
            git clone "$repo_url" "$REPO_DIR/$repo_name" || echo "WARN: failed to clone $repo_name"
        else
            echo "Repo $repo_name already present, fetching updates ..."
            (cd "$REPO_DIR/$repo_name" && git fetch --all) || echo "WARN: failed to fetch $repo_name"
        fi
        if [ -d "$REPO_DIR/$repo_name/.git" ] && ! grep -q "name = \"$repo_name\"" "$CONFIG_DIR/config.toml"; then
            echo "Registering $repo_name with gitforest ..."
            gitforest --config "$CONFIG_DIR/config.toml" add "$REPO_DIR/$repo_name" --name "$repo_name" --description "Hosted seed repo" || echo "WARN: failed to register $repo_name"
        fi
    done
}

process_repo_list

if [ "$MODE" = "daemon" ]; then
    echo "Starting gitforest daemon (role=$ROLE, p2p=$P2P_PORT, api=$API_PORT)"
    unset GITFOREST_SKIP_AUTH
    exec gitforest --config "$CONFIG_DIR/config.toml" daemon
else
    exec "$@"
fi
