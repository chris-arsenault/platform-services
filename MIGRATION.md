# Migration Guide: Extracting Shared Services from Websites

This document describes the steps to migrate Cognito, auth-trigger, and
user-access resources from the `websites` Terraform state to the new
`platform-services` state.

## Prerequisites

- `platform-control` has been applied (creates the `deployer-platform` role,
  state bucket, and injects GitHub secrets into `platform-services` repo)
- The `platform-services` auth-trigger Lambda has been built (`npm ci && npm run build`
  in `apps/auth-trigger/`)

## Overview

The existing resources live in the `websites` state file
(`ahara-static-websites.tfstate` in `tf-state-websites-559098897826`).
We need to import them into the `platform-services` state file
(`platform-services.tfstate` in the platform state bucket), then remove
them from the `websites` state.

The Cognito user pool MUST NOT be destroyed and recreated — doing so would
invalidate all existing user sessions and change the pool ID embedded in
every consuming application.

## Step 1: Apply platform-control first

```bash
cd ~/src/platform-control/infrastructure/terraform
terraform init -backend-config bucket=$STATE_BUCKET -backend-config region=us-east-1
terraform plan -var github_pat=$PAT
terraform apply -var github_pat=$PAT
```

This creates:
- `deployer-platform` IAM role with platform-services permissions
- `tf-state-platform-559098897826` S3 bucket for platform-services state
- GitHub secrets (`OIDC_ROLE`, `STATE_BUCKET`, `PREFIX`) on the
  `platform-services` repo

## Step 2: Initialize platform-services state

```bash
cd ~/src/platform-services/infrastructure/terraform
terraform init -backend-config bucket=$STATE_BUCKET -backend-config region=us-east-1
```

## Step 3: Import existing resources

Get the current resource IDs from the websites state:

```bash
cd ~/src/websites/infrastructure/terraform
terraform init -backend-config bucket=tf-state-websites-559098897826 -backend-config region=us-east-1

# Get Cognito user pool ID
terraform output cognito_user_pool_id
# Get Cognito client IDs
terraform output cognito_client_ids
```

Then import into platform-services state. The resource addresses below
map old websites resources to new platform-services resources:

```bash
cd ~/src/platform-services/infrastructure/terraform

# Cognito user pool
terraform import 'module.cognito.aws_cognito_user_pool.pool' <user_pool_id>

# Cognito domain and cert (look up ARNs via AWS console or CLI)
terraform import 'module.cognito.aws_cognito_user_pool_domain.domain' 'auth.ahara.io'
terraform import 'module.cognito.aws_acm_certificate.domain' <cert_arn>
terraform import 'module.cognito.aws_acm_certificate_validation.domain' <cert_arn>

# Cognito clients (iterate over each app key)
terraform import 'module.cognito.aws_cognito_user_pool_client.clients["scorchbook"]' '<pool_id>/<client_id>'
terraform import 'module.cognito.aws_cognito_user_pool_client.clients["svap"]' '<pool_id>/<client_id>'
terraform import 'module.cognito.aws_cognito_user_pool_client.clients["canonry"]' '<pool_id>/<client_id>'
terraform import 'module.cognito.aws_cognito_user_pool_client.clients["ahara"]' '<pool_id>/<client_id>'

# SonarQube Cognito client
terraform import 'aws_cognito_user_pool_client.sonarqube' '<pool_id>/<sonarqube_client_id>'

# DynamoDB user access table
terraform import 'aws_dynamodb_table.user_access' 'platform-user-access'
# NOTE: The old table name was 'websites-user-access'. If you want to
# keep the same table, update locals.tf to use the old name. Otherwise,
# a new table will be created and the old one left in place.

# Cognito user
terraform import 'aws_cognito_user.chris' '<pool_id>/chris'

# Auth trigger Lambda
terraform import 'aws_lambda_function.auth_trigger' 'platform-auth-trigger'
# NOTE: Same issue — old name was 'websites-auth-trigger'. Either rename
# the Lambda or update the Terraform resource to match.

# Auth trigger IAM role
terraform import 'aws_iam_role.auth_trigger' 'platform-auth-trigger'

# Lambda permission
terraform import 'aws_lambda_permission.auth_trigger_cognito' 'platform-auth-trigger/AllowCognitoInvoke'

# DNS records (the cert validation record and Cognito domain CNAME)
terraform import 'module.cognito.aws_route53_record.cert_validation["auth.ahara.io"]' '<zone_id>_<record_name>_<record_type>'
terraform import 'module.cognito.aws_route53_record.domain' '<zone_id>_auth.ahara.io_CNAME'
```

## Step 4: Handle resource naming

The existing resources use `websites-` prefix (e.g., `websites-auth-trigger`,
`websites-user-access`). Platform-services uses `platform-` prefix. You have
two options:

**Option A (recommended): Keep old names, update Terraform to match**

Update `platform-services/infrastructure/terraform/locals.tf`:
```hcl
locals {
  user_access_table_name = "websites-user-access"  # keep old name
}
```

Update `auth-trigger.tf` to use `websites-auth-trigger` as the function name
and IAM role name. This avoids any resource recreation.

**Option B: Rename resources**

Let Terraform destroy the old-named resources and create new ones. This will
cause a brief outage of the auth-trigger Lambda. The Cognito pool itself
is unaffected since it's imported by ID.

## Step 5: Plan and verify

```bash
cd ~/src/platform-services/infrastructure/terraform
terraform plan
```

The plan should show:
- No changes to imported resources (if names match)
- New resources: SSM parameters, SNS topic, budget, cost anomaly detection
- The `random_password` for chris will need to be imported or will generate
  a new value (harmless — the Cognito user already exists)

## Step 6: Remove from websites state

After platform-services is managing the resources:

```bash
cd ~/src/websites/infrastructure/terraform

# Remove Cognito module
terraform state rm 'module.cognito'

# Remove user access table
terraform state rm 'module.user_access_table'
terraform state rm 'aws_dynamodb_table_item.seed_user'

# Remove auth trigger
terraform state rm 'data.archive_file.auth_trigger'
terraform state rm 'aws_iam_role.auth_trigger'
terraform state rm 'aws_iam_role_policy_attachment.auth_trigger_basic'
terraform state rm 'aws_iam_role_policy.auth_trigger'
terraform state rm 'aws_lambda_function.auth_trigger'
terraform state rm 'aws_lambda_permission.auth_trigger_cognito'
terraform state rm 'aws_ssm_parameter.auth_client_map'

# Remove SonarQube Cognito client (EC2 stays for now)
terraform state rm 'aws_cognito_user_pool_client.sonarqube'
terraform state rm 'aws_ssm_parameter.sonarqube_cognito_client_id'
terraform state rm 'aws_ssm_parameter.sonarqube_cognito_client_secret'

# Remove user
terraform state rm 'aws_cognito_user.chris'
terraform state rm 'random_password.cognito_chris'
```

## Step 7: Update websites Terraform

After state removal, delete or update the following files in
`websites/infrastructure/terraform/`:

- Delete `identity.tf` (Cognito module + user access table)
- Delete `auth-trigger.tf`
- Delete `users.tf`
- Update `locals.tf` to remove `cognito_*` and `user_access_table_name` locals
- Update `sonarqube.tf` to read Cognito pool ID from SSM instead of module output
- Update `canonry.tf` to read Cognito pool ID and client IDs from SSM
- Update `outputs.tf` to remove `cognito_*` outputs (or read from SSM)
- Update all site-*.tf files that reference `module.cognito.client_ids`

Replace direct module references with SSM data sources:

```hcl
data "aws_ssm_parameter" "cognito_pool_id" {
  name = "/platform/cognito/user-pool-id"
}

data "aws_ssm_parameter" "cognito_client_scorchbook" {
  name = "/platform/cognito/clients/scorchbook"
}
```

## Step 8: Update svap

Replace `terraform_remote_state` with SSM reads in
`svap/infrastructure/terraform/svap.tf`:

```hcl
# DELETE this block:
# data "terraform_remote_state" "websites" { ... }

# ADD these:
data "aws_ssm_parameter" "cognito_pool_id" {
  name = "/platform/cognito/user-pool-id"
}

data "aws_ssm_parameter" "cognito_client_svap" {
  name = "/platform/cognito/clients/svap"
}
```

Update `locals.tf`:
```hcl
locals {
  cognito_user_pool_id = data.aws_ssm_parameter.cognito_pool_id.value
  cognito_client_id    = data.aws_ssm_parameter.cognito_client_svap.value
  cognito_issuer       = "https://cognito-idp.us-east-1.amazonaws.com/${local.cognito_user_pool_id}"
}
```

## Step 9: Update CI workflows

Update the SonarQube CI token SSM path in:
- `websites/.github/workflows/lint.yml`
- `the-canonry/.github/workflows/lint.yml`

Change: `/websites/sonarqube/ci-token` → `/platform/sonarqube/ci-token`

## Rollback

If anything goes wrong during migration, the resources still exist in AWS.
You can re-import them into the websites state using `terraform import`.
No resources are destroyed during this migration process.
