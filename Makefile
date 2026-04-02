.PHONY: build build-release test check fmt fmt-check clippy clean

build:
	cd engine && cargo build

build-release:
	cd engine && cargo build --release
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		echo "codesigning..."; \
		codesign --force --sign - engine/target/release/werma; \
	fi

test:
	cd engine && cargo test

clippy:
	cd engine && cargo clippy -- -D warnings

fmt:
	cd engine && cargo fmt

fmt-check:
	cd engine && cargo fmt -- --check

check: fmt-check clippy test

clean:
	cd engine && cargo clean
