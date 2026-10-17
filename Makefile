.PHONY: run check install self-host

run:
	cargo run --locked

check:
	cargo fmt --all -- --check
	cargo clippy --locked --all-targets -- -D warnings
	cargo test --locked --all-targets -- --show-output
	cargo build --locked

install:
	scripts/install.sh

self-host:
	scripts/self-host.sh
