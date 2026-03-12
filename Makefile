.PHONY: help start stop logs status build test test-fast test-slow test-docker test-mcp test-all-cov clean reset \
        cloud-start cloud-stop cloud-logs cloud-status cloud-health cloud-clean cloud-rebuild \
        install dev build-wheel publish publish-test bump-version check lint format type-check \
        new-key list-keys revoke-keys

# Load environment variables from .env file if it exists
ifneq (,$(wildcard .env))
    include .env
    export
endif

help:
	@echo "Memoria — Multi-tenant memory service"
	@echo ""
	@echo "Quick Start:"
	@echo "  make start              Start all services (MatrixOne + API)"
	@echo "  make stop               Stop all services"
	@echo "  make status             Show service status"
	@echo "  make logs               Follow API logs"
	@echo ""
	@echo "Docker / Cloud:"
	@echo "  make cloud-start        Alias for start"
	@echo "  make cloud-stop         Alias for stop"
	@echo "  make cloud-logs         Alias for logs"
	@echo "  make cloud-status       Alias for status"
	@echo "  make cloud-health       Check API health status"
	@echo "  make cloud-rebuild      Rebuild + restart API container"
	@echo "  make cloud-clean        Stop + remove all data"
	@echo ""
	@echo "Development:"
	@echo "  make install            Install dependencies (editable)"
	@echo "  make dev                Start API locally (no Docker, needs DB)"
	@echo "  make check              Run lint + type check"
	@echo "  make format             Auto-fix and reformat code"
	@echo "  make bump-version BUMP=patch  - Bump version (patch/minor/major)"
	@echo ""
	@echo "Tests:"
	@echo "  make test               Run API e2e tests (TestClient, needs DB)"
	@echo "  make test-unit          Run unit tests (14 tests)"
	@echo "  make test               Run all tests (unit + e2e + mcp + integration, needs DB)"
	@echo "  make test-unit          Run unit tests (no DB needed)"
	@echo "  make test-integration   Run integration tests (needs DB)"
	@echo "  make test-docker        Run Docker integration tests (needs: make start)"
	@echo "  make test-mcp           Run MCP server tests"
	@echo "  make test-all-cov       Run all tests with coverage report"
	@echo ""
	@echo "Build & Publish:"
	@echo "  make build              Build Docker image"
	@echo "  make build-wheel        Build wheel distribution"
	@echo "  make publish-test       Publish to TestPyPI"
	@echo "  make publish            Publish to PyPI"
	@echo ""
	@echo "Maintenance:"
	@echo "  make clean              Remove build artifacts"
	@echo "  make reset              Stop + remove data + restart fresh"
	@echo ""
	@echo "API Keys (dev):"
	@echo "  make new-key USER=alice NAME=dev-key   Create API key for user"
	@echo "  make list-keys USER=alice              List active keys for user"
	@echo "  make revoke-keys USER=alice            Revoke all keys for user"

# ── Docker / Cloud ──────────────────────────────────────────────────

start:
	@echo "Starting Memoria..."
	@docker compose up -d
	@echo "API: http://localhost:$${API_PORT:-8100}  Swagger: http://localhost:$${API_PORT:-8100}/docs"

stop:
	@docker compose down

status:
	@docker compose ps

logs:
	@docker compose logs -f api

build:
	@docker compose build api

cloud-start: start
cloud-stop: stop
cloud-logs: logs
cloud-status: status

cloud-health:
	@echo "Checking Memoria API health..."
	@API_PORT=$${API_PORT:-8100}; \
	if curl -s --noproxy localhost http://localhost:$$API_PORT/health > /dev/null 2>&1; then \
		curl -s --noproxy localhost http://localhost:$$API_PORT/health | python -m json.tool 2>/dev/null || curl -s --noproxy localhost http://localhost:$$API_PORT/health; \
		echo ""; \
	else \
		echo "❌ API is not responding on port $$API_PORT"; \
		echo "   Make sure the service is running: make cloud-start"; \
		exit 1; \
	fi

cloud-rebuild:
	@docker compose build api
	@docker compose up -d api
	@echo "API rebuilt and restarted"

new-key:
	@API_PORT=$${API_PORT:-8100}; \
	USER=$${USER:-mo-developer}; \
	NAME=$${NAME:-default}; \
	MASTER_KEY=$${MEMORIA_MASTER_KEY:-test-master-key-for-docker-compose}; \
	echo "Creating API key for user '$$USER' (name: $$NAME)..."; \
	RESPONSE=$$(curl -s -w "\n%{http_code}" --noproxy localhost \
		-X POST "http://localhost:$$API_PORT/auth/keys" \
		-H "Authorization: Bearer $$MASTER_KEY" \
		-H "Content-Type: application/json" \
		-d "{\"user_id\": \"$$USER\", \"name\": \"$$NAME\"}" 2>/dev/null); \
	HTTP_CODE=$$(echo "$$RESPONSE" | tail -n1); \
	BODY=$$(echo "$$RESPONSE" | sed '$$d'); \
	if [ "$$HTTP_CODE" = "201" ]; then \
		echo "✅ API key created successfully!"; \
		echo ""; \
		echo "Raw Key:   $$(echo "$$BODY" | python -c "import sys,json; d=json.load(sys.stdin); print(d.get('raw_key','N/A'))" 2>/dev/null || echo "$$BODY")"; \
		echo "Key ID:    $$(echo "$$BODY" | python -c "import sys,json; d=json.load(sys.stdin); print(d.get('key_id','N/A'))" 2>/dev/null)"; \
		echo "User ID:   $$(echo "$$BODY" | python -c "import sys,json; d=json.load(sys.stdin); print(d.get('user_id','N/A'))" 2>/dev/null)"; \
		echo "Name:      $$(echo "$$BODY" | python -c "import sys,json; d=json.load(sys.stdin); print(d.get('name','N/A'))" 2>/dev/null)"; \
		echo "Prefix:    $$(echo "$$BODY" | python -c "import sys,json; d=json.load(sys.stdin); print(d.get('key_prefix','N/A'))" 2>/dev/null)"; \
		echo ""; \
		echo "⚠️  Save the raw key now - it won't be shown again!"; \
	elif [ "$$HTTP_CODE" = "401" ]; then \
		echo "❌ Authentication failed — check MEMORIA_MASTER_KEY"; \
		exit 1; \
	elif [ "$$HTTP_CODE" = "000" ]; then \
		echo "❌ Cannot connect to API on port $$API_PORT — run: make start"; \
		exit 1; \
	else \
		echo "❌ Failed to create API key (HTTP $$HTTP_CODE): $$BODY"; \
		exit 1; \
	fi

list-keys:
	@API_PORT=$${API_PORT:-8100}; \
	USER=$${USER:-mo-developer}; \
	MASTER_KEY=$${MEMORIA_MASTER_KEY:-test-master-key-for-docker-compose}; \
	echo "Active API keys for user '$$USER':"; \
	RESPONSE=$$(curl -s -w "\n%{http_code}" --noproxy localhost \
		-X GET "http://localhost:$$API_PORT/admin/users/$$USER/keys" \
		-H "Authorization: Bearer $$MASTER_KEY" 2>/dev/null); \
	HTTP_CODE=$$(echo "$$RESPONSE" | tail -n1); \
	BODY=$$(echo "$$RESPONSE" | sed '$$d'); \
	if [ "$$HTTP_CODE" = "200" ]; then \
		echo "$$BODY" | python -c \
		'import sys,json; d=json.load(sys.stdin); keys=d.get("keys",[]); print("\n  %d key(s) found\n"%len(keys)); [print("  [%d] %s  (%s...)\n      key_id:     %s\n      created:    %s\n      expires:    %s\n      last_used:  %s\n"%(i+1,k["name"],k["key_prefix"],k["key_id"],k["created_at"][:19],k["expires_at"][:19] if k.get("expires_at") else "never",k["last_used_at"][:19] if k.get("last_used_at") else "never")) for i,k in enumerate(keys)]'; \
	elif [ "$$HTTP_CODE" = "401" ] || [ "$$HTTP_CODE" = "403" ]; then \
		echo "❌ Authentication failed — check MEMORIA_MASTER_KEY"; \
		exit 1; \
	elif [ "$$HTTP_CODE" = "000" ]; then \
		echo "❌ Cannot connect to API on port $$API_PORT — run: make start"; \
		exit 1; \
	else \
		echo "❌ Failed (HTTP $$HTTP_CODE): $$BODY"; \
		exit 1; \
	fi

revoke-keys:
	@API_PORT=$${API_PORT:-8100}; \
	USER=$${USER:-mo-developer}; \
	MASTER_KEY=$${MEMORIA_MASTER_KEY:-test-master-key-for-docker-compose}; \
	echo "Revoking all API keys for user '$$USER'..."; \
	RESPONSE=$$(curl -s -w "\n%{http_code}" --noproxy localhost \
		-X DELETE "http://localhost:$$API_PORT/admin/users/$$USER/keys" \
		-H "Authorization: Bearer $$MASTER_KEY" 2>/dev/null); \
	HTTP_CODE=$$(echo "$$RESPONSE" | tail -n1); \
	BODY=$$(echo "$$RESPONSE" | sed '$$d'); \
	if [ "$$HTTP_CODE" = "200" ]; then \
		REVOKED=$$(echo "$$BODY" | python -c "import sys,json; print(json.load(sys.stdin).get('revoked',0))" 2>/dev/null); \
		echo "✅ Revoked $$REVOKED key(s) for user '$$USER'"; \
	elif [ "$$HTTP_CODE" = "401" ] || [ "$$HTTP_CODE" = "403" ]; then \
		echo "❌ Authentication failed — check MEMORIA_MASTER_KEY"; \
		exit 1; \
	elif [ "$$HTTP_CODE" = "000" ]; then \
		echo "❌ Cannot connect to API on port $$API_PORT — run: make start"; \
		exit 1; \
	else \
		echo "❌ Failed (HTTP $$HTTP_CODE): $$BODY"; \
		exit 1; \
	fi

cloud-clean:
	@docker compose down
	@rm -rf data/
	@echo "All data removed"

reset:
	@docker compose down
	@rm -rf data/
	@docker compose up -d
	@echo "Reset complete"

# ── Local Development ───────────────────────────────────────────────

install:
	@pip install -e ".[dev,openai-embedding]"

check: lint type-check

lint:
	@ruff check memoria/ tests/
	@ruff format --check memoria/ tests/

format:
	@ruff check --fix memoria/ tests/
	@ruff format memoria/ tests/

type-check:
	@mypy memoria/

dev:
	@python -m uvicorn memoria.api.main:app --reload --port 8100

# ── Tests ───────────────────────────────────────────────────────────

test:
	@python -m pytest tests/unit/ memoria/tests/test_e2e.py memoria/tests/test_mcp.py tests/integration/ -v -n auto --dist=loadgroup --ignore=tests/integration/test_mcp_stdio_e2e.py
	@echo "For Docker tests: make start && make test-docker"

test-fast:
	@python -m pytest tests/unit/ memoria/tests/test_e2e.py memoria/tests/test_mcp.py -v -n auto

test-slow:
	@python -m pytest tests/integration/ -v -n auto --dist=loadgroup --ignore=tests/integration/test_mcp_stdio_e2e.py

test-unit:
	@python -m pytest tests/unit/ -v -n auto

test-integration:
	@python -m pytest tests/integration/ -v -n auto --dist=loadgroup --ignore=tests/integration/test_mcp_stdio_e2e.py

test-docker:
	@echo "Requires: make start"
	@python -m pytest memoria/tests/test_docker.py -v

test-mcp:
	@python -m pytest memoria/tests/test_mcp.py -v

test-all-cov:
	@echo "Running all tests with coverage..."
	@python -m pytest tests/unit/ memoria/tests/test_e2e.py memoria/tests/test_mcp.py \
		--cov=memoria --cov-report=term-missing --cov-report=html:htmlcov \
		-v -n auto 2>&1 | tee coverage.log
	@echo ""
	@echo "✅ Coverage report generated:"
	@echo "   - Terminal: see above"
	@echo "   - HTML: htmlcov/index.html"
	@echo "   - Log: coverage.log"

# ── Build & Publish ─────────────────────────────────────────────────

DIST = dist

build-wheel:
	@echo "Building memoria wheel..."
	@rm -rf $(DIST)
	@pip install --quiet build 2>/dev/null || true
	@python -m build --wheel --outdir $(DIST)
	@echo "✅ Built: $$(ls $(DIST)/*.whl)"

publish: build-wheel
	@echo "Publishing to PyPI..."
	@pip install --quiet twine 2>/dev/null || true
	@twine upload $(DIST)/*
	@echo "✅ Published to PyPI"

publish-test: build-wheel
	@echo "Publishing to TestPyPI..."
	@pip install --quiet twine 2>/dev/null || true
	@twine upload --repository testpypi $(DIST)/*
	@echo "✅ Published to TestPyPI"

# ── Clean ───────────────────────────────────────────────────────────

clean:
	@rm -rf $(DIST) build/ *.egg-info memoria/*.egg-info
	@find . -name "__pycache__" -type d -exec rm -rf {} + 2>/dev/null || true
	@find . -name "*.pyc" -delete 2>/dev/null || true
	@echo "Cleaned"

# ── Version Management ──────────────────────────────────────────────

.PHONY: bump-version
bump-version:
	@python scripts/bump_version.py $(BUMP)
