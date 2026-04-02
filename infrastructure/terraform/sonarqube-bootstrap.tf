# =============================================================================
# SonarQube Bootstrap Lambda
#
# Narrowly scoped Lambda for SonarQube post-deploy tasks:
# - Waits for SonarQube health
# - Creates CI analysis token
# - Stores token in SSM
#
# Runs in VPC (same as other TrueNAS Lambdas) to reach SonarQube
# at 192.168.66.3:30090 via the WireGuard VPN.
# =============================================================================

data "archive_file" "sonarqube_bootstrap" {
  type        = "zip"
  source_file = "${path.module}/../../apps/target/lambda/sonarqube-bootstrap/bootstrap"
  output_path = "${path.module}/sonarqube-bootstrap-lambda.zip"
}

resource "aws_iam_role" "sonarqube_bootstrap" {
  name               = "platform-sonarqube-bootstrap"
  assume_role_policy = data.aws_iam_policy_document.auth_trigger_assume.json
}

resource "aws_iam_role_policy_attachment" "sonarqube_bootstrap_basic" {
  role       = aws_iam_role.sonarqube_bootstrap.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy_attachment" "sonarqube_bootstrap_vpc" {
  role       = aws_iam_role.sonarqube_bootstrap.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole"
}

resource "aws_iam_role_policy" "sonarqube_bootstrap_ssm" {
  name = "platform-sonarqube-bootstrap-ssm"
  role = aws_iam_role.sonarqube_bootstrap.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["ssm:GetParameter"]
        Resource = ["arn:aws:ssm:${data.aws_region.current.id}:${data.aws_caller_identity.current.account_id}:parameter/platform/sonarqube/admin-password"]
      },
      {
        Effect   = "Allow"
        Action   = ["ssm:PutParameter"]
        Resource = ["arn:aws:ssm:${data.aws_region.current.id}:${data.aws_caller_identity.current.account_id}:parameter/platform/sonarqube/ci-token"]
      }
    ]
  })
}

resource "aws_lambda_function" "sonarqube_bootstrap" {
  function_name = "platform-sonarqube-bootstrap"
  role          = aws_iam_role.sonarqube_bootstrap.arn
  handler       = "bootstrap"
  runtime       = "provided.al2023"

  filename         = data.archive_file.sonarqube_bootstrap.output_path
  source_code_hash = data.archive_file.sonarqube_bootstrap.output_base64sha256

  timeout     = 660
  memory_size = 128

  vpc_config {
    subnet_ids         = split(",", nonsensitive(data.aws_ssm_parameter.private_subnet_ids.value))
    security_group_ids = [nonsensitive(data.aws_ssm_parameter.lambda_security_group_id.value)]
  }

  environment {
    variables = {
      SONARQUBE_URL = "http://192.168.66.3:30090"
    }
  }
}

resource "aws_ssm_parameter" "sonarqube_bootstrap_function" {
  name  = "${local.ssm_prefix}/sonarqube/bootstrap-function-name"
  type  = "String"
  value = aws_lambda_function.sonarqube_bootstrap.function_name
}
