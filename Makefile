.PHONY: ci lint fmt test test-integration terraform-fmt-check build deploy

ci: lint fmt test terraform-fmt-check

lint:
	cd apps && cargo clippy -- -D warnings

fmt:
	cd apps && cargo fmt --check

test:
	cd apps && cargo test --lib

test-integration:
	cd apps && cargo test --test '*'

terraform-fmt-check:
	terraform fmt -check -recursive infrastructure/terraform/

build:
	cd apps && cargo lambda build --release

deploy:
	scripts/deploy.sh
