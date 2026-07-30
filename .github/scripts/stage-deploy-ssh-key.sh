#!/usr/bin/env bash
# Stage the Tailscale-deploy SSH key on the ephemeral runner.
#
# Env: PI_SSH_KEY — the private key whose public half is installed in the pi
# user's authorized_keys on the Raspberry Pi (restricted to 100.64.0.0/10).
#
# The runner is ephemeral and has no persisted known_hosts, so we accept the
# Pi's host key on first connect.
#
# Env: PI_SSH_KEY — the private deploy key; PI_HOST — the Pi's Tailscale IP
# (repo variable), used to scope the ssh Host block.
set -euo pipefail

install -d -m700 ~/.ssh
printf '%s\n' "${PI_SSH_KEY}" > ~/.ssh/id_ed25519
chmod 600 ~/.ssh/id_ed25519

printf '%s\n' "Host ${PI_HOST}" \
  '  StrictHostKeyChecking accept-new' \
  '  UserKnownHostsFile ~/.ssh/known_hosts' > ~/.ssh/config
chmod 600 ~/.ssh/config
