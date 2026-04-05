.PHONY: ci lint fmt test test-integration terraform-fmt-check build deploy

ci: lint fmt test terraform-fmt-check

lint:
	cd backend && cargo clippy -- -D warnings

fmt:
	cd backend && cargo fmt --check

test:
	cd backend && cargo test --lib

test-integration:
	cd backend && cargo test --test '*'

terraform-fmt-check:
	terraform fmt -check -recursive infrastructure/terraform/

build:
	cd backend && cargo lambda build --release

deploy:
	scripts/deploy.sh
