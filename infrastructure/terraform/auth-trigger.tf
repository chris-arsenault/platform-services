# =============================================================================
# Pre-Authentication Lambda
# Gates Cognito login by checking per-user app access in DynamoDB.
# =============================================================================

data "archive_file" "auth_trigger" {
  type        = "zip"
  source_file = "${path.module}/../../apps/auth-trigger/dist/handler.js"
  output_path = "${path.module}/auth-trigger-lambda.zip"
}

data "aws_iam_policy_document" "auth_trigger_assume" {
  statement {
    effect = "Allow"
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
    actions = ["sts:AssumeRole"]
  }
}

data "aws_iam_policy_document" "auth_trigger" {
  statement {
    actions   = ["dynamodb:GetItem"]
    resources = [aws_dynamodb_table.user_access.arn]
  }
  statement {
    actions   = ["ssm:GetParameter"]
    resources = ["arn:aws:ssm:*:*:parameter${local.ssm_prefix}/auth-trigger/client-map"]
  }
}

resource "aws_iam_role" "auth_trigger" {
  name               = "platform-auth-trigger"
  assume_role_policy = data.aws_iam_policy_document.auth_trigger_assume.json
}

resource "aws_iam_role_policy_attachment" "auth_trigger_basic" {
  role       = aws_iam_role.auth_trigger.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy" "auth_trigger" {
  name   = "platform-auth-trigger-inline"
  role   = aws_iam_role.auth_trigger.id
  policy = data.aws_iam_policy_document.auth_trigger.json
}

resource "aws_lambda_function" "auth_trigger" {
  function_name = "platform-auth-trigger"
  role          = aws_iam_role.auth_trigger.arn
  handler       = "handler.handler"
  runtime       = "nodejs24.x"

  filename         = data.archive_file.auth_trigger.output_path
  source_code_hash = data.archive_file.auth_trigger.output_base64sha256

  timeout     = 5
  memory_size = 128

  environment {
    variables = {
      TABLE_NAME       = aws_dynamodb_table.user_access.name
      CLIENT_MAP_PARAM = "${local.ssm_prefix}/auth-trigger/client-map"
    }
  }
}

resource "aws_lambda_permission" "auth_trigger_cognito" {
  statement_id  = "AllowCognitoInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.auth_trigger.function_name
  principal     = "cognito-idp.amazonaws.com"
  source_arn    = module.cognito.user_pool_arn
}

resource "aws_ssm_parameter" "auth_client_map" {
  name = "${local.ssm_prefix}/auth-trigger/client-map"
  type = "String"
  value = jsonencode(merge(
    { for key, id in module.cognito.client_ids : id => key },
    { (aws_cognito_user_pool_client.sonarqube.id) = "sonarqube" }
  ))
}
