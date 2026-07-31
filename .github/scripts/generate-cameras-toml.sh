#!/usr/bin/env bash
# Generate the live cameras.toml on the runner from three sources, none of
# which lives in the repo:
#   server_url  — the deployed server's Lightsail HTTPS URL, read from the
#                 `hamama` CloudFormation stack output `ServerUrl` (the
#                 AWS::Lightsail::Container's Url attribute)
#   auth_token  — the AUTH_TOKEN secret (shared with the scraper/server)
#   the rest     — the CAMERAS_CONFIG_TOML secret (interval + [[cameras]] defs)
#
# Top-level keys must precede the [[cameras]] array-of-tables in TOML, so the
# two injected keys are prepended. Secrets arrive via env vars (not argv) so
# values with shell metacharacters don't break parsing.
#
# Env: AUTH_TOKEN, CAMERAS_CONFIG_TOML, AWS_REGION (set by configure-aws-credentials).
# Writes: cameras.toml in the cwd.
set -euo pipefail

server_url="$(aws cloudformation describe-stacks --stack-name hamama \
  --query "Stacks[0].Outputs[?OutputKey=='ServerUrl'].OutputValue" \
  --output text)"
# The uploader appends "/cameras/images", so trim the trailing slash the
# Lightsail URL ships with.
server_url="${server_url%/}"

{
  printf 'server_url = "%s"\n' "$server_url"
  printf 'auth_token = "%s"\n' "$AUTH_TOKEN"
  printf '\n'
  printf '%s\n' "$CAMERAS_CONFIG_TOML"
} > cameras.toml

# Sanity: refuse to deploy a config missing the injected keys — or whose
# server_url is empty (e.g. the stack is mid-ROLLBACK and the ServerUrl output
# is missing, so describe-stacks returned nothing). An empty server_url would
# ship a dead endpoint to the Pi and only fail at runtime.
grep -Eq '^server_url = ".+"' cameras.toml
grep -Eq '^auth_token = ".+"' cameras.toml

echo "--- generated cameras.toml (secrets redacted) ---"
sed -E 's/(auth_token = ").*"/\1***"/' cameras.toml
