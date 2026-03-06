# =============================================================================
# DNS Hosted Zones (data sources — zones are managed via Route53 console/registrar)
# =============================================================================

data "aws_route53_zone" "ahara" {
  name         = "${var.domain_name}."
  private_zone = false
}

# Publish zone IDs so other projects can create records without hardcoding
resource "aws_ssm_parameter" "dns_ahara_zone_id" {
  name  = "${local.ssm_prefix}/dns/ahara-io-zone-id"
  type  = "String"
  value = data.aws_route53_zone.ahara.zone_id
}
