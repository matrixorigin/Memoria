use crate::benchmark;
use std::collections::HashMap;
use std::path::Path;

pub(crate) fn cmd_benchmark(
    api_url: &str,
    token: &str,
    dataset: &str,
    out: Option<&str>,
    validate_only: bool,
) {
    fn print_category_breakdown(
        heading: &str,
        values: &HashMap<String, benchmark::CategoryBreakdown>,
    ) {
        if values.is_empty() {
            return;
        }
        let mut items: Vec<_> = values.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        println!("  {heading}:");
        for (_key, item) in items {
            println!(
                "    {}: {:.1} ({}) [{}]",
                item.label, item.score, item.grade, item.scenario_count
            );
        }
    }

    let dataset_path = {
        let p = Path::new(dataset);
        if p.exists() {
            p.to_path_buf()
        } else {
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
            let candidates = [
                manifest
                    .join("../../../benchmarks/datasets")
                    .join(format!("{dataset}.json")),
                manifest
                    .join("../../../memoria/datasets")
                    .join(format!("{dataset}.json")),
            ];
            candidates
                .into_iter()
                .find(|c| c.exists())
                .unwrap_or_else(|| {
                    eprintln!("Dataset not found: {dataset}");
                    eprintln!("Looked in: benchmarks/datasets/{dataset}.json");
                    std::process::exit(1);
                })
        }
    };

    let content = std::fs::read_to_string(&dataset_path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {e}", dataset_path.display());
        std::process::exit(1);
    });

    if validate_only {
        let errors = benchmark::validate_dataset(&content);
        if errors.is_empty() {
            println!("Dataset is valid.");
        } else {
            println!("Validation failed ({} errors):", errors.len());
            for e in &errors {
                println!("  {e}");
            }
            std::process::exit(1);
        }
        return;
    }

    let ds: benchmark::ScenarioDataset = serde_json::from_str(&content).unwrap_or_else(|e| {
        eprintln!("Failed to parse dataset: {e}");
        std::process::exit(1);
    });
    let api_version = ds.resolved_api_version().unwrap_or_else(|e| {
        eprintln!("Invalid benchmark dataset API version: {e}");
        std::process::exit(1);
    });
    println!(
        "Dataset: {} {} [{}] ({} scenarios)",
        ds.dataset_id,
        ds.version,
        api_version.as_str(),
        ds.scenarios.len()
    );

    let executor = benchmark::BenchmarkExecutor::new(api_url, token, api_version);
    let mut executions = HashMap::new();

    for scenario in &ds.scenarios {
        print!("  Running {}...", scenario.scenario_id);
        let exec = executor.execute(scenario);
        let result = benchmark::score_scenario(scenario, &exec);
        let icon = match result.grade.as_str() {
            "S" | "A" => "✅",
            "B" => "⚠️",
            _ => "❌",
        };
        println!(" {icon} {:.1} ({})", result.total_score, result.grade);
        executions.insert(scenario.scenario_id.clone(), exec);
    }

    let report = benchmark::score_dataset(&ds, &executions);
    println!(
        "\nOverall: {:.1} ({})",
        report.overall_score, report.overall_grade
    );
    if !report.by_difficulty.is_empty() {
        let mut items: Vec<_> = report.by_difficulty.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        print!("  By difficulty:");
        for (k, v) in &items {
            print!(" {k}={v:.1}");
        }
        println!();
    }
    if !report.by_tag.is_empty() {
        let mut items: Vec<_> = report.by_tag.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        print!("  By tag:");
        for (k, v) in &items {
            print!(" {k}={v:.1}");
        }
        println!();
    }
    if !report.by_domain.is_empty() {
        let mut items: Vec<_> = report.by_domain.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        print!("  By domain:");
        for (k, v) in &items {
            print!(" {k}={v:.1}");
        }
        println!();
    }
    print_category_breakdown("By source family", &report.by_source_family);
    print_category_breakdown(
        "LongMemEval official categories",
        &report.by_longmemeval_category,
    );
    print_category_breakdown("BEAM official abilities", &report.by_beam_ability);

    if let Some(path) = out {
        let json = serde_json::to_string_pretty(&report).unwrap();
        std::fs::write(path, &json).unwrap_or_else(|e| eprintln!("Failed to write {path}: {e}"));
        println!("  Saved: {path}");
    }
}
