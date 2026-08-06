.PHONY: check build dist lint fmt test screencasts screencast-gif docs docs-serve

check:
	cargo check --all-features

build:
	cargo build --all-features

# Set TARGET to cross-compile, e.g. TARGET=x86_64-unknown-linux-musl.
dist:
	cargo build --profile dist --locked -p wiff --bin wiff $(if $(TARGET),--target $(TARGET))

lint:
	cargo clippy --all-features --all-targets -- -D warnings

fmt:
	cargo +nightly fmt

test:
	cargo nextest run --all-features

# Regenerate the per-scenario casts and the amalgamated wiff-demo.cast from the
# wiff-cast tool. Requires a writable stage (WIFF_CAST_STAGE, default
# /tmp/wiff-cast).
screencasts:
	cargo build --release -p wiff
	cargo run --release -q -p wiff-cast

# Render the amalgamated cast to a single animated GIF for hosts that cannot play
# a cast, such as the GitHub README. Requires asciinema's `agg`: cargo install
# --locked --git https://github.com/asciinema/agg.
screencast-gif:
	@command -v agg >/dev/null || { echo "agg not found; install with: cargo install --locked --git https://github.com/asciinema/agg"; exit 1; }
	agg --theme asciinema docs/assets/casts/wiff-demo.cast docs/assets/wiff-demo.gif

# Build the documentation site into ./site.
docs:
	rm -rf .zensical site
	zensical build --clean

docs-serve:
	zensical serve -a 0.0.0.0:8000
