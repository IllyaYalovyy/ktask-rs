.PHONY: run check install

run:
	cargo run --locked

check:
	cargo fmt --all -- --check
	cargo clippy --locked --all-targets -- -D warnings
	cargo test --locked --all-targets -- --show-output
	cargo build --locked

install:
	cargo install --locked --path . --bin ktask
