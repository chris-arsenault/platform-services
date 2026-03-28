# Platform-wide CORS preflight handler
# A single Lambda responds to all OPTIONS requests with CORS headers.
# Projects do not need their own OPTIONS listener rules — only tower-http
# CorsLayer (or equivalent) on actual responses.

data "archive_file" "cors_handler" {
  type        = "zip"
  output_path = "${path.module}/cors-handler.zip"

  source {
    content  = <<-PY
def handler(event, context):
    return {
        "statusCode": 204,
        "headers": {
            "Access-Control-Allow-Origin": "*",
            "Access-Control-Allow-Methods": "GET, POST, PUT, DELETE, OPTIONS, HEAD",
            "Access-Control-Allow-Headers": "Authorization, Content-Type",
            "Access-Control-Max-Age": "86400"
        },
        "body": ""
    }
PY
    filename = "index.py"
  }
}

resource "aws_iam_role" "cors_handler" {
  name = "platform-cors-handler"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Action    = "sts:AssumeRole"
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
    }]
  })
}

resource "aws_iam_role_policy_attachment" "cors_handler" {
  role       = aws_iam_role.cors_handler.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_lambda_function" "cors_handler" {
  function_name    = "platform-cors-handler"
  role             = aws_iam_role.cors_handler.arn
  handler          = "index.handler"
  runtime          = "python3.13"
  timeout          = 3
  memory_size      = 128
  filename         = data.archive_file.cors_handler.output_path
  source_code_hash = data.archive_file.cors_handler.output_base64sha256
}

resource "aws_lb_target_group" "cors_handler" {
  name        = "platform-cors-tg"
  target_type = "lambda"
}

resource "aws_lb_target_group_attachment" "cors_handler" {
  target_group_arn = aws_lb_target_group.cors_handler.arn
  target_id        = aws_lambda_function.cors_handler.arn
  depends_on       = [aws_lambda_permission.cors_handler]
}

resource "aws_lambda_permission" "cors_handler" {
  statement_id  = "AllowALBInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.cors_handler.function_name
  principal     = "elasticloadbalancing.amazonaws.com"
  source_arn    = aws_lb_target_group.cors_handler.arn
}

resource "aws_lb_listener_rule" "cors_preflight" {
  listener_arn = nonsensitive(data.aws_ssm_parameter.alb_listener_arn.value)
  priority     = 1

  condition {
    http_request_method {
      values = ["OPTIONS"]
    }
  }

  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.cors_handler.arn
  }
}
