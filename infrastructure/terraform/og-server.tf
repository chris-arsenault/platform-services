# =============================================================================
# OG Server — platform Lambda artifact for dynamic OpenGraph HTML generation.
#
# Built in this repo, uploaded to S3. Deployed per-project by the website
# module in ahara-tf-patterns (reads bucket/key from SSM).
# =============================================================================

data "archive_file" "og_server" {
  type        = "zip"
  source_file = "${path.module}/../../backend/target/lambda/og-server/bootstrap"
  output_path = "${path.module}/og-server-lambda.zip"
}

resource "aws_s3_object" "og_server" {
  bucket       = aws_s3_bucket.migrations.id
  key          = "platform/og-server/${data.archive_file.og_server.output_md5}.zip"
  source       = data.archive_file.og_server.output_path
  source_hash  = data.archive_file.og_server.output_md5
  content_type = "application/zip"
}

resource "aws_ssm_parameter" "og_server_s3_bucket" {
  name  = "${local.ssm_prefix}/og-server/s3-bucket"
  type  = "String"
  value = aws_s3_bucket.migrations.id
}

resource "aws_ssm_parameter" "og_server_s3_key" {
  name  = "${local.ssm_prefix}/og-server/s3-key"
  type  = "String"
  value = aws_s3_object.og_server.key
}
