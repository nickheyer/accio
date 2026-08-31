.PHONY: test build dev release

test:
	cargo test --workspace

build:
	cargo build --release

dev:
	cargo run

release:
	bash scripts/release/pushReleaseTag.sh $(RELEASE_FLAGS)
