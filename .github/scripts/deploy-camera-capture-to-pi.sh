#!/usr/bin/env bash
# Deploy the built binary, systemd unit, and generated config to the
# Raspberry Pi over Tailscale, then restart the service.
#
# Prereqs: the runner has joined the tailnet (tailscale/github-action), the
# deploy SSH key is staged (stage-deploy-ssh-key.sh), and cameras.toml was
# generated in the cwd (generate-cameras-toml.sh).
#
# The Pi is reached at its Tailscale IP. The pi user has NOPASSWD sudo, so
# the install/restart runs passwordless. is-active exits non-zero if the
# service didn't come up → the step fails.
set -euo pipefail

PI=pi@100.64.132.105
BIN=target/aarch64-unknown-linux-gnu/release/camera-capture
UNIT=crates/camera-capture/camera-capture.service
CFG=cameras.toml

scp -o StrictHostKeyChecking=accept-new "$BIN" "$UNIT" "$CFG" "$PI":/tmp/

ssh -o StrictHostKeyChecking=accept-new "$PI" 'sudo -n bash -s' <<'REMOTE'
  set -e
  install -m755  /tmp/camera-capture          /opt/camera-capture/camera-capture
  install -m644  /tmp/camera-capture.service  /etc/systemd/system/camera-capture.service
  install -m600  /tmp/cameras.toml            /opt/camera-capture/cameras.toml
  systemctl daemon-reload
  systemctl restart camera-capture
  systemctl is-active camera-capture
REMOTE

echo "--- post-deploy journal (last 8 lines) ---"
ssh -o StrictHostKeyChecking=accept-new "$PI" 'journalctl -u camera-capture -n 8 --no-pager'
