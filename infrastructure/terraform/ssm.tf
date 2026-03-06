# =============================================================================
# SSM Parameters — Cross-project service discovery bus
#
# All platform-level resource IDs are published here so consuming projects
# can read them with `data "aws_ssm_parameter"` instead of using
# terraform_remote_state coupling.
# =============================================================================

# --- Cognito ---

resource "aws_ssm_parameter" "cognito_user_pool_id" {
  name  = "${local.ssm_prefix}/cognito/user-pool-id"
  type  = "String"
  value = module.cognito.user_pool_id
}

resource "aws_ssm_parameter" "cognito_user_pool_arn" {
  name  = "${local.ssm_prefix}/cognito/user-pool-arn"
  type  = "String"
  value = module.cognito.user_pool_arn
}

resource "aws_ssm_parameter" "cognito_domain" {
  name  = "${local.ssm_prefix}/cognito/domain"
  type  = "String"
  value = module.cognito.domain_name
}

# Per-app client IDs
resource "aws_ssm_parameter" "cognito_client_ids" {
  for_each = var.cognito_clients

  name  = "${local.ssm_prefix}/cognito/clients/${each.key}"
  type  = "String"
  value = module.cognito.client_ids[each.key]
}

# SonarQube client (separate because it has a secret)
resource "aws_ssm_parameter" "sonarqube_cognito_client_id" {
  name  = "${local.ssm_prefix}/sonarqube/cognito-client-id"
  type  = "String"
  value = aws_cognito_user_pool_client.sonarqube.id
}

resource "aws_ssm_parameter" "sonarqube_cognito_client_secret" {
  name  = "${local.ssm_prefix}/sonarqube/cognito-client-secret"
  type  = "SecureString"
  value = aws_cognito_user_pool_client.sonarqube.client_secret
}

# --- SonarQube CI token (will be populated after SonarQube is running) ---

resource "aws_ssm_parameter" "sonarqube_url" {
  name  = "${local.ssm_prefix}/sonarqube/url"
  type  = "String"
  value = "https://${local.sonarqube_domain}"
}

resource "aws_ssm_parameter" "sonarqube_ci_token" {
  name  = "${local.ssm_prefix}/sonarqube/ci-token"
  type  = "SecureString"
  value = "PLACEHOLDER"

  lifecycle {
    ignore_changes = [value]
  }
}
