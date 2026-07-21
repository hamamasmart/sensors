#!/usr/bin/env bash
# Idempotently ensures the SAM packaging bucket exists in the deploy region,
# with versioning enabled. The bucket name encodes account + region so it is
# globally unique and stable across deploys. Exposes the name to the workflow
# via $GITHUB_ENV (as SAM_BUCKET) for the subsequent `sam deploy` step.
set -euo pipefail


account=$(aws sts get-caller-identity --query Account --output text)
bucket="hamama-sam-artifacts-${account}-${AWS_REGION}"

echo "Artifacts bucket: ${bucket} (region ${AWS_REGION})"

if aws s3api head-bucket --bucket "${bucket}" --region "${AWS_REGION}" 2>/dev/null; then
  echo "Bucket already exists."
else
  echo "Bucket missing; creating it..."
  # us-east-1 rejects LocationConstraint; every other region requires it.
  if [ "${AWS_REGION}" = "us-east-1" ]; then
    aws s3api create-bucket --bucket "${bucket}" --region "${AWS_REGION}"
  else
    aws s3api create-bucket --bucket "${bucket}" --region "${AWS_REGION}" \
      --create-bucket-configuration "LocationConstraint=${AWS_REGION}"
  fi
  aws s3api put-bucket-versioning --bucket "${bucket}" --region "${AWS_REGION}" \
    --versioning-configuration Status=Enabled
fi

echo "SAM_BUCKET=${bucket}" >> "$GITHUB_ENV"
