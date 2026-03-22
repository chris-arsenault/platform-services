#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TF_DIR="${ROOT_DIR}/infrastructure/terraform"

STATE_BUCKET="${STATE_BUCKET:-tfstate-559098897826}"
STATE_REGION="${STATE_REGION:-us-east-1}"
USE_LOCKFILE="${USE_LOCKFILE:-true}"

tf() {
  terraform -chdir="${TF_DIR}" "$@"
}

echo "Building auth-trigger Lambda..."
AUTH_DIR="${ROOT_DIR}/apps/auth-trigger"
(cd "${AUTH_DIR}" && npm ci && npm run build)

echo "Initializing Terraform backend..."
tf init \
  -backend-config="bucket=${STATE_BUCKET}" \
  -backend-config="region=${STATE_REGION}" \
  -backend-config="use_lockfile=${USE_LOCKFILE}"

echo "Applying Terraform..."
tf apply -auto-approve

echo "Platform services deployed."
echo "Cognito user pool ID:"
tf output -raw cognito_user_pool_id 2>/dev/null || echo "(not available)"
