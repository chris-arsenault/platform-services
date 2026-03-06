locals {
  default_auth_flows = [
    "ALLOW_USER_PASSWORD_AUTH",
    "ALLOW_REFRESH_TOKEN_AUTH",
    "ALLOW_USER_SRP_AUTH"
  ]
}

data "aws_route53_zone" "primary" {
  name         = "${var.domain_zone_name}."
  private_zone = false
}

resource "aws_cognito_user_pool" "pool" {
  name = var.user_pool_name

  auto_verified_attributes = ["email"]
  alias_attributes         = ["email"]

  password_policy {
    minimum_length    = 8
    require_lowercase = true
    require_numbers   = true
    require_symbols   = false
    require_uppercase = true
  }

  dynamic "lambda_config" {
    for_each = var.pre_auth_lambda_arn != "" ? [1] : []
    content {
      pre_authentication = var.pre_auth_lambda_arn
    }
  }
}

resource "aws_acm_certificate" "domain" {
  domain_name       = var.domain_name
  validation_method = "DNS"
}

resource "aws_route53_record" "cert_validation" {
  for_each = toset([var.domain_name])

  zone_id = data.aws_route53_zone.primary.zone_id
  name = one([
    for dvo in aws_acm_certificate.domain.domain_validation_options : dvo.resource_record_name
    if dvo.domain_name == each.value
  ])
  type = one([
    for dvo in aws_acm_certificate.domain.domain_validation_options : dvo.resource_record_type
    if dvo.domain_name == each.value
  ])
  ttl = 300
  records = [one([
    for dvo in aws_acm_certificate.domain.domain_validation_options : dvo.resource_record_value
    if dvo.domain_name == each.value
  ])]
}

resource "aws_acm_certificate_validation" "domain" {
  certificate_arn         = aws_acm_certificate.domain.arn
  validation_record_fqdns = [for record in aws_route53_record.cert_validation : record.fqdn]
}

resource "aws_cognito_user_pool_domain" "domain" {
  domain          = var.domain_name
  user_pool_id    = aws_cognito_user_pool.pool.id
  certificate_arn = aws_acm_certificate.domain.arn

  depends_on = [aws_acm_certificate_validation.domain]
}

resource "aws_route53_record" "domain" {
  zone_id = data.aws_route53_zone.primary.zone_id
  name    = var.domain_name
  type    = "CNAME"
  ttl     = 300
  records = [aws_cognito_user_pool_domain.domain.cloudfront_distribution]
}

resource "aws_cognito_user_pool_client" "clients" {
  for_each     = var.clients
  name         = each.value
  user_pool_id = aws_cognito_user_pool.pool.id

  generate_secret     = false
  explicit_auth_flows = local.default_auth_flows

  supported_identity_providers = ["COGNITO"]
}
