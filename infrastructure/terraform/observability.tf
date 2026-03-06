# =============================================================================
# Observability — Alarms, SNS, Budgets
# =============================================================================

# --- SNS Topic for platform-wide alarms ---

resource "aws_sns_topic" "alarms" {
  name = "platform-alarms"
}

resource "aws_ssm_parameter" "alarm_topic_arn" {
  name  = "${local.ssm_prefix}/alarms/sns-topic-arn"
  type  = "String"
  value = aws_sns_topic.alarms.arn
}

# --- Budget alert ---

resource "aws_budgets_budget" "monthly" {
  name         = "platform-monthly"
  budget_type  = "COST"
  limit_amount = "100"
  limit_unit   = "USD"
  time_unit    = "MONTHLY"

  notification {
    comparison_operator       = "GREATER_THAN"
    threshold                 = 80
    threshold_type            = "PERCENTAGE"
    notification_type         = "ACTUAL"
    subscriber_sns_topic_arns = [aws_sns_topic.alarms.arn]
  }

  notification {
    comparison_operator       = "GREATER_THAN"
    threshold                 = 100
    threshold_type            = "PERCENTAGE"
    notification_type         = "ACTUAL"
    subscriber_sns_topic_arns = [aws_sns_topic.alarms.arn]
  }
}

# --- Cost anomaly detection ---

resource "aws_ce_anomaly_monitor" "platform" {
  name              = "platform-cost-monitor"
  monitor_type      = "DIMENSIONAL"
  monitor_dimension = "SERVICE"
}

resource "aws_ce_anomaly_subscription" "platform" {
  name = "platform-cost-anomaly"

  monitor_arn_list = [aws_ce_anomaly_monitor.platform.arn]

  frequency = "IMMEDIATE"

  subscriber {
    type    = "SNS"
    address = aws_sns_topic.alarms.arn
  }

  threshold_expression {
    dimension {
      key           = "ANOMALY_TOTAL_IMPACT_ABSOLUTE"
      values        = ["10"]
      match_options = ["GREATER_THAN_OR_EQUAL"]
    }
  }
}
