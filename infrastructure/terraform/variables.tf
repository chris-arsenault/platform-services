variable "domain_name" {
  description = "Primary domain for platform services"
  type        = string
  default     = "ahara.io"
}

variable "cognito_user_pool_name" {
  description = "Name for the shared Cognito user pool"
  type        = string
  default     = "scorchbook-ffcf7631-users"
}

variable "cognito_clients" {
  description = "Map of app keys to Cognito client display names"
  type        = map(string)
  default = {
    scorchbook = "scorchbook-ffcf7631-app"
    svap       = "svap-app"
    canonry    = "scorchbook-ffcf7631-canonry-app"
    ahara      = "ahara-app"
  }
}

variable "seed_user_email" {
  description = "Email for the seed admin user"
  type        = string
  default     = "chris@chris-arsenault.net"
}
