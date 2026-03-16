#!/usr/bin/env python3
"""Run the Memoria benchmark against the Rust API server.

Usage:
    python3 benchmarks/run_rust.py [--api-url http://localhost:8100] [--dataset core-v1]

The script starts the Rust API server automatically if --api-url is not given.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

# Add project root to path so we can import memoria
ROOT = Path(__file__).parent.parent
sys.path.insert(0, str(ROOT))

from memoria.core.memory.benchmark.executor import BenchmarkExecutor
from memoria.core.memory.benchmark.schema import ScenarioDataset
from memoria.core.memory.benchmark import scorer as bench_scorer


def wait_for_server(url: str, timeout: float = 30.0) -> bool:
    import httpx
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            r = httpx.get(f"{url}/v1/memories", headers={"X-User-Id": "healthcheck"}, timeout=2)
            if r.status_code < 500:
                return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--api-url", default=None, help="Rust API URL (auto-start if not given)")
    parser.add_argument("--dataset", default="core-v1", help="Dataset name under benchmarks/datasets/")
    parser.add_argument("--token", default="test-master-key-for-docker-compose")
    parser.add_argument("--out", default=None, help="Write JSON report to file")
    args = parser.parse_args()

    dataset_path = ROOT / "benchmarks" / "datasets" / f"{args.dataset}.json"
    if not dataset_path.exists():
        print(f"Dataset not found: {dataset_path}", file=sys.stderr)
        sys.exit(1)

    dataset = ScenarioDataset.model_validate_json(dataset_path.read_text())

    proc = None
    api_url = args.api_url
    if api_url is None:
        # Auto-start Rust API
        env = os.environ.copy()
        binary = ROOT / "memoria_rs" / "target" / "release" / "memoria-api"
        if not binary.exists():
            binary = ROOT / "memoria_rs" / "target" / "debug" / "memoria-api"
        if not binary.exists():
            print("Rust API binary not found. Build with: cargo build -p memoria-api", file=sys.stderr)
            sys.exit(1)
        api_url = "http://127.0.0.1:8100"
        print(f"Starting Rust API: {binary}")
        proc = subprocess.Popen(
            [str(binary)],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if not wait_for_server(api_url):
            proc.terminate()
            print("Rust API failed to start", file=sys.stderr)
            sys.exit(1)
        print(f"Rust API ready at {api_url}")

    try:
        executor = BenchmarkExecutor(api_url=api_url, api_token=args.token)

        print(f"\nRunning {len(dataset.scenarios)} scenarios from {args.dataset}...\n")
        results = []
        executions = {}
        for scenario in dataset.scenarios:
            execution = executor.execute(scenario)
            executions[scenario.scenario_id] = execution
            result = bench_scorer.score_scenario(scenario, execution)
            results.append(result)
            grade = result.grade
            score = result.total_score
            status = "✅" if grade in ("S", "A") else "⚠️" if grade == "B" else "❌"
            print(f"  {status} {scenario.scenario_id:40s} {grade}  {score:5.1f}  {scenario.difficulty}/{scenario.horizon}")

        report = bench_scorer.score_dataset(dataset, executions)
        print(f"\n{'='*60}")
        print(f"Overall: {report.overall_grade}  {report.overall_score:.1f}/100")
        print(f"By difficulty: {dict(report.by_difficulty)}")
        print(f"By tag:        {dict(report.by_tag)}")

        if args.out:
            Path(args.out).write_text(report.model_dump_json(indent=2))
            print(f"\nReport written to {args.out}")

    finally:
        if proc:
            proc.terminate()
        executor.close()


if __name__ == "__main__":
    main()
