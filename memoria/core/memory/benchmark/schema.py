"""Benchmark data models.

Design principles:
- Scenarios carry real data (seed memories, queries, ground truth), not templates
- Scoring compares actual results against ground truth, not self-reported metrics
- All scenario data lives in JSON files, not hardcoded in Python
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field, model_validator

ChallengeTag = Literal[
    "dedup",
    "conflict",
    "trust",
    "drift",
    "interruption",
    "adversarial",
]
DifficultyLevel = Literal["L1", "L2", "L3", "L4", "L5"]
HorizonLevel = Literal["short", "medium", "long"]
GradeLevel = Literal["S", "A", "B", "C", "D"]

DIFFICULTY_ORDER: dict[DifficultyLevel, int] = {
    "L1": 1,
    "L2": 2,
    "L3": 3,
    "L4": 4,
    "L5": 5,
}


def grade_from_score(score: float) -> GradeLevel:
    if score >= 90:
        return "S"
    if score >= 80:
        return "A"
    if score >= 70:
        return "B"
    if score >= 60:
        return "C"
    return "D"


# ---------------------------------------------------------------------------
# Scenario definition — what goes into a JSON dataset file
# ---------------------------------------------------------------------------


class SeedMemory(BaseModel):
    """A memory pre-loaded before the scenario runs."""

    content: str = Field(min_length=1)
    memory_type: str = Field(default="semantic")
    is_noise: bool = Field(default=False, description="True if this is a distractor")
    is_outdated: bool = Field(
        default=False, description="True if this should NOT be returned"
    )
    age_days: float | None = Field(
        default=None,
        description="Simulate memory age: observed_at = now - age_days. Used for decay tests.",
    )
    initial_confidence: float | None = Field(
        default=None, description="Override initial confidence (default: 1.0)"
    )
    trust_tier: str | None = Field(
        default=None, description="Override trust tier (e.g. 'T1', 'T4')"
    )


MaturationOp = Literal[
    "extract_entities",
    "extract_entities_llm",
    "consolidate",
    "governance",
    "reflect",
]


class ScenarioStep(BaseModel):
    """One action the executor performs against the memory system."""

    action: Literal["store", "retrieve", "correct", "purge", "search"]
    # For store/correct:
    content: str | None = None
    memory_type: str | None = None
    # For store: time/confidence/tier overrides (same semantics as SeedMemory)
    age_days: float | None = None
    initial_confidence: float | None = None
    trust_tier: str | None = None
    # For retrieve/search:
    query: str | None = None
    top_k: int | None = None
    # For correct:
    reason: str | None = None
    # For purge:
    memory_ids: list[str] | None = None
    topic: str | None = None  # purge by keyword (no IDs needed)

    @model_validator(mode="after")
    def _check_fields(self) -> "ScenarioStep":
        if self.action in ("retrieve", "search") and not self.query:
            raise ValueError(f"{self.action} step requires query")
        if self.action == "store" and not self.content:
            raise ValueError("store step requires content")
        if self.action == "correct" and not self.content:
            raise ValueError("correct step requires content (new_content)")
        if self.action == "correct" and not self.query:
            raise ValueError(
                "correct step requires query (the search term to find the memory)"
            )
        return self


class FollowUpStrategy(BaseModel):
    """Defines how to generate follow-up queries from initial results.

    Simulates agent heuristic behavior: retrieve → inspect → refine → retrieve again.
    Each strategy is a different "agent personality" that the benchmark evaluates.
    """

    name: str = Field(min_length=1, description="Strategy name for reporting")
    description: str = Field(default="")
    max_rounds: int = Field(default=2, ge=1, le=5)
    # How to pick follow-up queries from returned results:
    #   "entity_expand" — extract entity names from results, query each
    #   "keyword_refine" — combine original query with keywords from results
    #   "chain" — use each result's content as the next query
    mode: Literal["entity_expand", "keyword_refine", "chain"] = "entity_expand"
    # Optional: explicit follow-up queries (overrides mode)
    follow_up_queries: list[str] = Field(default_factory=list)


class MemoryAssertion(BaseModel):
    """What we check after executing the scenario steps."""

    # For retrieval quality: which seed memories should/shouldn't appear
    query: str = Field(min_length=1, description="The retrieval query to evaluate")
    top_k: int = Field(default=5, ge=1)
    # Ground truth: substrings that MUST appear in returned memories
    expected_contents: list[str] = Field(
        min_length=1, description="Substrings that must appear in results"
    )
    # Ground truth: substrings that must NOT appear in returned memories
    excluded_contents: list[str] = Field(
        default_factory=list,
        description="Substrings that must NOT appear in results (noise/outdated)",
    )
    # Optional: agent follow-up strategies to evaluate
    # Each strategy produces a separate score; the assertion passes if ANY strategy passes
    follow_ups: list[FollowUpStrategy] = Field(
        default_factory=list,
        description="Agent heuristic strategies to try. Scored independently.",
    )


class Scenario(BaseModel):
    """A complete benchmark scenario with real data."""

    scenario_id: str = Field(min_length=1)
    title: str = Field(min_length=1)
    description: str = Field(default="")
    domain: str = Field(default="general")
    difficulty: DifficultyLevel
    horizon: HorizonLevel
    tags: list[ChallengeTag] = Field(min_length=1)
    # The actual data
    seed_memories: list[SeedMemory] = Field(
        min_length=1, description="Pre-loaded memories before steps run"
    )
    maturation: list[MaturationOp] = Field(
        default_factory=list,
        description="Post-seed backend ops to run before steps/assertions "
        "(e.g. extract_entities, consolidate). Executed in order.",
    )
    steps: list[ScenarioStep] = Field(
        default_factory=list, description="Actions to perform (store, correct, etc.)"
    )
    assertions: list[MemoryAssertion] = Field(
        min_length=1, description="Ground truth checks after steps"
    )


class ScenarioDataset(BaseModel):
    """A versioned collection of scenarios."""

    dataset_id: str = Field(min_length=1)
    version: str = Field(pattern=r"^v\d+(\.\d+)*$")
    description: str = Field(default="")
    scenarios: list[Scenario] = Field(min_length=1)

    @model_validator(mode="after")
    def _unique_ids(self) -> "ScenarioDataset":
        ids = [s.scenario_id for s in self.scenarios]
        if len(ids) != len(set(ids)):
            raise ValueError("scenario_id must be unique in a dataset")
        return self


# ---------------------------------------------------------------------------
# Scoring results
# ---------------------------------------------------------------------------


class MQSSubScore(BaseModel):
    """Memory Quality Score — how well the system retrieves and maintains memories."""

    precision: float = Field(
        ge=0.0, le=100.0, description="% of returned that are relevant"
    )
    recall: float = Field(
        ge=0.0, le=100.0, description="% of expected that were returned"
    )
    noise_rejection: float = Field(
        ge=0.0, le=100.0, description="% of excluded content correctly absent"
    )

    def score(self) -> float:
        return (self.precision + self.recall + self.noise_rejection) / 3


class AUSSubScore(BaseModel):
    """Agent Utility Score — how well memory supports the agent's task."""

    step_success_rate: float = Field(
        ge=0.0, le=100.0, description="% of steps that succeeded"
    )
    assertion_pass_rate: float = Field(
        ge=0.0, le=100.0, description="% of assertions that passed"
    )

    def score(self) -> float:
        return (self.step_success_rate + self.assertion_pass_rate) / 2


class BenchmarkScoreCard(BaseModel):
    mqs: MQSSubScore
    aus: AUSSubScore
    mqs_weight: float = Field(default=0.65, ge=0.0, le=1.0)
    aus_weight: float = Field(default=0.35, ge=0.0, le=1.0)

    def total_score(self) -> float:
        return self.mqs_weight * self.mqs.score() + self.aus_weight * self.aus.score()

    def grade(self) -> GradeLevel:
        return grade_from_score(self.total_score())


class ScenarioResult(BaseModel):
    scenario_id: str
    title: str
    difficulty: DifficultyLevel
    horizon: HorizonLevel
    tags: list[ChallengeTag]
    scorecard: BenchmarkScoreCard
    total_score: float
    grade: GradeLevel
    # Details for debugging
    assertion_details: list[dict] = Field(default_factory=list)
    step_details: list[dict] = Field(default_factory=list)
    error: str | None = None


class BenchmarkReport(BaseModel):
    dataset_id: str
    version: str
    scenario_count: int
    overall_score: float
    overall_grade: GradeLevel
    by_difficulty: dict[DifficultyLevel, float] = Field(default_factory=dict)
    by_horizon: dict[HorizonLevel, float] = Field(default_factory=dict)
    by_tag: dict[ChallengeTag, float] = Field(default_factory=dict)
    results: list[ScenarioResult]
