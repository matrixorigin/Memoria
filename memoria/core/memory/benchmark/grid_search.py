"""Grid search over activation hyperparameters.

Runs the benchmark multiple times with different parameter combinations,
comparing results in a summary table.

Requires Docker access to restart the API container with different env vars.
"""

from __future__ import annotations

import itertools
import json
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from memoria.core.memory.benchmark import (
    BenchmarkExecutor,
    load_dataset,
    score_dataset,
    score_scenario,
)


@dataclass
class GridResult:
    params: dict[str, str]
    overall_score: float
    overall_grade: str
    scenario_scores: dict[str, float]


# Fast grid — ~4 combos, finishes in ~10 min. Covers the two highest-impact params.
FAST_GRID: dict[str, list[Any]] = {
    "MEM_ACTIVATION_ENTITY_LINK_MULTIPLIER": [1.2, 2.0],
    "MEM_ACTIVATION_ASSOCIATION_THRESHOLD": [0.45, 0.65],
}

# Full grid — ~48 combos, covers all tunable scoring weights.
# Run after dataset is stable. Expect ~2h.
FULL_GRID: dict[str, list[Any]] = {
    # Entity link strength vs association edge noise cutoff
    "MEM_ACTIVATION_ENTITY_LINK_MULTIPLIER": [1.2, 1.8, 2.4],
    "MEM_ACTIVATION_ASSOCIATION_THRESHOLD": [0.45, 0.55, 0.65],
    # Final scoring mix: semantic similarity vs graph activation signal
    "MEM_ACTIVATION_LAMBDA_SEMANTIC": [0.25, 0.40],
    "MEM_ACTIVATION_LAMBDA_ACTIVATION": [0.25, 0.40],
    # Spreading decay — how fast activation fades over hops
    "MEM_ACTIVATION_DECAY_RATE": [0.4, 0.6],
    "MEM_ACTIVATION_SPREADING_FACTOR": [0.7, 0.9],
}

# Default grid used when --grid-search flag is passed without explicit grid
DEFAULT_GRID = FAST_GRID


def _restart_api_with_env(
    env_overrides: dict[str, str],
    compose_dir: str,
    service: str = "api",
    port: int = 8100,
    timeout: int = 30,
) -> bool:
    """Restart API container with env overrides. Returns True if healthy."""
    # Stop existing
    subprocess.run(
        ["docker", "compose", "stop", service],
        cwd=compose_dir,
        capture_output=True,
    )
    # Build env args
    env_args: list[str] = []
    for k, v in env_overrides.items():
        env_args.extend(["-e", f"{k}={v}"])

    # Run with overrides
    subprocess.run(
        [
            "docker",
            "compose",
            "run",
            "-d",
            "--name",
            f"memoria-{service}-grid",
            "-p",
            f"{port}:8000",
            *env_args,
            "--rm",
            service,
        ],
        cwd=compose_dir,
        capture_output=True,
    )

    # Wait for health
    import httpx

    for _ in range(timeout):
        try:
            r = httpx.get(
                f"http://127.0.0.1:{port}/health",
                timeout=2,
                trust_env=False,
            )
            if r.status_code == 200:
                return True
        except Exception:
            pass
        time.sleep(1)
    return False


def _stop_grid_api(compose_dir: str) -> None:
    subprocess.run(
        ["docker", "rm", "-f", "memoria-api-grid"],
        capture_output=True,
    )
    subprocess.run(
        ["docker", "compose", "up", "-d", "api"],
        cwd=compose_dir,
        capture_output=True,
    )


def run_grid_search(
    dataset_path: str,
    api_url: str,
    api_token: str,
    compose_dir: str,
    grid: dict[str, list[Any]] | None = None,
    timeout: float = 30.0,
    output_dir: str = "/tmp/grid_search",
) -> list[GridResult]:
    """Run grid search over activation params.

    Args:
        dataset_path: Path to benchmark dataset JSON.
        api_url: Base API URL.
        api_token: API token.
        compose_dir: Path to docker-compose.yml directory.
        grid: Parameter grid. Keys are env var names, values are lists of values.
        timeout: HTTP timeout per request.
        output_dir: Directory to save individual run results.
    """
    if grid is None:
        grid = DEFAULT_GRID

    Path(output_dir).mkdir(parents=True, exist_ok=True)
    dataset = load_dataset(dataset_path)

    # Generate all combinations
    keys = list(grid.keys())
    values = list(grid.values())
    combos = list(itertools.product(*values))

    results: list[GridResult] = []
    print(f"Grid search: {len(combos)} combinations over {keys}")

    for i, combo in enumerate(combos):
        env = dict(zip(keys, (str(v) for v in combo)))
        label = " ".join(f"{k.split('_')[-1]}={v}" for k, v in env.items())
        print(f"\n[{i + 1}/{len(combos)}] {label}")

        # Restart API with these params
        if not _restart_api_with_env(env, compose_dir):
            print("  ✗ API failed to start, skipping")
            continue

        try:
            executor = BenchmarkExecutor(
                api_url=api_url,
                api_token=api_token,
                timeout=timeout,
                strategy="activation:v1",
            )
            try:
                executions = {}
                for scenario in dataset.scenarios:
                    execution = executor.execute(scenario)
                    executions[scenario.scenario_id] = execution
                    result = score_scenario(scenario, execution)
                    print(f"  {scenario.scenario_id}: {result.total_score:.1f}", end="")
                print()
            finally:
                executor.close()

            report = score_dataset(dataset, executions)
            scenario_scores = {r.scenario_id: r.total_score for r in report.results}

            gr = GridResult(
                params=env,
                overall_score=report.overall_score,
                overall_grade=report.overall_grade,
                scenario_scores=scenario_scores,
            )
            results.append(gr)
            print(f"  Overall: {report.overall_score:.1f} ({report.overall_grade})")

            # Save individual result
            run_file = Path(output_dir) / f"run_{i:03d}.json"
            run_file.write_text(
                json.dumps(
                    {
                        "params": env,
                        "score": report.overall_score,
                        "scenarios": scenario_scores,
                    },
                    indent=2,
                ),
                encoding="utf-8",
            )
        except Exception as e:
            print(f"  ✗ Error: {e}")
        finally:
            _stop_grid_api(compose_dir)
            time.sleep(2)

    # Print summary
    if results:
        print("\n" + "=" * 70)
        print("GRID SEARCH RESULTS")
        print("=" * 70)
        results.sort(key=lambda r: r.overall_score, reverse=True)
        for rank, gr in enumerate(results, 1):
            params_str = " ".join(
                f"{k.split('_')[-1]}={v}" for k, v in gr.params.items()
            )
            print(
                f"  #{rank} {gr.overall_score:5.1f} ({gr.overall_grade}) {params_str}"
            )

        # Save summary
        summary_file = Path(output_dir) / "summary.json"
        summary_file.write_text(
            json.dumps(
                [
                    {
                        "rank": i + 1,
                        "params": r.params,
                        "score": r.overall_score,
                        "grade": r.overall_grade,
                    }
                    for i, r in enumerate(results)
                ],
                indent=2,
            ),
            encoding="utf-8",
        )
        print(f"\nSaved to {output_dir}/")

    return results
