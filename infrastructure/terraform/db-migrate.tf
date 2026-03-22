# =============================================================================
# Database Migration Service
#
# S3 bucket holds migration files per project:
#   migrations/<project>/001_initial.sql      — forward migrations (auto-triggered)
#   migrations/<project>/rollback/001_initial.sql — rollback scripts
#   migrations/<project>/seed/001_data.sql    — seed data
#
# EventBridge triggers on migration file uploads.
# Manual operations (rollback, seed, drop) via direct Lambda invocation.
# =============================================================================

variable "migration_projects" {
  description = "Registered projects and their database names"
  type        = map(object({ db_name = string }))
  default = {
    platform = { db_name = "platform" }
    svap     = { db_name = "svap" }
  }
}

# --- S3 bucket for migration files ---

resource "aws_s3_bucket" "migrations" {
  bucket = "platform-migrations-${data.aws_caller_identity.current.account_id}"
}

resource "aws_s3_bucket_versioning" "migrations" {
  bucket = aws_s3_bucket.migrations.id
  versioning_configuration { status = "Enabled" }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "migrations" {
  bucket = aws_s3_bucket.migrations.id
  rule {
    apply_server_side_encryption_by_default { sse_algorithm = "AES256" }
  }
}

resource "aws_s3_bucket_public_access_block" "migrations" {
  bucket                  = aws_s3_bucket.migrations.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_notification" "migrations" {
  bucket      = aws_s3_bucket.migrations.id
  eventbridge = true
}

# --- Lambda ---

data "archive_file" "db_migrate" {
  type        = "zip"
  source_file = "${path.module}/../../apps/db-migrate/dist/handler.js"
  output_path = "${path.module}/db-migrate-lambda.zip"
}

resource "aws_iam_role" "db_migrate" {
  name               = "platform-db-migrate"
  assume_role_policy = data.aws_iam_policy_document.auth_trigger_assume.json
}

resource "aws_iam_role_policy_attachment" "db_migrate_basic" {
  role       = aws_iam_role.db_migrate.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy_attachment" "db_migrate_vpc" {
  role       = aws_iam_role.db_migrate.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole"
}

resource "aws_iam_role_policy" "db_migrate_s3" {
  name = "platform-db-migrate-s3"
  role = aws_iam_role.db_migrate.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["s3:GetObject", "s3:ListBucket"]
      Resource = [aws_s3_bucket.migrations.arn, "${aws_s3_bucket.migrations.arn}/*"]
    }]
  })
}

resource "aws_security_group" "db_migrate" {
  name        = "platform-db-migrate"
  description = "DB migration Lambda"
  vpc_id      = nonsensitive(data.aws_ssm_parameter.vpc_id.value)

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_lambda_function" "db_migrate" {
  function_name = "platform-db-migrate"
  role          = aws_iam_role.db_migrate.arn
  handler       = "handler.handler"
  runtime       = "nodejs24.x"

  filename         = data.archive_file.db_migrate.output_path
  source_code_hash = data.archive_file.db_migrate.output_base64sha256

  timeout     = 120
  memory_size = 256

  vpc_config {
    subnet_ids         = split(",", nonsensitive(data.aws_ssm_parameter.private_subnet_ids.value))
    security_group_ids = [aws_security_group.db_migrate.id]
  }

  environment {
    variables = {
      DB_HOST           = aws_db_instance.platform.address
      DB_PORT           = tostring(aws_db_instance.platform.port)
      DB_USER           = aws_db_instance.platform.username
      DB_PASSWORD       = random_password.rds_master.result
      MIGRATIONS_BUCKET = aws_s3_bucket.migrations.id
      PROJECT_MAP       = jsonencode({ for k, v in var.migration_projects : k => v })
    }
  }
}

# --- EventBridge rule: trigger on migration file uploads ---

resource "aws_cloudwatch_event_rule" "migration_upload" {
  name = "platform-db-migration-trigger"

  event_pattern = jsonencode({
    source      = ["aws.s3"]
    detail-type = ["Object Created"]
    detail = {
      bucket = { name = [aws_s3_bucket.migrations.id] }
      object = { key = [{ prefix = "migrations/" }] }
    }
  })
}

resource "aws_cloudwatch_event_target" "migration_lambda" {
  rule = aws_cloudwatch_event_rule.migration_upload.name
  arn  = aws_lambda_function.db_migrate.arn
}

resource "aws_lambda_permission" "eventbridge_migrate" {
  statement_id  = "AllowEventBridgeInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.db_migrate.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.migration_upload.arn
}

# --- SSM outputs ---

resource "aws_ssm_parameter" "migrations_bucket" {
  name  = "${local.ssm_prefix}/db/migrations-bucket"
  type  = "String"
  value = aws_s3_bucket.migrations.id
}

resource "aws_ssm_parameter" "db_migrate_function" {
  name  = "${local.ssm_prefix}/db/migrate-function"
  type  = "String"
  value = aws_lambda_function.db_migrate.function_name
}
