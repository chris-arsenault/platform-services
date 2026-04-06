# =============================================================================
# CI Ingest Lambda — receives build reports, stores in shared RDS
# =============================================================================

resource "random_password" "ci_ingest_token" {
  length  = 32
  special = false
}

module "ci_ingest" {
  source   = "git::https://github.com/chris-arsenault/ahara-tf-patterns.git//modules/alb-api"
  hostname = "ci.ahara.io"

  environment = {
    DB_HOST       = aws_db_instance.platform.address
    DB_PORT       = tostring(aws_db_instance.platform.port)
    DB_NAME       = aws_db_instance.platform.db_name
    DB_SSM_PREFIX = "/platform/db/platform"
    INGEST_TOKEN  = random_password.ci_ingest_token.result
  }

  iam_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["ssm:GetParameter"]
        Resource = ["arn:aws:ssm:${data.aws_region.current.id}:${data.aws_caller_identity.current.account_id}:parameter/platform/db/platform/*"]
      }
    ]
  })

  lambdas = {
    ingest = {
      zip    = "${path.module}/../../backend/target/lambda/ci-ingest/bootstrap.zip"
      routes = [{ priority = 150, paths = ["/*"], authenticated = false }]
    }
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
