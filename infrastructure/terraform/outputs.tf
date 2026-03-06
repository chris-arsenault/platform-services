output "cognito_user_pool_id" {
  description = "Cognito user pool ID"
  value       = module.cognito.user_pool_id
}

output "cognito_client_ids" {
  description = "Map of app keys to Cognito client IDs"
  value       = module.cognito.client_ids
}

output "cognito_chris_password" {
  description = "Initial password for seed admin user"
  value       = random_password.cognito_chris.result
  sensitive   = true
}

output "alarm_topic_arn" {
  description = "SNS topic ARN for platform alarms"
  value       = aws_sns_topic.alarms.arn
}
