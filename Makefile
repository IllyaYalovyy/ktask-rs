.PHONY: run check golden install self-host

# What the PTY tests recorded in the last run, compared with tests/golden.
SCREENS := $(CURDIR)/target/screens
# The suite gets no artifact directory: only the screen gate writes evidence.
SUITE := env -u KTASK_ARTIFACT_DIR KTASK_SCREENS_DIR="$(SCREENS)" cargo test --locked --all-targets

run:
	cargo run --locked

check:
	cargo fmt --all -- --check
	cargo clippy --locked --all-targets -- -D warnings
	rm -rf "$(SCREENS)"
	$(SUITE) -- --show-output
	cargo run --locked --quiet --example screens -- verify "$(SCREENS)"
	cargo build --locked

# Records every screen again and adopts the result as tests/golden. The test that reads the
# approved set is left to `check`, so it cannot stop the set from being regenerated.
golden:
	rm -rf "$(SCREENS)"
	$(SUITE) -- --skip every_screen_recorded_before_the_golden_set_is_in_it
	cargo run --locked --quiet --example screens -- update "$(SCREENS)"

install:
	scripts/install-user.sh

self-host:
	scripts/self-host.sh
