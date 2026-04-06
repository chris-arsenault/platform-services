# =============================================================================
# Cognito User Pool (shared across all platform apps)
#
# DEPENDENCY: The Cognito custom domain (auth.ahara.io) requires that the
# apex domain (ahara.io) has an A record in Route53. This is an undocumented
# AWS prerequisite. The record is currently managed by the websites project
# (ahara.io SPA). If that site is ever removed, a placeholder A record must
# be created to keep the Cognito domain functional.
# =============================================================================

module "cognito" {
  source = "./modules/cognito"

  user_pool_name      = var.cognito_user_pool_name
  domain_name         = local.auth_domain
  domain_zone_name    = var.domain_name
  clients             = var.cognito_clients
  pre_auth_lambda_arn = module.auth_trigger.function_arn
}

# =============================================================================
# User Access Table (gates per-app access via pre-auth Lambda)
# =============================================================================

resource "aws_dynamodb_table" "user_access" {
  name         = local.user_access_table_name
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "username"

  attribute {
    name = "username"
    type = "S"
  }
}


# =============================================================================
# ALB Cognito Client (for authenticate-cognito action on dashboard routes)
# =============================================================================

resource "aws_cognito_user_pool_client" "alb" {
  name         = "platform-alb"
  user_pool_id = module.cognito.user_pool_id

  generate_secret                      = true
  allowed_oauth_flows                  = ["code"]
  allowed_oauth_scopes                 = ["openid", "email", "profile"]
  allowed_oauth_flows_user_pool_client = true
  supported_identity_providers         = ["COGNITO"]

  callback_urls = [
    "https://dashboards.ahara.io/oauth2/idpresponse"
  ]

  logout_urls = [
    "https://dashboards.ahara.io/logout"
  ]
}

