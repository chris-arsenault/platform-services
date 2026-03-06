# =============================================================================
# Seed Users
# =============================================================================

resource "random_password" "cognito_chris" {
  length      = 16
  special     = false
  min_upper   = 1
  min_lower   = 1
  min_numeric = 1
}

resource "aws_cognito_user" "chris" {
  user_pool_id   = module.cognito.user_pool_id
  username       = "chris"
  password       = random_password.cognito_chris.result
  message_action = "SUPPRESS"

  attributes = {
    email          = var.seed_user_email
    email_verified = "true"
    name           = "chris"
  }
}
