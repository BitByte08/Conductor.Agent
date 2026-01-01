#!/usr/bin/env bash
set -euo pipefail

# Agent Update Script
# Usage (run as root):
#   sudo ./update-agent.sh <agent-id> [/path/to/repo]
# Example:
#   sudo ./update-agent.sh my-agent /home/developer/conductor

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENT_ID=${1:-}
REPO_PATH=${2:-$(cd "$SCRIPT_DIR/../.." && pwd)}

if [[ -z "$AGENT_ID" ]]; then
  echo "Usage: $0 <agent-id> [/path/to/repo]"
  exit 1
fi

# Ensure running as root
if [[ $(id -u) -ne 0 ]]; then
  echo "This script must be run as root"
  exit 1
fi

AGENT_DIR="$REPO_PATH/agent"
if [[ ! -d "$AGENT_DIR" ]]; then
  echo "Agent source not found at $AGENT_DIR"
  exit 1
fi

echo "Updating agent code from repository..."
cd "$REPO_PATH"
git fetch origin
git pull origin main || git pull origin master || echo "Warning: Could not pull latest code"

echo "Stopping agent service..."
systemctl stop "conductor-agent@${AGENT_ID}" || echo "Service was not running"

echo "Building updated agent (release)..."
cd "$AGENT_DIR"
cargo clean
cargo build --release

echo "Installing updated binary to /usr/local/bin"
install -m 755 target/release/conductor-agent /usr/local/bin/conductor-agent

echo "Starting agent service..."
systemctl start "conductor-agent@${AGENT_ID}"

echo "Update complete! Check status with: systemctl status conductor-agent@${AGENT_ID}"
echo "Follow logs with: journalctl -u conductor-agent@${AGENT_ID} -f"
