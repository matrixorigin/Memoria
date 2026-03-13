"""Execute benchmark scenarios against a live Memoria API.

The executor:
1. Creates an isolated session per scenario
2. Loads seed memories into the system
3. Runs scenario steps (store, correct, purge, etc.)
4. Runs assertion queries and collects raw results
5. Returns raw results for the scorer to evaluate against ground truth
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Any

import httpx

from memoria.core.memory.benchmark.schema import (
    MemoryAssertion,
    Scenario,
    ScenarioStep,
)


@dataclass
class StepResult:
    action: str
    success: bool
    error: str | None = None


@dataclass
class MaturationResult:
    op: str
    success: bool
    error: str | None = None


@dataclass
class FollowUpResult:
    strategy_name: str
    returned_contents: list[str] = field(default_factory=list)
    rounds: int = 0
    queries_used: list[str] = field(default_factory=list)


@dataclass
class AssertionResult:
    query: str
    returned_contents: list[str] = field(default_factory=list)
    follow_up_results: list[FollowUpResult] = field(default_factory=list)
    error: str | None = None


@dataclass
class ScenarioExecution:
    """Raw execution output — no scoring yet."""

    scenario_id: str
    step_results: list[StepResult] = field(default_factory=list)
    assertion_results: list[AssertionResult] = field(default_factory=list)
    maturation_results: list[MaturationResult] = field(default_factory=list)
    error: str | None = None


class BenchmarkExecutor:
    """Executes scenarios against the Memoria REST API.

    Each scenario gets its own user_id (bench-{run_id}-{scenario_id}) so
    graph nodes and edges are fully isolated between scenarios.
    """

    def __init__(
        self,
        api_url: str,
        api_token: str,
        timeout: float = 30.0,
        client: httpx.Client | None = None,
        strategy: str | None = None,
    ) -> None:
        self._run_id = str(int(time.time()))
        self._strategy = strategy
        self._api_token = api_token
        self._owned = client is None
        self._base_url = api_url.rstrip("/")
        self._timeout = timeout
        self._client = client or self._make_client("init")

    def _make_client(self, scenario_suffix: str) -> httpx.Client:
        user_id = f"bench-{self._run_id}-{scenario_suffix}"
        return httpx.Client(
            base_url=self._base_url,
            headers={
                "Authorization": f"Bearer {self._api_token}",
                "X-Impersonate-User": user_id,
            },
            timeout=self._timeout,
            trust_env=False,
        )

    def close(self) -> None:
        if self._owned:
            self._client.close()

    def execute(self, scenario: Scenario) -> ScenarioExecution:
        # Each scenario gets its own user + client for full graph isolation
        sid = scenario.scenario_id.lower()
        user_id = f"bench-{self._run_id}-{sid}"
        if self._owned:
            self._client.close()
            self._client = self._make_client(sid)
        session_id = f"bench-{self._run_id}-{sid}"
        execution = ScenarioExecution(scenario_id=scenario.scenario_id)
        try:
            # Phase 0: pin strategy for this scenario's user
            if self._strategy:
                self._set_strategy(user_id, self._strategy)

            # Phase 1: load seed memories
            for seed in scenario.seed_memories:
                self._store(seed.content, seed.memory_type, session_id)

            # Phase 2: maturation — run backend ops so graph/entities are built
            for op in scenario.maturation:
                mr = self._run_maturation(op, user_id)
                execution.maturation_results.append(mr)

            # Phase 3: run steps
            for step in scenario.steps:
                step_result = self._run_step(step, session_id)
                execution.step_results.append(step_result)

            # Phase 4: run assertion queries
            for assertion in scenario.assertions:
                assertion_result = self._run_assertion(assertion, session_id)
                execution.assertion_results.append(assertion_result)
        except Exception as e:
            execution.error = str(e)
        return execution

    def setup(self, scenario: Scenario) -> str:
        """Seed + maturation only. Returns the user_id for later evaluate() calls."""
        sid = scenario.scenario_id.lower()
        user_id = f"bench-{self._run_id}-{sid}"
        if self._owned:
            self._client.close()
            self._client = self._make_client(sid)
        session_id = f"bench-{self._run_id}-{sid}"

        for seed in scenario.seed_memories:
            self._store(seed.content, seed.memory_type, session_id)

        for op in scenario.maturation:
            self._run_maturation(op, user_id)

        for step in scenario.steps:
            self._run_step(step, session_id)

        return user_id

    def evaluate(
        self,
        scenario: Scenario,
        user_id: str,
        strategy: str,
    ) -> ScenarioExecution:
        """Run assertions only against an already-seeded user, with a given strategy."""
        sid = scenario.scenario_id.lower()
        session_id = f"bench-{self._run_id}-{sid}"
        # Ensure client has the right user header
        if self._owned:
            self._client.close()
            self._client = self._make_client(sid)

        # Reset access counts so evaluation order doesn't affect frequency boost
        self._client.post(
            f"/admin/users/{user_id}/reset-access-counts",
            headers={"Authorization": f"Bearer {self._api_token}"},
        )

        self._set_strategy(user_id, strategy)

        execution = ScenarioExecution(scenario_id=scenario.scenario_id)
        try:
            for assertion in scenario.assertions:
                assertion_result = self._run_assertion(assertion, session_id)
                execution.assertion_results.append(assertion_result)
        except Exception as e:
            execution.error = str(e)
        return execution

    def _store(self, content: str, memory_type: str, session_id: str) -> bool:
        resp = self._client.post(
            "/v1/memories",
            json={
                "content": content,
                "memory_type": memory_type,
                "session_id": session_id,
                "source": "benchmark",
            },
        )
        resp.raise_for_status()
        return True

    # ── Maturation: trigger backend ops via admin endpoint (no cooldown) ──

    def _run_maturation(self, op: str, user_id: str) -> MaturationResult:
        try:
            resp = self._client.post(
                f"/admin/governance/{user_id}/trigger",
                params={"op": op},
            )
            resp.raise_for_status()
            return MaturationResult(op=op, success=True)
        except Exception as e:
            return MaturationResult(op=op, success=False, error=str(e))

    def _set_strategy(self, user_id: str, strategy: str) -> None:
        """Pin retrieval strategy for the benchmark user via admin endpoint."""
        resp = self._client.post(
            f"/admin/users/{user_id}/strategy",
            params={"strategy": strategy},
        )
        resp.raise_for_status()

    def _retrieve(
        self, query: str, session_id: str, top_k: int = 5
    ) -> list[dict[str, Any]]:
        resp = self._client.post(
            "/v1/memories/retrieve",
            json={
                "query": query,
                "top_k": top_k,
                "session_id": session_id,
                "include_cross_session": False,
            },
        )
        resp.raise_for_status()
        data = resp.json()
        return data if isinstance(data, list) else []

    def _search(self, query: str, top_k: int = 10) -> list[dict[str, Any]]:
        resp = self._client.post(
            "/v1/memories/search",
            json={"query": query, "top_k": top_k},
        )
        resp.raise_for_status()
        data = resp.json()
        return data if isinstance(data, list) else []

    def _correct(self, query: str, new_content: str, reason: str) -> bool:
        resp = self._client.post(
            "/v1/memories/correct",
            json={"query": query, "new_content": new_content, "reason": reason},
        )
        return resp.status_code < 400

    def _purge(
        self, memory_ids: list[str], reason: str, topic: str | None = None
    ) -> bool:
        body: dict = {"reason": reason}
        if topic:
            body["topic"] = topic
        elif memory_ids:
            body["memory_ids"] = memory_ids
        else:
            return True  # nothing to purge
        resp = self._client.post("/v1/memories/purge", json=body)
        return resp.status_code < 400

    def _run_step(self, step: ScenarioStep, session_id: str) -> StepResult:
        try:
            if step.action == "store":
                self._store(
                    step.content or "", step.memory_type or "semantic", session_id
                )
                return StepResult(action="store", success=True)
            elif step.action == "retrieve":
                self._retrieve(step.query or "", session_id, step.top_k or 5)
                return StepResult(action="retrieve", success=True)
            elif step.action == "search":
                self._search(step.query or "", step.top_k or 10)
                return StepResult(action="search", success=True)
            elif step.action == "correct":
                ok = self._correct(
                    query=step.query or "",
                    new_content=step.content or "",
                    reason=step.reason or "benchmark correction",
                )
                return StepResult(action="correct", success=ok)
            elif step.action == "purge":
                ok = self._purge(
                    step.memory_ids or [],
                    reason=step.reason or "benchmark purge",
                    topic=step.topic,
                )
                return StepResult(action="purge", success=ok)
            else:
                return StepResult(
                    action=step.action, success=False, error="unknown action"
                )
        except Exception as e:
            return StepResult(action=step.action, success=False, error=str(e))

    def _run_assertion(
        self, assertion: MemoryAssertion, session_id: str
    ) -> AssertionResult:
        try:
            items = self._retrieve(assertion.query, session_id, assertion.top_k)
            contents = [item.get("content", "") for item in items]
            result = AssertionResult(query=assertion.query, returned_contents=contents)

            # Run follow-up strategies (agent heuristic simulation)
            for strategy in assertion.follow_ups:
                fur = self._run_follow_up(strategy, assertion, session_id, contents)
                result.follow_up_results.append(fur)

            return result
        except Exception as e:
            return AssertionResult(query=assertion.query, error=str(e))

    def _run_follow_up(
        self,
        strategy: Any,
        assertion: MemoryAssertion,
        session_id: str,
        initial_contents: list[str],
    ) -> FollowUpResult:
        """Execute a follow-up strategy: retrieve → inspect → refine → retrieve."""
        import re

        fur = FollowUpResult(strategy_name=strategy.name)
        all_contents = list(initial_contents)
        fur.queries_used.append(assertion.query)

        for _round in range(strategy.max_rounds):
            # Generate follow-up queries
            if strategy.follow_up_queries:
                # Explicit queries — use them in order
                idx = _round
                if idx >= len(strategy.follow_up_queries):
                    break
                queries = [strategy.follow_up_queries[idx]]
            elif strategy.mode == "entity_expand":
                # Extract entity-like terms from results
                entities = set()
                for c in all_contents:
                    # Extract @mentions, CamelCase, quoted terms, tech terms
                    entities.update(re.findall(r"@\w+", c))
                    entities.update(
                        w for w in re.findall(r"[A-Z][a-z]+(?:[A-Z][a-z]+)+", c)
                    )
                    entities.update(re.findall(r"[`'\"]([^`'\"]+)[`'\"]", c))
                # Remove already-queried terms
                new_entities = entities - set(fur.queries_used)
                if not new_entities:
                    break
                queries = list(new_entities)[:3]
            elif strategy.mode == "keyword_refine":
                # Combine original query with keywords from results
                words = set()
                for c in all_contents:
                    words.update(c.split()[:5])
                refined = f"{assertion.query} {' '.join(list(words)[:3])}"
                queries = [refined]
            elif strategy.mode == "chain":
                # Use last result as next query
                if all_contents:
                    queries = [all_contents[-1][:200]]
                else:
                    break
            else:
                break

            for q in queries:
                fur.queries_used.append(q)
                items = self._retrieve(q, session_id, assertion.top_k)
                new_contents = [
                    item.get("content", "")
                    for item in items
                    if item.get("content", "") not in all_contents
                ]
                all_contents.extend(new_contents)

            fur.rounds += 1

        fur.returned_contents = all_contents
        return fur
