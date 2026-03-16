DATABASE_URL ?= mysql://root:111@localhost:6001/memoria_rs
CARGO = SQLX_OFFLINE=true DATABASE_URL=$(DATABASE_URL) cargo

.PHONY: check test test-unit test-integration build clean

check:
	$(CARGO) check

build:
	$(CARGO) build

build-release:
	$(CARGO) build --release -p memoria-mcp
	@echo "Binary: target/release/memoria-mcp-rs ($(shell du -sh target/release/memoria-mcp-rs | cut -f1))"

# Unit tests only (no DB needed)
test-unit:
	SQLX_OFFLINE=true cargo test --lib -p memoria-core -p memoria-service -p memoria-mcp -- --nocapture

# Integration tests (needs MatrixOne)
test-integration:
	$(CARGO) test -p memoria-storage --test store_crud --test mo_compat --test mo_correct -- --nocapture

# All tests
test:
	$(CARGO) test

clean:
	cargo clean
