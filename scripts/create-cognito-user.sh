#!/usr/bin/env bash
set -euo pipefail

USERNAME="martin"
PASSWORD="$1"

if [[ -z "${PASSWORD:-}" ]]; then
  echo "Usage: $0 <password>" >&2
  echo "Password must be >=8 chars, with uppercase, lowercase, and number." >&2
  exit 1
fi

USER_POOL_ID=$(aws ssm get-parameter \
  --name /platform/cognito/user-pool-id \
  --query 'Parameter.Value' \
  --output text)

echo "Creating user '${USERNAME}' in pool ${USER_POOL_ID}..."

aws cognito-idp admin-create-user \
  --user-pool-id "$USER_POOL_ID" \
  --username "$USERNAME" \
  --temporary-password "$PASSWORD" \
  --message-action SUPPRESS

echo "Setting permanent password (skips FORCE_CHANGE_PASSWORD)..."

aws cognito-idp admin-set-user-password \
  --user-pool-id "$USER_POOL_ID" \
  --username "$USERNAME" \
  --password "$PASSWORD" \
  --permanent

echo "Done. User '${USERNAME}' is CONFIRMED with no password change required."
