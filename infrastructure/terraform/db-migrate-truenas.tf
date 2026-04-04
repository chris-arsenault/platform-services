# =============================================================================
# TrueNAS Database Migration Lambda
#
# Creates databases and app roles on TrueNAS PostgreSQL (192.168.66.3:5432).
# Same pattern as db-migrate for shared RDS, but network-isolated to
# TrueNAS Postgres only.
# =============================================================================

variable "truenas_db_projects" {
  description = "Registered TrueNAS database projects"
  type        = map(object({ db_name = string }))
  default = {
    sonarqube = { db_name = "sonarqube" }
  }
}

data "archive_file" "db_migrate_truenas" {
  type        = "zip"
  source_file = "${path.module}/../../apps/target/lambda/db-migrate-truenas/bootstrap"
  output_path = "${path.module}/db-migrate-truenas-lambda.zip"
}

resource "aws_iam_role" "db_migrate_truenas" {
  name               = "platform-db-migrate-truenas"
  assume_role_policy = data.aws_iam_policy_document.auth_trigger_assume.json
}

resource "aws_iam_role_policy_attachment" "db_migrate_truenas_basic" {
  role       = aws_iam_role.db_migrate_truenas.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy_attachment" "db_migrate_truenas_vpc" {
  role       = aws_iam_role.db_migrate_truenas.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole"
}

resource "aws_iam_role_policy" "db_migrate_truenas_ssm" {
  name = "platform-db-migrate-truenas-ssm"
  role = aws_iam_role.db_migrate_truenas.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["ssm:GetParameter"]
        Resource = ["arn:aws:ssm:${data.aws_region.current.id}:${data.aws_caller_identity.current.account_id}:parameter/platform/truenas/*"]
      },
      {
        Effect   = "Allow"
        Action   = ["ssm:PutParameter", "ssm:GetParameter"]
        Resource = ["arn:aws:ssm:${data.aws_region.current.id}:${data.aws_caller_identity.current.account_id}:parameter/platform/truenas-db/*"]
      }
    ]
  })
}

resource "aws_lambda_function" "db_migrate_truenas" {
  function_name = "platform-db-migrate-truenas"
  role          = aws_iam_role.db_migrate_truenas.arn
  handler       = "bootstrap"
  runtime       = "provided.al2023"

  filename         = data.archive_file.db_migrate_truenas.output_path
  source_code_hash = data.archive_file.db_migrate_truenas.output_base64sha256

  timeout     = 120
  memory_size = 128

  vpc_config {
    subnet_ids         = data.aws_subnets.private.ids
    security_group_ids = [data.aws_security_group.platform_lambda.id]
  }

  environment {
    variables = {
      PG_HOST     = "192.168.66.3"
      PG_PORT     = "5432"
      PROJECT_MAP = jsonencode({ for k, v in var.truenas_db_projects : k => v })
    }
  }
}

# SSM outputs
resource "aws_ssm_parameter" "db_migrate_truenas_function" {
  name  = "${local.ssm_prefix}/truenas-db/function-name"
  type  = "String"
  value = aws_lambda_function.db_migrate_truenas.function_name
}
