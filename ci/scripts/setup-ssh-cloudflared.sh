#!/bin/bash
set -euxo pipefail

SSH_PORT="${SSH_PORT:-2222}"
WORK_DIR="$(pwd)"
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$ARCH" in
  x86_64) ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
esac

if [[ "$OS" == *"mingw"* ]] || [[ "$OS" == *"msys"* ]]; then
  PLATFORM="windows"
  CLOUDFLARED_BIN="cloudflared.exe"
  curl -fsSL "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-windows-$ARCH.exe" -o "$CLOUDFLARED_BIN"
elif [ "$OS" = "darwin" ]; then
  PLATFORM="macos"
  CLOUDFLARED_BIN="cloudflared"
  curl -fsSL "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-darwin-$ARCH.tgz" | tar xz
else
  PLATFORM="linux"
  CLOUDFLARED_BIN="cloudflared"
  curl -fsSL "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-$ARCH" -o "$CLOUDFLARED_BIN"
fi
chmod +x "$CLOUDFLARED_BIN"

AUTHORIZED_KEYS=$(curl -sf "https://github.com/$GITHUB_ACTOR.keys")
if [ -z "$AUTHORIZED_KEYS" ]; then
  echo "ERROR: No SSH keys found for $GITHUB_ACTOR"
  echo "Add keys at: https://github.com/settings/keys"
  exit 1
fi

if [ "$PLATFORM" = "windows" ]; then
  ADMIN_KEYS_FILE_BASH="/c/ProgramData/ssh/administrators_authorized_keys"
  ADMIN_KEYS_FILE_WIN="C:/ProgramData/ssh/administrators_authorized_keys"
  ADMIN_KEYS_FILE_ICACLS="C:\ProgramData\ssh\administrators_authorized_keys"

  echo "$AUTHORIZED_KEYS" > "$ADMIN_KEYS_FILE_BASH"
  icacls.exe "$ADMIN_KEYS_FILE_ICACLS" //inheritance:r //grant "SYSTEM:(F)" //grant "Administrators:(F)"

  SSHD_CONFIG="/c/ProgramData/ssh/sshd_config"
  [ -f "$SSHD_CONFIG" ] && cp "$SSHD_CONFIG" "${SSHD_CONFIG}.backup"

  cat > "$SSHD_CONFIG" <<EOF
Port $SSH_PORT
PubkeyAuthentication yes
PasswordAuthentication no
AuthorizedKeysFile $ADMIN_KEYS_FILE_WIN
Subsystem sftp sftp-server.exe
EOF

  powershell.exe -Command "Restart-Service sshd"

  USERNAME="runneradmin"
else
  mkdir -p "$WORK_DIR/.ssh"
  echo "$AUTHORIZED_KEYS" > "$WORK_DIR/.ssh/authorized_keys"
  chmod 700 "$WORK_DIR/.ssh"
  chmod 600 "$WORK_DIR/.ssh/authorized_keys"

  ssh-keygen -q -t rsa -b 4096 -f "$WORK_DIR/ssh_host_rsa_key" -N ''
  chmod 600 "$WORK_DIR/ssh_host_rsa_key"

  cat > "$WORK_DIR/sshd_config" <<EOF
Port $SSH_PORT
HostKey $WORK_DIR/ssh_host_rsa_key
AuthorizedKeysFile $WORK_DIR/.ssh/authorized_keys
PubkeyAuthentication yes
PasswordAuthentication no
StrictModes no
UsePAM no
PidFile $WORK_DIR/sshd.pid
EOF

  /usr/sbin/sshd -f "$WORK_DIR/sshd_config" -E "$WORK_DIR/sshd.log"

  sleep 2
  if [ ! -f "$WORK_DIR/sshd.pid" ] || ! kill -0 "$(cat "$WORK_DIR/sshd.pid")" 2>/dev/null; then
    echo "ERROR: sshd failed to start"
    cat "$WORK_DIR/sshd.log"
    exit 1
  fi

  USERNAME="runner"
fi

./"$CLOUDFLARED_BIN" tunnel --no-autoupdate --url "tcp://localhost:$SSH_PORT" > cloudflared.log 2>&1 &
CLOUDFLARED_PID=$!

TUNNEL_URL=""
for _ in $(seq 1 30); do
  TUNNEL_URL=$(grep -oE 'https://[a-z0-9-]+\.trycloudflare\.com' cloudflared.log 2>/dev/null | head -1 || true)
  [ -n "$TUNNEL_URL" ] && break
  sleep 1
done

if [ -z "$TUNNEL_URL" ]; then
  echo "ERROR: Failed to get tunnel URL"
  cat cloudflared.log
  exit 1
fi

echo ""
echo "╔════════════════════════════════════════════════════════════╗"
echo "║                   SSH CONNECTION READY                     ║"
echo "╚════════════════════════════════════════════════════════════╝"
echo ""
echo "ssh -o ProxyCommand='cloudflared access tcp --hostname $TUNNEL_URL' $USERNAME@action"
echo ""

cleanup() {
  kill "$CLOUDFLARED_PID" 2>/dev/null || true
  if [ "$PLATFORM" = "windows" ]; then
    powershell.exe -Command "Stop-Service sshd" || true
  elif [ -f "$WORK_DIR/sshd.pid" ]; then
    kill "$(cat "$WORK_DIR/sshd.pid")" 2>/dev/null || true
  fi
}

trap cleanup EXIT INT TERM

check_connections() {
  if [ "$PLATFORM" = "windows" ]; then
    netstat -an | grep -q ":$SSH_PORT.*ESTABLISHED"
  elif [ "$PLATFORM" = "macos" ]; then
    netstat -anp tcp | grep -q "\.$SSH_PORT.*ESTABLISHED"
  else
    netstat -tn | grep -q ":$SSH_PORT.*ESTABLISHED"
  fi
}

HAD_CONNECTION=false

while kill -0 "$CLOUDFLARED_PID" 2>/dev/null; do
  if check_connections; then
    [ "$HAD_CONNECTION" = false ] && HAD_CONNECTION=true
  elif [ "$HAD_CONNECTION" = true ]; then
    exit 0
  fi
  sleep 2
done
