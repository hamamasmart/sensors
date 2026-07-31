#!/usr/bin/env bash
# Run `sam deploy` for the hamama stack, forwarding the shared stack parameters
# from the environment. Both the bootstrap step and the real deploy step of
# .github/workflows/deploy.yml call this so the sam-deploy invocation lives in
# exactly one place.
#
#   $1 — ServerImageUri. Empty on the bootstrap deploy (so the parameter is
#        omitted entirely and CloudFormation applies its Default "", gating the
#        deployment out — and on an update keeps the previous value via
#        UsePreviousValue, so a flaky bootstrap re-run can't clobber a live
#        deployment). The real image URI on every later deploy.
#
# Secrets arrive via env vars (DATABASE_URL, AUTH_TOKEN, PHYTECH_EMAIL,
# PHYTECH_PASSWORD) rather than argv so values with shell metacharacters don't
# break sam's parameter parsing. Also reads AWS_REGION and SAM_BUCKET from the
# environment (set by configure-aws-credentials and ensure-sam-bucket.sh).
set -euo pipefail

image_uri="${1:-}"

# Omit ServerImageUri entirely when empty (see header) rather than passing an
# empty `ServerImageUri=`, which would reset the parameter on update.
if [ -n "$image_uri" ]; then
  image_override="ServerImageUri=${image_uri}"
else
  image_override=""
fi

sam deploy \
  --template-file template.yaml \
  --stack-name hamama \
  --region "${AWS_REGION}" \
  --capabilities CAPABILITY_NAMED_IAM \
  --s3-bucket "${SAM_BUCKET}" \
  --s3-prefix hamama \
  --no-confirm-changeset \
  --no-disable-rollback \
  --no-fail-on-empty-changeset \
  --parameter-overrides \
    ${image_override} \
    DatabaseUrl="${DATABASE_URL}" \
    AuthToken="${AUTH_TOKEN}" \
    PhytechEmail="${PHYTECH_EMAIL}" \
    PhytechPassword="${PHYTECH_PASSWORD}"
