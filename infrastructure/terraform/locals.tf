locals {
  auth_domain      = "auth.${var.domain_name}"
  sonarqube_domain = "sonar.${var.domain_name}"

  user_access_table_name = "platform-user-access"

  # SSM parameter prefix for all platform-level config
  ssm_prefix = "/platform"
}
