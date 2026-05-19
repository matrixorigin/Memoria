-include .env
export

DATABASE_URL ?= mysql://root:111@localhost:6001/memoria
CARGO = SQLX_OFFLINE=true DATABASE_URL=$(DATABASE_URL) cargo

.PHONY: check test test-unit test-integration build clean \
        build-musl build-aarch64-musl build-all-musl

check:
	$(CARGO) check

build:
	$(CARGO) build

build-release:
	$(CARGO) build --release -p memoria-cli
	@echo "Binary: target/release/memoria ($(shell du -sh target/release/memoria | cut -f1))"

# Unit tests only (no DB needed)
test-unit:
	SQLX_OFFLINE=true cargo test --lib -p memoria-core -p memoria-service -p memoria-mcp -- --nocapture

# Integration tests (needs MatrixOne)
# Uses memoria_test (dim=4) to avoid conflict with API DB (dim=1024)
test-integration:
	DATABASE_URL=mysql://root:111@localhost:6001/memoria_test SQLX_OFFLINE=true cargo test -p memoria-storage --test store_crud --test mo_compat --test mo_correct -- --nocapture
	DATABASE_URL=mysql://root:111@localhost:6001/memoria_test SQLX_OFFLINE=true cargo test -p memoria-storage --test branch_ops -- --test-threads=1 --nocapture

# MCP tests — snapshot/branch tests must run serially (rollback is account-level)
test-mcp:
	$(CARGO) test -p memoria-mcp --test core_tools_e2e --test mcp_e2e -- --nocapture
	$(CARGO) test -p memoria-mcp --test snapshot_e2e --test branch_e2e --test integration_full --test graph_e2e -- --test-threads=1 --nocapture

# All tests (serial for snapshot/branch safety)
test:
	$(CARGO) test -p memoria-core -p memoria-service -p memoria-git -- --nocapture
	DATABASE_URL=mysql://root:111@localhost:6001/memoria_test SQLX_OFFLINE=true cargo test -p memoria-storage --test store_crud --test mo_compat --test mo_correct -- --nocapture
	DATABASE_URL=mysql://root:111@localhost:6001/memoria_test SQLX_OFFLINE=true cargo test -p memoria-storage --test branch_ops -- --test-threads=1 --nocapture
	$(CARGO) test -p memoria-mcp --test tools_unit --test core_tools_e2e --test mcp_e2e -- --nocapture
	$(CARGO) test -p memoria-mcp --test snapshot_e2e --test branch_e2e --test integration_full --test graph_e2e -- --test-threads=1 --nocapture
	$(CARGO) test -p memoria-api --test api_e2e -- --test-threads=1 --nocapture

clean:
	cargo clean

# ── Cross-compilation (mirrors CI release-rust.yml) ─────────────────
#
# Requires:
#   x86_64 musl : sudo apt-get install musl-tools
#                 rustup target add x86_64-unknown-linux-musl
#   aarch64 musl: cargo install cross  (uses Docker; uses separate target/cross/ dir
#                 to avoid glibc cache conflicts with native builds)
#                 sudo apt-get install binutils-aarch64-linux-gnu  (for strip)

build-musl:
	@echo "Building x86_64-unknown-linux-musl..."
	SQLX_OFFLINE=true cargo build --release -p memoria-cli --target x86_64-unknown-linux-musl
	@strip target/x86_64-unknown-linux-musl/release/memoria
	@echo "Binary: target/x86_64-unknown-linux-musl/release/memoria ($(shell du -sh target/x86_64-unknown-linux-musl/release/memoria | cut -f1), statically linked)"
	@ldd target/x86_64-unknown-linux-musl/release/memoria 2>&1 | grep -E 'statically|not a dynamic'

build-aarch64-musl:
	@command -v cross >/dev/null 2>&1 || { echo "❌ cross not installed. Run: cargo install cross"; exit 1; }
	@command -v docker >/dev/null 2>&1 || { echo "❌ docker not running. cross requires Docker"; exit 1; }
	@echo "Building aarch64-unknown-linux-musl (via cross)..."
	SQLX_OFFLINE=true CARGO_TARGET_DIR=target/cross \
		cross build --release -p memoria-cli --target aarch64-unknown-linux-musl
	@aarch64-linux-gnu-strip target/cross/aarch64-unknown-linux-musl/release/memoria 2>/dev/null || \
		echo "⚠️  strip skipped (install: sudo apt-get install binutils-aarch64-linux-gnu)"
	@echo "Binary: target/cross/aarch64-unknown-linux-musl/release/memoria ($(shell du -sh target/cross/aarch64-unknown-linux-musl/release/memoria | cut -f1))"

build-all-musl: build-musl build-aarch64-musl
	@echo "✅ Both musl binaries ready"
	@ls -lh target/x86_64-unknown-linux-musl/release/memoria \
	         target/cross/aarch64-unknown-linux-musl/release/memoria
