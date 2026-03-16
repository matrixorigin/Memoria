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

# MCP tests — snapshot/branch tests must run serially (rollback is account-level)
test-mcp:
	$(CARGO) test -p memoria-mcp --test core_tools_e2e --test mcp_e2e -- --nocapture
	$(CARGO) test -p memoria-mcp --test snapshot_e2e --test branch_e2e --test integration_full --test graph_e2e -- --test-threads=1 --nocapture

# All tests (serial for snapshot/branch safety)
test:
	$(CARGO) test -p memoria-core -p memoria-service -p memoria-storage -p memoria-git -- --nocapture
	$(CARGO) test -p memoria-mcp --test tools_unit --test core_tools_e2e --test mcp_e2e -- --nocapture
	$(CARGO) test -p memoria-mcp --test snapshot_e2e --test branch_e2e --test integration_full --test graph_e2e -- --test-threads=1 --nocapture
	$(CARGO) test -p memoria-api --test api_e2e -- --nocapture

clean:
	cargo clean
