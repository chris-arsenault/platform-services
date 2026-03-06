variable "user_pool_name" {
  description = "Name for the Cognito user pool"
  type        = string
}

variable "domain_name" {
  description = "Custom domain name for Cognito (e.g., auth.example.com)"
  type        = string
}

variable "domain_zone_name" {
  description = "Route53 hosted zone name for the Cognito domain"
  type        = string
}

variable "clients" {
  description = "Map of client keys to client names"
  type        = map(string)
}

variable "pre_auth_lambda_arn" {
  description = "ARN of the Pre-Authentication Lambda trigger"
  type        = string
  default     = ""
}
