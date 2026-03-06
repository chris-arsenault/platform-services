output "user_pool_id" {
  description = "Cognito user pool ID"
  value       = aws_cognito_user_pool.pool.id
}

output "user_pool_arn" {
  description = "Cognito user pool ARN"
  value       = aws_cognito_user_pool.pool.arn
}

output "client_ids" {
  description = "Map of client keys to app client IDs"
  value       = { for key, client in aws_cognito_user_pool_client.clients : key => client.id }
}

output "domain_name" {
  description = "Cognito custom domain"
  value       = var.domain_name
}
