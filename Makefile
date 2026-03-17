.PHONY: help check-env up down status logs logs-db \
        up-db down-db up-api down-api rebuild-api health \
        dev build build-local check \
        test test-unit test-integration test-e2e bench \
        new-key list-keys revoke-keys \
        clean reset

# Load .env if present
ifneq (,$(wildcard .env))
    include .env
    export
endif

DB_URL ?= mysql://root:$(or $(MEMORIA_DB_PASSWORD),111)@localhost:$(or $(MATRIXONE_PORT),6001)/$(or $(MEMORIA_DB_NAME),memoria)
TEST_DB_URL ?= mysql://root:$(or $(MEMORIA_DB_PASSWORD),111)@localhost:$(or $(MATRIXONE_PORT),6001)/$(or $(MEMORIA_TEST_DB_NAME),memoria_test)

# Ensure cargo is in PATH
export PATH := $(HOME)/.cargo/bin:$(PATH)

help:
	@echo "Memoria — Persistent memory for AI agents"
	@echo ""
	@echo "Docker (all services):"
	@echo "  make up                 Start MatrixOne + API"
	@echo "  make down               Stop all"
	@echo "  make status             Show service status"
	@echo "  make logs               Follow API logs"
	@echo "  make logs-db            Follow MatrixOne logs"
	@echo "  make health             Check API /health"
	@echo "  make reset              Stop + delete data + restart"
	@echo "  make clean              Stop + delete all data"
	@echo ""
	@echo "Docker (individual):"
	@echo "  make up-db              Start MatrixOne only"
	@echo "  make down-db            Stop MatrixOne only"
	@echo "  make up-api             Start API container only"
	@echo "  make down-api           Stop API container only"
	@echo "  make rebuild-api        Rebuild + restart API container"
	@echo ""
	@echo "Local dev (no Docker for API):"
	@echo "  make dev                Run API locally (RUST_LOG=debug make dev)"
	@echo "  make build              Build release binary"
	@echo "  make build-local        Build with local embedding support"
	@echo "  make check              cargo check + clippy"
	@echo ""
	@echo "Tests:"
	@echo "  make test               All tests (needs DB)"
	@echo "  make test-unit          Unit tests (no DB)"
	@echo "  make test-e2e           E2E API tests (needs DB)"
	@echo "  make bench              Run benchmark (needs: make up)"
	@echo ""
	@echo "API Keys:"
	@echo "  make new-key USER=alice NAME=dev-key"
	@echo "  make list-keys USER=alice"
	@echo "  make revoke-keys USER=alice"

# ── Docker: all services ────────────────────────────────────────────

check-env:
	@if [ ! -f .env ]; then echo "❌ .env not found. Run: cp .env.example .env && edit .env"; exit 1; fi

up: check-env
	@docker compose up -d
	@echo "API: http://localhost:$${API_PORT:-8100}"

down:
	@docker compose down

status:
	@docker compose ps

logs:
	@docker compose logs -f api

logs-db:
	@docker compose logs -f matrixone

health:
	@API_PORT=$${API_PORT:-8100}; \
	curl -sf --noproxy localhost http://localhost:$$API_PORT/health && echo "" || \
	(echo "❌ API not responding on port $$API_PORT"; exit 1)

# ── Docker: individual services ─────────────────────────────────────

up-db:
	@docker compose up -d matrixone
	@echo "MatrixOne: localhost:$${MATRIXONE_PORT:-6001}"

down-db:
	@docker compose stop matrixone matrixone-init

up-api: check-env
	@docker compose up -d api
	@echo "API: http://localhost:$${API_PORT:-8100}"

down-api:
	@docker compose stop api

rebuild-api: check-env
	@docker compose build api
	@docker compose up -d api
	@echo "API rebuilt and restarted"

# ── Local dev ───────────────────────────────────────────────────────

dev: check-env
	@cd memoria && RUST_LOG=$${RUST_LOG:-info} DATABASE_URL=$(DB_URL) SQLX_OFFLINE=true \
		EMBEDDING_PROVIDER=$${MEMORIA_EMBEDDING_PROVIDER:-openai} \
		EMBEDDING_API_KEY=$${MEMORIA_EMBEDDING_API_KEY:-} \
		EMBEDDING_BASE_URL=$${MEMORIA_EMBEDDING_BASE_URL:-} \
		EMBEDDING_MODEL=$${MEMORIA_EMBEDDING_MODEL:-BAAI/bge-m3} \
		EMBEDDING_DIM=$${MEMORIA_EMBEDDING_DIM:-1024} \
		LLM_API_KEY=$${MEMORIA_LLM_API_KEY:-} \
		LLM_BASE_URL=$${MEMORIA_LLM_BASE_URL:-} \
		LLM_MODEL=$${MEMORIA_LLM_MODEL:-} \
		MASTER_KEY=$${MEMORIA_MASTER_KEY:-} \
		cargo run -p memoria-cli -- serve

build:
	@echo "Building release binary..."
	@cd memoria && SQLX_OFFLINE=true cargo build --release -p memoria-cli
	@echo "Binary: memoria/target/release/memoria"

build-local:
	@echo "Building release binary with local embedding..."
	@cd memoria && SQLX_OFFLINE=true cargo build --release -p memoria-cli --features local-embedding
	@echo "Binary: memoria/target/release/memoria (with local embedding)"

# ── Release ─────────────────────────────────────────────────────────

# Usage: make release VERSION=0.2.0
release:
	@if [ -z "$(VERSION)" ]; then echo "❌ Usage: make release VERSION=x.y.z"; exit 1; fi
	@git diff --quiet || (echo "❌ Uncommitted changes — commit first"; exit 1)
	@echo "==> Bumping version to $(VERSION)..."
	@sed -i 's/^version = ".*"/version = "$(VERSION)"/' memoria/Cargo.toml
	@cd memoria && cargo check 2>/dev/null
	@echo "==> Generating CHANGELOG..."
	@if command -v git-cliff >/dev/null 2>&1; then \
		git-cliff --tag "v$(VERSION)" -o CHANGELOG.md; \
	else \
		echo "⚠️  git-cliff not installed — skipping CHANGELOG (cargo install git-cliff)"; \
	fi
	@git add memoria/Cargo.toml memoria/Cargo.lock CHANGELOG.md 2>/dev/null
	@git commit -m "chore(release): v$(VERSION)"
	@git tag -a "v$(VERSION)" -m "Release v$(VERSION)"
	@echo "==> Pushing..."
	@git push && git push origin "v$(VERSION)"
	@echo "✅ v$(VERSION) released — CI will build binaries + Docker image"

# Pre-release: make release-rc VERSION=0.2.0-rc1
release-rc:
	@if [ -z "$(VERSION)" ]; then echo "❌ Usage: make release-rc VERSION=x.y.z-rcN"; exit 1; fi
	@git diff --quiet || (echo "❌ Uncommitted changes — commit first"; exit 1)
	@sed -i 's/^version = ".*"/version = "$(VERSION)"/' memoria/Cargo.toml
	@cd memoria && cargo check 2>/dev/null
	@git add memoria/Cargo.toml memoria/Cargo.lock 2>/dev/null
	@git commit -m "chore(release): v$(VERSION)"
	@git tag -a "v$(VERSION)" -m "Pre-release v$(VERSION)"
	@git push && git push origin "v$(VERSION)"
	@echo "✅ Pre-release v$(VERSION) pushed"

# Push Docker image locally (without CI)
release-docker:
	@docker compose build api
	@docker tag memoria-api:latest matrixorigin/memoria:latest
	@if [ -n "$(VERSION)" ]; then docker tag memoria-api:latest matrixorigin/memoria:$(VERSION); fi
	@docker push matrixorigin/memoria:latest
	@if [ -n "$(VERSION)" ]; then docker push matrixorigin/memoria:$(VERSION); fi
	@echo "✅ Pushed to Docker Hub"

check:
	@cd memoria && cargo check && cargo clippy -- -D warnings

# ── Tests ───────────────────────────────────────────────────────────

test:
	@cd memoria && DATABASE_URL=$(TEST_DB_URL) SQLX_OFFLINE=true \
		EMBEDDING_API_KEY=$${MEMORIA_EMBEDDING_API_KEY:-} \
		EMBEDDING_BASE_URL=$${MEMORIA_EMBEDDING_BASE_URL:-} \
		EMBEDDING_MODEL=$${MEMORIA_EMBEDDING_MODEL:-BAAI/bge-m3} \
		EMBEDDING_DIM=$${MEMORIA_EMBEDDING_DIM:-1024} \
		LLM_API_KEY=$${MEMORIA_LLM_API_KEY:-} \
		LLM_BASE_URL=$${MEMORIA_LLM_BASE_URL:-} \
		LLM_MODEL=$${MEMORIA_LLM_MODEL:-} \
		cargo test -- --test-threads=1

test-unit:
	@cd memoria && SQLX_OFFLINE=true cargo test --lib -p memoria-core -p memoria-service -p memoria-mcp

test-integration:
	@cd memoria && DATABASE_URL=$(TEST_DB_URL) SQLX_OFFLINE=true cargo test -p memoria-storage -- --test-threads=1

test-e2e:
	@cd memoria && DATABASE_URL=$(TEST_DB_URL) SQLX_OFFLINE=true \
		EMBEDDING_API_KEY=$${MEMORIA_EMBEDDING_API_KEY:-} \
		EMBEDDING_BASE_URL=$${MEMORIA_EMBEDDING_BASE_URL:-} \
		EMBEDDING_MODEL=$${MEMORIA_EMBEDDING_MODEL:-BAAI/bge-m3} \
		EMBEDDING_DIM=$${MEMORIA_EMBEDDING_DIM:-1024} \
		LLM_API_KEY=$${MEMORIA_LLM_API_KEY:-} \
		LLM_BASE_URL=$${MEMORIA_LLM_BASE_URL:-} \
		LLM_MODEL=$${MEMORIA_LLM_MODEL:-} \
		cargo test -p memoria-api --test api_e2e

# ── Benchmark ───────────────────────────────────────────────────────

BENCH_URL   ?= http://localhost:$${API_PORT:-8100}
BENCH_TOKEN ?= $${MEMORIA_MASTER_KEY:-test-master-key-for-docker-compose}

bench: check-env
	@cd memoria && for ds in core-v1 core-v2 forget-v1 graph-entity-v1 graph-entity-v2 large-graph-v1 large-graph-v2; do \
		echo ""; \
		SQLX_OFFLINE=true cargo run -p memoria-cli -- benchmark \
			--api-url "$(BENCH_URL)" --token "$(BENCH_TOKEN)" --dataset $$ds || true; \
	done

# ── API Keys ────────────────────────────────────────────────────────

new-key: check-env
	@API_PORT=$${API_PORT:-8100}; \
	USER=$${USER:-mo-developer}; \
	NAME=$${NAME:-default}; \
	MASTER_KEY=$${MEMORIA_MASTER_KEY:-test-master-key-for-docker-compose}; \
	curl -sf --noproxy localhost \
		-X POST "http://localhost:$$API_PORT/auth/keys" \
		-H "Authorization: Bearer $$MASTER_KEY" \
		-H "Content-Type: application/json" \
		-d "{\"user_id\": \"$$USER\", \"name\": \"$$NAME\"}" && echo "" || \
	echo "❌ Failed — is the API running?"

list-keys: check-env
	@API_PORT=$${API_PORT:-8100}; \
	USER=$${USER:-mo-developer}; \
	MASTER_KEY=$${MEMORIA_MASTER_KEY:-test-master-key-for-docker-compose}; \
	curl -sf --noproxy localhost \
		"http://localhost:$$API_PORT/admin/users/$$USER/keys" \
		-H "Authorization: Bearer $$MASTER_KEY" && echo "" || \
	echo "❌ Failed — is the API running?"

revoke-keys: check-env
	@API_PORT=$${API_PORT:-8100}; \
	USER=$${USER:-mo-developer}; \
	MASTER_KEY=$${MEMORIA_MASTER_KEY:-test-master-key-for-docker-compose}; \
	curl -sf --noproxy localhost \
		-X DELETE "http://localhost:$$API_PORT/admin/users/$$USER/keys" \
		-H "Authorization: Bearer $$MASTER_KEY" && echo "" || \
	echo "❌ Failed — is the API running?"

# ── Clean ───────────────────────────────────────────────────────────

clean:
	@docker compose down
	@rm -rf data/
	@cd memoria && cargo clean
	@echo "All cleaned"

reset: check-env
	@docker compose down
	@rm -rf data/
	@docker compose up -d
	@echo "Reset complete — API: http://localhost:$${API_PORT:-8100}"
