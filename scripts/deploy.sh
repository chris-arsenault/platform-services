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

echo "Building Lambdas..."
(cd "${ROOT_DIR}/backend" && cargo lambda build --release)

echo "Running migrations..."
#db-migrate

echo "Initializing Terraform backend..."
tf init -reconfigure \
  -backend-config="bucket=${STATE_BUCKET}" \
  -backend-config="region=${STATE_REGION}" \
  -backend-config="use_lockfile=${USE_LOCKFILE}"

echo "Applying Terraform..."
tf apply -auto-approve

echo "Platform services deployed."
