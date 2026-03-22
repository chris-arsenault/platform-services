# =============================================================================
# CI Ingest Lambda — receives build reports, stores in shared RDS
# =============================================================================

data "archive_file" "ci_ingest" {
  type        = "zip"
  source_file = "${path.module}/../../apps/ci-ingest/dist/handler.js"
  output_path = "${path.module}/ci-ingest-lambda.zip"
}

resource "aws_iam_role" "ci_ingest" {
  name               = "platform-ci-ingest"
  assume_role_policy = data.aws_iam_policy_document.auth_trigger_assume.json
}

resource "aws_iam_role_policy_attachment" "ci_ingest_basic" {
  role       = aws_iam_role.ci_ingest.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy_attachment" "ci_ingest_vpc" {
  role       = aws_iam_role.ci_ingest.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole"
}

resource "aws_security_group" "ci_ingest" {
  name        = "platform-ci-ingest"
  description = "CI ingest Lambda"
  vpc_id      = nonsensitive(data.aws_ssm_parameter.vpc_id.value)

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "random_password" "ci_ingest_db" {
  length  = 24
  special = false
}

resource "random_password" "ci_ingest_token" {
  length  = 32
  special = false
}

resource "aws_lambda_function" "ci_ingest" {
  function_name = "platform-ci-ingest"
  role          = aws_iam_role.ci_ingest.arn
  handler       = "handler.handler"
  runtime       = "nodejs24.x"

  filename         = data.archive_file.ci_ingest.output_path
  source_code_hash = data.archive_file.ci_ingest.output_base64sha256

  timeout     = 10
  memory_size = 128

  vpc_config {
    subnet_ids         = split(",", nonsensitive(data.aws_ssm_parameter.private_subnet_ids.value))
    security_group_ids = [aws_security_group.ci_ingest.id]
  }

  environment {
    variables = {
      DB_HOST      = aws_db_instance.platform.address
      DB_PORT      = tostring(aws_db_instance.platform.port)
      DB_USER      = "ci_ingest"
      DB_PASSWORD  = random_password.ci_ingest_db.result
      DB_NAME      = "platform"
      INGEST_TOKEN = random_password.ci_ingest_token.result
    }
  }
}

# --- ALB integration ---

resource "aws_lb_target_group" "ci_ingest" {
  name        = "platform-ci-tg"
  target_type = "lambda"
}

resource "aws_lb_target_group_attachment" "ci_ingest" {
  target_group_arn = aws_lb_target_group.ci_ingest.arn
  target_id        = aws_lambda_function.ci_ingest.arn
  depends_on       = [aws_lambda_permission.ci_ingest_alb]
}

resource "aws_lambda_permission" "ci_ingest_alb" {
  statement_id  = "AllowALBInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.ci_ingest.function_name
  principal     = "elasticloadbalancing.amazonaws.com"
  source_arn    = aws_lb_target_group.ci_ingest.arn
}

resource "aws_lb_listener_rule" "ci_ingest" {
  listener_arn = nonsensitive(data.aws_ssm_parameter.alb_listener_arn.value)
  priority     = 150

  condition {
    host_header {
      values = ["ci.ahara.io"]
    }
  }

  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.ci_ingest.arn
  }
}

# --- TLS cert ---

resource "aws_acm_certificate" "ci_ingest" {
  domain_name       = "ci.ahara.io"
  validation_method = "DNS"
}

resource "aws_route53_record" "ci_ingest_cert_validation" {
  for_each = {
    for dvo in aws_acm_certificate.ci_ingest.domain_validation_options :
    dvo.domain_name => {
      name  = dvo.resource_record_name
      type  = dvo.resource_record_type
      value = dvo.resource_record_value
    }
  }

  zone_id = data.aws_route53_zone.ahara.zone_id
  name    = each.value.name
  type    = each.value.type
  ttl     = 60
  records = [each.value.value]
}

resource "aws_acm_certificate_validation" "ci_ingest" {
  certificate_arn         = aws_acm_certificate.ci_ingest.arn
  validation_record_fqdns = [for r in aws_route53_record.ci_ingest_cert_validation : r.fqdn]
}

resource "aws_lb_listener_certificate" "ci_ingest" {
  listener_arn    = nonsensitive(data.aws_ssm_parameter.alb_listener_arn.value)
  certificate_arn = aws_acm_certificate_validation.ci_ingest.certificate_arn
}

# --- DNS ---

resource "aws_route53_record" "ci_ingest" {
  zone_id = data.aws_route53_zone.ahara.zone_id
  name    = "ci.ahara.io"
  type    = "A"

  alias {
    name                   = nonsensitive(data.aws_ssm_parameter.alb_dns_name.value)
    zone_id                = nonsensitive(data.aws_ssm_parameter.alb_zone_id.value)
    evaluate_target_health = true
  }
}

# --- SSM outputs ---

resource "aws_ssm_parameter" "ci_ingest_url" {
  name  = "${local.ssm_prefix}/ci/url"
  type  = "String"
  value = "https://ci.ahara.io"
}

resource "aws_ssm_parameter" "ci_ingest_token" {
  name  = "${local.ssm_prefix}/ci/ingest-token"
  type  = "SecureString"
  value = random_password.ci_ingest_token.result
}
