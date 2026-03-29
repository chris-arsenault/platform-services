# =============================================================================
# Shared PostgreSQL RDS instance (smallest viable size)
# =============================================================================

data "aws_ssm_parameter" "vpc_id" {
  name = "/platform/network/vpc-id"
}

data "aws_ssm_parameter" "private_subnet_ids" {
  name = "/platform/network/private-subnet-ids"
}

data "aws_ssm_parameter" "alb_security_group_id" {
  name = "/platform/network/alb-security-group-id"
}

data "aws_ssm_parameter" "lambda_security_group_id" {
  name = "/platform/network/lambda-security-group-id"
}

data "aws_ssm_parameter" "alb_listener_arn" {
  name = "/platform/network/alb-listener-arn"
}

data "aws_ssm_parameter" "alb_dns_name" {
  name = "/platform/network/alb-dns-name"
}

data "aws_ssm_parameter" "alb_zone_id" {
  name = "/platform/network/alb-zone-id"
}

resource "aws_db_subnet_group" "platform" {
  name       = "platform-db"
  subnet_ids = split(",", nonsensitive(data.aws_ssm_parameter.private_subnet_ids.value))
}

resource "aws_security_group" "rds" {
  name        = "platform-rds"
  description = "Shared RDS access"
  vpc_id      = nonsensitive(data.aws_ssm_parameter.vpc_id.value)

  ingress {
    description = "PostgreSQL from VPC"
    from_port   = 5432
    to_port     = 5432
    protocol    = "tcp"
    cidr_blocks = ["10.42.0.0/16"]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "random_password" "rds_master" {
  length  = 24
  special = false
}

resource "aws_db_instance" "platform" {
  identifier = "platform-shared"

  engine         = "postgres"
  engine_version = "16"
  instance_class = "db.t4g.micro"

  allocated_storage     = 20
  max_allocated_storage = 50
  storage_type          = "gp3"
  storage_encrypted     = true

  db_name  = "platform"
  username = "platform_admin"
  password = random_password.rds_master.result

  db_subnet_group_name   = aws_db_subnet_group.platform.name
  vpc_security_group_ids = [aws_security_group.rds.id]

  multi_az            = false
  publicly_accessible = false
  skip_final_snapshot = true

  backup_retention_period = 7
  backup_window           = "04:00-05:00"
  maintenance_window      = "sun:06:00-sun:07:00"

  performance_insights_enabled = false

  lifecycle {
    ignore_changes = [engine_version]
  }
}
