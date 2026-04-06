# =============================================================================
# Pre-Authentication Lambda
# Gates Cognito login by checking per-user app access in DynamoDB.
# =============================================================================

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

resource "aws_iam_role_policy_attachment" "auth_trigger_vpc" {
  role       = aws_iam_role.auth_trigger.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole"
}

resource "aws_iam_role_policy" "auth_trigger" {
  name   = "platform-auth-trigger-inline"
  role   = aws_iam_role.auth_trigger.id
  policy = data.aws_iam_policy_document.auth_trigger.json
}

module "auth_trigger" {
  source   = "git::https://github.com/chris-arsenault/ahara-tf-patterns.git//modules/lambda"
  name     = "platform-auth-trigger"
  binary   = "${path.module}/../../backend/target/lambda/auth-trigger/bootstrap"
  role_arn = aws_iam_role.auth_trigger.arn
  timeout  = 5

  environment = {
    TABLE_NAME       = aws_dynamodb_table.user_access.name
    CLIENT_MAP_PARAM = "${local.ssm_prefix}/auth-trigger/client-map"
  }
}

resource "aws_lambda_permission" "auth_trigger_cognito" {
  statement_id  = "AllowCognitoInvoke"
  action        = "lambda:InvokeFunction"
  function_name = module.auth_trigger.function_name
  principal     = "cognito-idp.amazonaws.com"
  source_arn    = module.cognito.user_pool_arn
}

data "aws_ssm_parameters_by_path" "auth_trigger_clients" {
  path            = "${local.ssm_prefix}/auth-trigger/clients"
  with_decryption = false
}

locals {
  # Merge platform-managed clients with client-registered entries from SSM
  # Client projects publish their Cognito client ID to /platform/auth-trigger/clients/<name>
  external_client_map = {
    for i, name in data.aws_ssm_parameters_by_path.auth_trigger_clients.names :
    data.aws_ssm_parameters_by_path.auth_trigger_clients.values[i] =>
    replace(name, "${local.ssm_prefix}/auth-trigger/clients/", "")
  }
}

resource "aws_ssm_parameter" "auth_client_map" {
  name = "${local.ssm_prefix}/auth-trigger/client-map"
  type = "String"
  value = jsonencode(merge(
    { for key, id in module.cognito.client_ids : id => key },
    local.external_client_map
  ))
}
