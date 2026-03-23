.PHONY: lint fmt test build deploy

lint:
	cd apps && cargo clippy -- -D warnings

fmt:
	cd apps && cargo fmt --check

test:
	cd apps && cargo test

build:
	cd apps && cargo lambda build --release

deploy:
	scripts/deploy.sh
