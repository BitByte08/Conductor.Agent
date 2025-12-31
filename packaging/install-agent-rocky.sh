#!/usr/bin/env bash
set -euo pipefail

# Self-contained installer for Rocky Linux (and similar RHEL-compatible distros)
# Usage (run as root):
#   sudo ./install-agent-rocky.sh <agent-id> [/path/to/repo]
# Example:
#   sudo ./install-agent-rocky.sh my-agent /home/developer/Conductor

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

# Basic packages
echo "Installing system packages..."
dnf install -y gcc make openssl-devel pkgconfig git curl which

# NOTE: we no longer create an OS user for the service here; operators may choose to run the service under a dedicated user separately.

# Ensure Rust toolchain (rustup + cargo)
if ! command -v cargo >/dev/null 2>&1; then
  echo "Installing Rust toolchain (rustup)..."
  # Install rustup non-interactively for the current effective user (typically root when run via sudo)
  curl https://sh.rustup.rs -sSf | sh -s -- -y
  # Add cargo to PATH for current script execution (rustup installs to $HOME/.cargo/bin)
  export PATH="$HOME/.cargo/bin:$PATH"
fi

# Build the agent
AGENT_DIR="$REPO_PATH/agent"
if [[ ! -d "$AGENT_DIR" ]]; then
  echo "Agent source not found at $AGENT_DIR"
  exit 1
fi

echo "Building agent (release)..."
cd "$AGENT_DIR"
if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found in PATH after rustup installation"
  exit 1
fi
cargo build --release

# Install binary
echo "Installing binary to /usr/local/bin"
install -m 755 target/release/conductor-agent /usr/local/bin/conductor-agent

# Prepare data dir for this agent
DATA_DIR="/var/lib/conductor/${AGENT_ID}"
mkdir -p "$DATA_DIR"
# Set ownership to the current user (caller). Using $USER as requested.
chown -R "$USER":"$USER" /var/lib/conductor

# Create a simple config file for this agent if missing
CONFIG_FILE="$DATA_DIR/conductor_config.json"
if [[ ! -f "$CONFIG_FILE" ]]; then
  cat > "$CONFIG_FILE" <<EOF
{
  "ram_mb": "4G",
  "agent_id": "${AGENT_ID}",
  "backend_url": "wss://conductor.bitworkspace.kr"
}
EOF
  chown "$USER":"$USER" "$CONFIG_FILE"
fi

# Install systemd unit (copy from script directory so script is self-contained)
SERVICE_SRC="$SCRIPT_DIR/conductor-agent@.service"
if [[ ! -f "$SERVICE_SRC" ]]; then
  echo "Service template not found at $SERVICE_SRC"
  exit 1
fi

cp "$SERVICE_SRC" /etc/systemd/system/conductor-agent@.service
chmod 644 /etc/systemd/system/conductor-agent@.service

# Reload systemd, enable and start
systemctl daemon-reload
systemctl enable --now "conductor-agent@${AGENT_ID}"

echo "Service enabled and started: conductor-agent@${AGENT_ID}"

echo "You may follow logs with: journalctl -u conductor-agent@${AGENT_ID} -f"

echo "Note: Make the install script executable with: chmod +x $SCRIPT_DIR/install-agent-rocky.sh if needed"
