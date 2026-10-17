.PHONY: check fmt lint test

check: fmt lint test

fmt:
	cargo fmt --check

lint:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test
