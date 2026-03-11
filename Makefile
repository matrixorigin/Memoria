.PHONY: help start stop logs status build test test-docker test-mcp test-all test-all-cov clean reset \
        cloud-start cloud-stop cloud-logs cloud-status cloud-clean cloud-rebuild \
        install dev build-wheel publish publish-test

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
	@echo "  make cloud-rebuild      Rebuild + restart API container"
	@echo "  make cloud-clean        Stop + remove all data"
	@echo ""
	@echo "Development:"
	@echo "  make install            Install dependencies (editable)"
	@echo "  make dev                Start API locally (no Docker, needs DB)"
	@echo ""
	@echo "Tests:"
	@echo "  make test               Run API e2e tests (TestClient, needs DB)"
	@echo "  make test-unit          Run unit tests (14 tests)"
	@echo "  make test-integration   Run integration tests (9 tests, needs DB)"
	@echo "  make test-docker        Run Docker integration tests (needs: make start)"
	@echo "  make test-mcp           Run MCP server tests"
	@echo "  make test-all           Run all tests"
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

cloud-rebuild:
	@docker compose build api
	@docker compose up -d api
	@echo "API rebuilt and restarted"

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

dev:
	@python -m uvicorn memoria.api.main:app --reload --port 8100

# ── Tests ───────────────────────────────────────────────────────────

test:
	@python -m pytest memoria/tests/test_e2e.py -v

test-unit:
	@python -m pytest tests/unit/ -v -n auto

test-integration:
	@python -m pytest tests/integration/ -v

test-docker:
	@echo "Requires: make start"
	@python -m pytest memoria/tests/test_docker.py -v

test-mcp:
	@python -m pytest memoria/tests/test_mcp.py -v

test-all: test-unit test test-mcp
	@echo "For Docker tests: make start && make test-docker"

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
