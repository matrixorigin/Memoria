/// memoria-rs CLI — configure AI tools to use Memoria memory service.
///
/// Commands:
///   memoria-rs init       — detect tools, write MCP config + steering rules
///   memoria-rs status     — show configuration status
///   memoria-rs update-rules — update steering rules to latest version

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

const VERSION: &str = "0.1.23";
const MCP_KEY: &str = "memoria";

// ── Embedded steering templates ───────────────────────────────────────────────

const KIRO_STEERING: &str = include_str!("../templates/kiro_steering.md");
const CURSOR_RULE: &str = include_str!("../templates/cursor_rule.md");
const CLAUDE_RULE: &str = include_str!("../templates/claude_rule.md");

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "memoria-rs", version = VERSION, about = "Configure AI tools for Memoria persistent memory")]
struct Cli {
    /// Project directory (default: current)
    #[arg(long, default_value = ".")]
    dir: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Write MCP config + steering rules
    Init {
        /// Target tool: kiro, cursor, claude (repeatable; default: auto-detect)
        #[arg(long, value_name = "TOOL")]
        tool: Vec<String>,
        /// Database URL for embedded mode
        #[arg(long)]
        db_url: Option<String>,
        /// Memoria REST API URL for remote mode
        #[arg(long)]
        api_url: Option<String>,
        /// API token for remote mode
        #[arg(long)]
        token: Option<String>,
        /// Default user ID
        #[arg(long, default_value = "default")]
        user: String,
        /// Overwrite customized rule files
        #[arg(long)]
        force: bool,
        /// Embedding provider (openai, local)
        #[arg(long)]
        embedding_provider: Option<String>,
        /// Embedding model name
        #[arg(long)]
        embedding_model: Option<String>,
        /// Embedding dimension
        #[arg(long)]
        embedding_dim: Option<String>,
        /// Embedding API key
        #[arg(long)]
        embedding_api_key: Option<String>,
        /// Embedding API base URL
        #[arg(long)]
        embedding_base_url: Option<String>,
    },
    /// Show MCP config and rule version status
    Status,
    /// Update steering rules to latest version
    UpdateRules,
}

// ── MCP entry builder ─────────────────────────────────────────────────────────

fn mcp_entry(
    db_url: Option<&str>,
    api_url: Option<&str>,
    token: Option<&str>,
    user: &str,
    embedding_provider: Option<&str>,
    embedding_model: Option<&str>,
    embedding_dim: Option<&str>,
    embedding_api_key: Option<&str>,
    embedding_base_url: Option<&str>,
) -> serde_json::Value {
    let cmd = which_cmd("memoria-mcp-rs").unwrap_or_else(|| "memoria-mcp-rs".to_string());

    if let Some(url) = api_url {
        let mut args = vec![serde_json::json!("--api-url"), serde_json::json!(url)];
        if let Some(t) = token {
            args.push(serde_json::json!("--token"));
            args.push(serde_json::json!(t));
        }
        if user != "default" {
            args.push(serde_json::json!("--user"));
            args.push(serde_json::json!(user));
        }
        return serde_json::json!({"command": cmd, "args": args});
    }

    let db = db_url.unwrap_or("mysql://root:111@localhost:6001/memoria_rs");
    let mut args = vec![
        serde_json::json!("--db-url"), serde_json::json!(db),
    ];
    if user != "default" {
        args.push(serde_json::json!("--user"));
        args.push(serde_json::json!(user));
    }

    // Only include env vars the user actually provided; placeholder the rest
    let mut env = serde_json::Map::new();
    let fields: &[(&str, Option<&str>)] = &[
        ("EMBEDDING_PROVIDER", embedding_provider),
        ("EMBEDDING_BASE_URL", embedding_base_url),
        ("EMBEDDING_API_KEY", embedding_api_key),
        ("EMBEDDING_MODEL", embedding_model),
        ("EMBEDDING_DIM", embedding_dim),
    ];
    for &(key, val) in fields {
        if let Some(v) = val {
            env.insert(key.to_string(), serde_json::json!(v));
        }
    }
    let env = serde_json::Value::Object(env);

    let mut entry = serde_json::json!({"command": cmd, "args": args});
    if !env.as_object().unwrap().is_empty() {
        entry["env"] = env;
    }
    entry
}

fn which_cmd(name: &str) -> Option<String> {
    std::env::var("PATH").ok().and_then(|path| {
        for dir in path.split(':') {
            let p = Path::new(dir).join(name);
            if p.exists() { return Some(p.to_string_lossy().to_string()); }
        }
        None
    })
}

// ── Detection ─────────────────────────────────────────────────────────────────

fn detect_tools(project_dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    if project_dir.join(".kiro").is_dir() { found.push("kiro".to_string()); }
    if project_dir.join(".cursor").is_dir() || project_dir.join(".cursorrc").exists() {
        found.push("cursor".to_string());
    }
    if project_dir.join("CLAUDE.md").exists() || project_dir.join(".claude").is_dir() {
        found.push("claude".to_string());
    }
    found
}

// ── Write helpers ─────────────────────────────────────────────────────────────

fn installed_version(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let re = regex_version(&content)?;
    Some(re)
}

fn regex_version(content: &str) -> Option<String> {
    let marker = "memoria-version:";
    let pos = content.find(marker)?;
    let rest = content[pos + marker.len()..].trim();
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

fn write_rule(path: &Path, content: &str, force: bool, project_dir: &Path) -> String {
    let rel = path.strip_prefix(project_dir).unwrap_or(path).display().to_string();
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(path, content).ok();
        return format!("  ✅ {rel} (created)");
    }
    let installed = installed_version(path);
    let new_ver = regex_version(&content[..content.len().min(500)]);
    if installed.as_deref() == new_ver.as_deref() {
        return format!("  ⏭️  {rel} (up to date)");
    }
    if !force {
        if let Some(ref _iv) = installed {
            let bak = path.with_extension(format!("{}.bak", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
            if let Ok(existing) = std::fs::read_to_string(path) {
                std::fs::write(&bak, existing).ok();
            }
        }
    }
    std::fs::write(path, content).ok();
    let from = installed.as_deref().unwrap_or("?");
    let to = new_ver.as_deref().unwrap_or("?");
    format!("  ✅ {rel} (updated {from} → {to})")
}

fn write_mcp_json(path: &Path, entry: &serde_json::Value, project_dir: &Path) -> String {
    let rel = path.strip_prefix(project_dir).unwrap_or(path).display().to_string();
    let mut config: serde_json::Value = if path.exists() {
        std::fs::read_to_string(path).ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({"mcpServers": {}}))
    } else {
        serde_json::json!({"mcpServers": {}})
    };
    config["mcpServers"][MCP_KEY] = entry.clone();
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    let content = serde_json::to_string_pretty(&config).unwrap_or_default() + "\n";
    std::fs::write(path, content).ok();
    format!("  ✅ {rel}")
}

fn configure_kiro(project_dir: &Path, entry: &serde_json::Value, force: bool) -> Vec<String> {
    vec![
        write_mcp_json(&project_dir.join(".kiro/settings/mcp.json"), entry, project_dir),
        write_rule(&project_dir.join(".kiro/steering/memory.md"), KIRO_STEERING, force, project_dir),
    ]
}

fn configure_cursor(project_dir: &Path, entry: &serde_json::Value, force: bool) -> Vec<String> {
    vec![
        write_mcp_json(&project_dir.join(".cursor/mcp.json"), entry, project_dir),
        write_rule(&project_dir.join(".cursor/rules/memory.mdc"), CURSOR_RULE, force, project_dir),
    ]
}

fn configure_claude(project_dir: &Path, entry: &serde_json::Value, _force: bool) -> Vec<String> {
    let mut actions = vec![
        write_mcp_json(&project_dir.join(".mcp.json"), entry, project_dir),
    ];
    let claude_md = project_dir.join("CLAUDE.md");
    if claude_md.exists() {
        let existing = std::fs::read_to_string(&claude_md).unwrap_or_default();
        if existing.contains("memoria-version:") {
            actions.push("  ⏭️  CLAUDE.md (already configured)".to_string());
        } else {
            let new_content = format!("{}\n\n{}", existing.trim_end(), CLAUDE_RULE);
            std::fs::write(&claude_md, new_content).ok();
            actions.push("  ✅ CLAUDE.md (appended)".to_string());
        }
    } else {
        std::fs::write(&claude_md, CLAUDE_RULE).ok();
        actions.push("  ✅ CLAUDE.md (created)".to_string());
    }
    actions
}

// ── Commands ──────────────────────────────────────────────────────────────────

fn cmd_init(
    project_dir: &Path,
    tools: Vec<String>,
    db_url: Option<String>,
    api_url: Option<String>,
    token: Option<String>,
    user: String,
    force: bool,
    embedding_provider: Option<String>,
    embedding_model: Option<String>,
    embedding_dim: Option<String>,
    embedding_api_key: Option<String>,
    embedding_base_url: Option<String>,
) {
    let detected = if tools.is_empty() { detect_tools(project_dir) } else { tools };
    if detected.is_empty() {
        println!("No AI tools detected. Use --tool kiro|cursor|claude to specify.");
        return;
    }

    let entry = mcp_entry(
        db_url.as_deref(), api_url.as_deref(), token.as_deref(), &user,
        embedding_provider.as_deref(), embedding_model.as_deref(),
        embedding_dim.as_deref(), embedding_api_key.as_deref(), embedding_base_url.as_deref(),
    );

    for tool in &detected {
        println!("{tool}:");
        let actions = match tool.as_str() {
            "kiro" => configure_kiro(project_dir, &entry, force),
            "cursor" => configure_cursor(project_dir, &entry, force),
            "claude" => configure_claude(project_dir, &entry, force),
            _ => { println!("  ⚠️  Unknown tool: {tool}"); continue; }
        };
        for line in actions { println!("{line}"); }
    }
    println!("\nDone! Restart your AI tool to load the MCP server.");
}

fn cmd_status(project_dir: &Path) {
    let configs = [
        ("kiro",   ".kiro/settings/mcp.json",    ".kiro/steering/memory.md"),
        ("cursor", ".cursor/mcp.json",            ".cursor/rules/memory.mdc"),
        ("claude", ".mcp.json",                   "CLAUDE.md"),
    ];
    let mut found_any = false;
    for (tool, mcp_path, rule_path) in &configs {
        let mcp = project_dir.join(mcp_path);
        if !mcp.exists() { continue; }
        found_any = true;
        let has_mcp = std::fs::read_to_string(&mcp).ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .map(|v| v["mcpServers"].get(MCP_KEY).is_some())
            .unwrap_or(false);
        let ver = installed_version(&project_dir.join(rule_path));
        let mcp_status = if has_mcp { "✅" } else { "❌ not configured" };
        let rule_status = match ver.as_deref() {
            Some(v) if v == VERSION => format!("✅ v{v}"),
            Some(v) => format!("⚠️  outdated ({v})"),
            None => "❌ missing".to_string(),
        };
        println!("  {tool}: mcp={mcp_status}  rules={rule_status}");
    }
    if !found_any {
        println!("No configuration found. Run 'memoria-rs init' first.");
    }
}

fn cmd_update_rules(project_dir: &Path) {
    let rules = [
        ("kiro",   project_dir.join(".kiro/steering/memory.md"),    KIRO_STEERING),
        ("cursor", project_dir.join(".cursor/rules/memory.mdc"),    CURSOR_RULE),
        ("claude", project_dir.join("CLAUDE.md"),                   CLAUDE_RULE),
    ];
    let mut updated = 0;
    for (tool, path, content) in &rules {
        if path.exists() {
            std::fs::write(path, content).ok();
            println!("  ✅ {tool}: rules updated to v{VERSION}");
            updated += 1;
        }
    }
    if updated == 0 {
        println!("No rule files found. Run 'memoria-rs init' first.");
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();
    let project_dir = cli.dir.canonicalize().unwrap_or(cli.dir);

    match cli.command {
        Commands::Init {
            tool, db_url, api_url, token, user, force,
            embedding_provider, embedding_model, embedding_dim,
            embedding_api_key, embedding_base_url,
        } => {
            cmd_init(
                &project_dir, tool, db_url, api_url, token, user, force,
                embedding_provider, embedding_model, embedding_dim,
                embedding_api_key, embedding_base_url,
            );
        }
        Commands::Status => cmd_status(&project_dir),
        Commands::UpdateRules => cmd_update_rules(&project_dir),
    }
    Ok(())
}
