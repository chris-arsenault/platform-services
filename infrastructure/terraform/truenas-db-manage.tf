# =============================================================================
# TrueNAS Database Management Lambda
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

data "archive_file" "truenas_db_manage" {
  type        = "zip"
  source_file = "${path.module}/../../apps/target/lambda/truenas-db-manage/bootstrap"
  output_path = "${path.module}/truenas-db-manage-lambda.zip"
}

resource "aws_iam_role" "truenas_db_manage" {
  name               = "platform-truenas-db-manage"
  assume_role_policy = data.aws_iam_policy_document.auth_trigger_assume.json
}

resource "aws_iam_role_policy_attachment" "truenas_db_manage_basic" {
  role       = aws_iam_role.truenas_db_manage.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy_attachment" "truenas_db_manage_vpc" {
  role       = aws_iam_role.truenas_db_manage.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole"
}

resource "aws_iam_role_policy" "truenas_db_manage_ssm" {
  name = "platform-truenas-db-manage-ssm"
  role = aws_iam_role.truenas_db_manage.id
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

# Dedicated security group - egress only to TrueNAS Postgres
resource "aws_security_group" "truenas_db_manage" {
  name        = "platform-truenas-db-manage"
  description = "TrueNAS DB manage Lambda - port 5432 to TrueNAS only"
  vpc_id      = nonsensitive(data.aws_ssm_parameter.vpc_id.value)

  egress {
    description = "PostgreSQL to TrueNAS"
    from_port   = 5432
    to_port     = 5432
    protocol    = "tcp"
    cidr_blocks = ["192.168.66.3/32"]
  }

  egress {
    description = "HTTPS for SSM API calls"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_lambda_function" "truenas_db_manage" {
  function_name = "platform-truenas-db-manage"
  role          = aws_iam_role.truenas_db_manage.arn
  handler       = "bootstrap"
  runtime       = "provided.al2023"

  filename         = data.archive_file.truenas_db_manage.output_path
  source_code_hash = data.archive_file.truenas_db_manage.output_base64sha256

  timeout     = 30
  memory_size = 128

  vpc_config {
    subnet_ids         = split(",", nonsensitive(data.aws_ssm_parameter.private_subnet_ids.value))
    security_group_ids = [aws_security_group.truenas_db_manage.id]
  }

  environment {
    variables = {
      PG_HOST           = "192.168.66.3"
      PG_PORT           = "5432"
      PG_ADMIN_USER     = nonsensitive(data.aws_ssm_parameter.truenas_pg_admin_user.value)
      PG_ADMIN_PASSWORD = nonsensitive(data.aws_ssm_parameter.truenas_pg_admin_password.value)
      PROJECT_MAP       = jsonencode({ for k, v in var.truenas_db_projects : k => v })
    }
  }
}

data "aws_ssm_parameter" "truenas_pg_admin_user" {
  name = "/platform/truenas/pg-admin-user"
}

data "aws_ssm_parameter" "truenas_pg_admin_password" {
  name = "/platform/truenas/pg-admin-password"
}

# SSM outputs
resource "aws_ssm_parameter" "truenas_db_manage_function" {
  name  = "${local.ssm_prefix}/truenas-db/function-name"
  type  = "String"
  value = aws_lambda_function.truenas_db_manage.function_name
}
