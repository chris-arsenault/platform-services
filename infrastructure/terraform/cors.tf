# Platform-wide CORS preflight handler
# A single Rust Lambda responds to all OPTIONS requests with CORS headers.
# Deployed at ALB priority 1 — projects do not need their own OPTIONS rules.

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

module "cors_handler" {
  source   = "git::https://github.com/chris-arsenault/ahara-tf-patterns.git//modules/lambda"
  name     = "platform-cors-handler"
  binary   = "${path.module}/../../backend/target/lambda/cors-handler/bootstrap"
  role_arn = aws_iam_role.cors_handler.arn
  timeout  = 3
  memory   = 128
}

resource "aws_lb_target_group" "cors_handler" {
  name        = "platform-cors-tg"
  target_type = "lambda"
}

resource "aws_lb_target_group_attachment" "cors_handler" {
  target_group_arn = aws_lb_target_group.cors_handler.arn
  target_id        = module.cors_handler.function_arn
  depends_on       = [aws_lambda_permission.cors_handler]
}

resource "aws_lambda_permission" "cors_handler" {
  statement_id  = "AllowALBInvoke"
  action        = "lambda:InvokeFunction"
  function_name = module.cors_handler.function_name
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
