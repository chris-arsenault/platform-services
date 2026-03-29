# =============================================================================
# Komodo Proxy Lambda — generic proxy to TrueNAS Komodo API
#
# Resolves SSM secrets and forwards Komodo API calls via VPN.
# Invoked by the deploy-truenas shared action and komodo-deploy CLI.
# =============================================================================

data "archive_file" "komodo_proxy" {
  type        = "zip"
  source_file = "${path.module}/../../apps/target/lambda/komodo-proxy/bootstrap"
  output_path = "${path.module}/komodo-proxy-lambda.zip"
}

resource "aws_iam_role" "komodo_proxy" {
  name               = "platform-komodo-proxy"
  assume_role_policy = data.aws_iam_policy_document.auth_trigger_assume.json
}

resource "aws_iam_role_policy_attachment" "komodo_proxy_basic" {
  role       = aws_iam_role.komodo_proxy.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy_attachment" "komodo_proxy_vpc" {
  role       = aws_iam_role.komodo_proxy.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole"
}

resource "aws_iam_role_policy" "komodo_proxy_ssm" {
  name = "platform-komodo-proxy-ssm"
  role = aws_iam_role.komodo_proxy.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "ssm:GetParameter"
      ]
      Resource = [
        "arn:aws:ssm:${data.aws_region.current.id}:${data.aws_caller_identity.current.account_id}:parameter/platform/*"
      ]
    }]
  })
}

resource "aws_lambda_function" "komodo_proxy" {
  function_name = "platform-komodo-proxy"
  role          = aws_iam_role.komodo_proxy.arn
  handler       = "bootstrap"
  runtime       = "provided.al2023"

  filename         = data.archive_file.komodo_proxy.output_path
  source_code_hash = data.archive_file.komodo_proxy.output_base64sha256

  timeout     = 30
  memory_size = 128

  vpc_config {
    subnet_ids         = split(",", nonsensitive(data.aws_ssm_parameter.private_subnet_ids.value))
    security_group_ids = [nonsensitive(data.aws_ssm_parameter.lambda_security_group_id.value)]
  }

  environment {
    variables = {
      KOMODO_URL = "http://192.168.66.3:30160"
    }
  }
}

# --- SSM outputs ---

resource "aws_ssm_parameter" "komodo_proxy_function" {
  name  = "${local.ssm_prefix}/komodo/function-name"
  type  = "String"
  value = aws_lambda_function.komodo_proxy.function_name
}
