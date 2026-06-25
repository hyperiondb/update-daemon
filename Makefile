.PHONY: all build release test test-core e2e fmt fmt-check clippy clean run validate install

all: build

build:
	cargo build --workspace

release:
	cargo build --release -p renew

test-core:
	cargo test -p renew-core

test:
	cargo test --workspace
	cargo test -p renew --features testkit --test e2e

e2e:
	cargo test -p renew --features testkit --test e2e -- --nocapture

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

clean:
	cargo clean

run:
	cargo run -p renew -- run

validate:
	cargo run -p renew -- validate
