use anyhow::{Context, Result};
use clap::Parser as ClapParser;
use colored::*;
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_yaml::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Language, Node, Parser};

#[derive(ClapParser, Debug)]
#[command(name = "scalelint")]
#[command(about = "Cloud-Native Static Scalability Scanner for CI/CD", long_about = None)]
struct Cli {
    /// Target directory or file to scan
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Output format: text, json, or sarif
    #[arg(long, default_value = "text")]
    format: String,

    /// Path to configuration file
    #[arg(long, default_value = "scalelint.yaml")]
    config: PathBuf,

    /// Exit with code 0 even if scalability issues are found (useful for audit-only mode)
    #[arg(long, default_value_t = false)]
    no_fail: bool,
}

#[derive(Deserialize, Debug)]
struct ScaleConfig {
    #[serde(default = "default_max_db_connections")]
    max_db_connections: u64,
    #[serde(default = "default_db_pool_size")]
    db_pool_size: u64,
    #[serde(default)]
    disabled_rules: Vec<String>,
}

fn default_max_db_connections() -> u64 {
    500
}
fn default_db_pool_size() -> u64 {
    10
}

impl Default for ScaleConfig {
    fn default() -> Self {
        Self {
            max_db_connections: default_max_db_connections(),
            db_pool_size: default_db_pool_size(),
            disabled_rules: Vec::new(),
        }
    }
}

#[derive(Serialize, Debug)]
struct Finding {
    rule_id: &'static str,
    severity: &'static str,
    file: String,
    line: usize,
    column: usize,
    message: String,
    snippet: String,
}

#[derive(Default, Debug)]
struct ProjectContext {
    indexed_fields: HashMap<String, HashSet<String>>,
    config: ScaleConfig,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut findings: Vec<Finding> = Vec::new();

    let config = if cli.config.exists() {
        let content = fs::read_to_string(&cli.config)?;
        serde_yaml::from_str(&content).unwrap_or_default()
    } else {
        ScaleConfig::default()
    };

    let mut ctx = ProjectContext {
        indexed_fields: HashMap::new(),
        config,
    };

    let mut ts_files = Vec::new();
    let mut yaml_files = Vec::new();

    // Pass 1: Discover files and index database schemas
    let walker = WalkBuilder::new(&cli.path).standard_filters(true).build();
    for result in walker.flatten() {
        let path = result.path().to_path_buf();
        if !path.is_file() {
            continue;
        }
        if path.file_name().and_then(|s| s.to_str()) == Some("scalelint.yaml") {
            continue;
        }
        match path.extension().and_then(|s| s.to_str()) {
            Some("prisma") => parse_prisma_schema(&path, &mut ctx)?,
            Some("ts") => ts_files.push(path),
            Some("yaml") | Some("yml") => yaml_files.push(path),
            _ => {}
        }
    }

    // Pass 2: Scan Application ASTs
    for path in &ts_files {
        scan_typescript_file(path, &ctx, &mut findings)?;
    }

    // Pass 3: Scan Kubernetes & KEDA Manifests
    for path in &yaml_files {
        scan_k8s_manifest(path, &ctx, &mut findings)?;
    }

    // Filter out disabled rules from scalelint.yaml
    findings.retain(|f| !ctx.config.disabled_rules.iter().any(|r| r == f.rule_id));

    // Render Output Format
    match cli.format.as_str() {
        "json" => println!("{}", serde_json::to_string_pretty(&findings)?),
        "sarif" => println!("{}", generate_sarif(&findings)?),
        _ => print_human_readable(&findings),
    }

    if !findings.is_empty() && !cli.no_fail {
        std::process::exit(1);
    }

    Ok(())
}

/// Checks if the current line or the line immediately above has `scalelint-ignore`
fn is_line_ignored(lines: &[&str], row: usize, rule_id: &str) -> bool {
    let check_comment = |line: &str| -> bool {
        if !line.contains("scalelint-ignore") {
            return false;
        }
        // Ignore all rules on this line if no specific rule ID is mentioned, or match specific rule_id
        !line.contains("SCALE-") || line.contains(rule_id)
    };

    if let Some(current) = lines.get(row) {
        if check_comment(current) {
            return true;
        }
    }
    if row > 0 {
        if let Some(prev) = lines.get(row - 1) {
            if check_comment(prev) {
                return true;
            }
        }
    }
    false
}

fn parse_prisma_schema(path: &Path, ctx: &mut ProjectContext) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let model_re = Regex::new(r"model\s+(\w+)\s*\{").unwrap();
    let field_index_re = Regex::new(r"^\s*(\w+)\s+\w+.*(@id|@unique)").unwrap();
    let block_index_re = Regex::new(r"@@(index|unique)\(\[([^\]]+)\]\)").unwrap();

    let mut current_model: Option<String> = None;

    for line in content.lines() {
        if let Some(cap) = model_re.captures(line) {
            let model_name = cap[1].to_lowercase();
            ctx.indexed_fields.entry(model_name.clone()).or_default();
            current_model = Some(model_name);
        } else if line.trim() == "}" {
            current_model = None;
        } else if let Some(ref model) = current_model {
            if let Some(cap) = field_index_re.captures(line) {
                ctx.indexed_fields
                    .get_mut(model)
                    .unwrap()
                    .insert(cap[1].to_string());
            }
            if let Some(cap) = block_index_re.captures(line) {
                for field in cap[2].split(',') {
                    let clean = field.trim().to_string();
                    ctx.indexed_fields.get_mut(model).unwrap().insert(clean);
                }
            }
        }
    }
    Ok(())
}

fn scan_typescript_file(
    path: &Path,
    ctx: &ProjectContext,
    findings: &mut Vec<Finding>,
) -> Result<()> {
    let source_code = fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;

    let mut parser = Parser::new();
    let language: Language = tree_sitter_typescript::language_typescript().into();
    parser.set_language(&language)?;

    let tree = parser
        .parse(&source_code, None)
        .context("Failed to parse AST")?;
    let lines: Vec<&str> = source_code.lines().collect();

    let where_re = Regex::new(r"where\s*:\s*\{\s*(\w+)\s*:").unwrap();
    walk_ast(
        tree.root_node(),
        false,
        false,
        &source_code,
        &lines,
        path,
        ctx,
        &where_re,
        findings,
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_ast(
    node: Node,
    inside_loop: bool,
    inside_function: bool,
    source: &str,
    lines: &[&str],
    path: &Path,
    ctx: &ProjectContext,
    where_re: &Regex,
    findings: &mut Vec<Finding>,
) {
    let kind = node.kind();
    let is_loop = matches!(
        kind,
        "for_statement" | "for_in_statement" | "while_statement" | "do_statement"
    );
    let is_fn = matches!(
        kind,
        "function_declaration" | "arrow_function" | "method_definition"
    );

    let now_inside_loop = inside_loop || is_loop;
    let now_inside_fn = inside_function || is_fn;

    // SCALE-001: Awaited DB/network call inside a loop
    if now_inside_loop && kind == "await_expression" {
        let text = node.utf8_text(source.as_bytes()).unwrap_or("");
        let start = node.start_position();
        if is_database_or_network_call(text) && !is_line_ignored(lines, start.row, "SCALE-001") {
            findings.push(Finding {
                rule_id: "SCALE-001",
                severity: "CRITICAL",
                file: path.display().to_string(),
                line: start.row + 1,
                column: start.column + 1,
                message: "N+1 Bottleneck: Awaited DB/network call inside a loop will exhaust connections during traffic spikes.".to_string(),
                snippet: lines.get(start.row).unwrap_or(&"").trim().to_string(),
            });
        }
    }

    // SCALE-005: Module-level in-memory state (new Map / new Set outside functions)
    if !now_inside_fn && kind == "new_expression" {
        let text = node.utf8_text(source.as_bytes()).unwrap_or("");
        let start = node.start_position();
        if (text.starts_with("new Map") || text.starts_with("new Set"))
            && !is_line_ignored(lines, start.row, "SCALE-005")
        {
            findings.push(Finding {
                rule_id: "SCALE-005",
                severity: "HIGH",
                file: path.display().to_string(),
                line: start.row + 1,
                column: start.column + 1,
                message: "Stateful Pod Anti-Pattern: Global in-memory Map/Set causes split-brain state when K8s/KEDA scales horizontally to multiple pods. Use Redis instead.".to_string(),
                snippet: lines.get(start.row).unwrap_or(&"").trim().to_string(),
            });
        }
    }

    // SCALE-002 & SCALE-003: Unbounded .findMany() and Unindexed WHERE clauses
    if kind == "call_expression" {
        let text = node.utf8_text(source.as_bytes()).unwrap_or("");
        if text.contains("prisma.") && text.contains(".findMany(") {
            let start = node.start_position();

            if !text.contains("take:")
                && !text.contains("take :")
                && !is_line_ignored(lines, start.row, "SCALE-002")
            {
                findings.push(Finding {
                    rule_id: "SCALE-002",
                    severity: "HIGH",
                    file: path.display().to_string(),
                    line: start.row + 1,
                    column: start.column + 1,
                    message: "Unbounded Query: .findMany() without a 'take' limit can cause Out-Of-Memory (OOMKilled) crashes as table rows grow.".to_string(),
                    snippet: lines.get(start.row).unwrap_or(&"").trim().to_string(),
                });
            }

            let parts: Vec<&str> = text.split('.').collect();
            if parts.len() >= 3 {
                let model_name = parts[1].trim().to_lowercase();
                if let Some(indexed_set) = ctx.indexed_fields.get(&model_name) {
                    if let Some(cap) = where_re.captures(text) {
                        let queried_field = &cap[1];
                        if !indexed_set.contains(queried_field)
                            && !is_line_ignored(lines, start.row, "SCALE-003")
                        {
                            findings.push(Finding {
                                rule_id: "SCALE-003",
                                severity: "CRITICAL",
                                file: path.display().to_string(),
                                line: start.row + 1,
                                column: start.column + 1,
                                message: format!(
                                    "Full Table Scan Risk: Filtering model '{}' on unindexed column '{}' will lock the database at scale. Add @@index([{}]) to schema.prisma.",
                                    model_name, queried_field, queried_field
                                ),
                                snippet: lines.get(start.row).unwrap_or(&"").trim().to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_ast(
            child,
            now_inside_loop,
            now_inside_fn,
            source,
            lines,
            path,
            ctx,
            where_re,
            findings,
        );
    }
}

fn scan_k8s_manifest(
    path: &Path,
    ctx: &ProjectContext,
    findings: &mut Vec<Finding>,
) -> Result<()> {
    let content = fs::read_to_string(path)?;
    for doc in serde_yaml::Deserializer::from_str(&content) {
        let val = match Value::deserialize(doc) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let kind = val.get("kind").and_then(|k| k.as_str()).unwrap_or("");

        if kind == "ScaledObject" || kind == "HorizontalPodAutoscaler" {
            let max_replicas = val
                .get("spec")
                .and_then(|s| s.get("maxReplicaCount").or_else(|| s.get("maxReplicas")))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            let total_connections = max_replicas * ctx.config.db_pool_size;
            if total_connections > ctx.config.max_db_connections {
                findings.push(Finding {
                    rule_id: "SCALE-004",
                    severity: "HIGH",
                    file: path.display().to_string(),
                    line: 1,
                    column: 1,
                    message: format!(
                        "K8s/KEDA Connection Storm: maxReplicas ({}) × pool_size ({}) = {} DB connections, exceeding configured max_db_connections ({}) in scalelint.yaml.",
                        max_replicas,
                        ctx.config.db_pool_size,
                        total_connections,
                        ctx.config.max_db_connections
                    ),
                    snippet: format!("kind: {} (maxReplicas: {})", kind, max_replicas),
                });
            }
        }
    }
    Ok(())
}

fn generate_sarif(findings: &[Finding]) -> Result<String> {
    let results: Vec<_> = findings
        .iter()
        .map(|f| {
            json!({
                "ruleId": f.rule_id,
                "level": if f.severity == "CRITICAL" { "error" } else { "warning" },
                "message": { "text": f.message },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": f.file.trim_start_matches("./") },
                        "region": {
                            "startLine": f.line,
                            "startColumn": f.column
                        }
                    }
                }]
            })
        })
        .collect();

    let sarif = json!({
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "ScaleLint",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": "https://github.com/scalelint/scalelint"
                }
            },
            "results": results
        }]
    });

    Ok(serde_json::to_string_pretty(&sarif)?)
}

fn is_database_or_network_call(call_text: &str) -> bool {
    let indicators = [
        "prisma.", "db.", "knex.", "Repository.", "Model.", "axios.", "fetch",
        ".findUnique", ".findFirst", ".findMany", ".query", ".execute", ".save",
    ];
    indicators.iter().any(|ind| call_text.contains(ind))
}

fn print_human_readable(findings: &[Finding]) {
    if findings.is_empty() {
        println!("{}", "✅ No scalability anti-patterns detected!".green().bold());
        return;
    }

    println!(
        "\n{} {}\n",
        "🚨 ScaleLint detected".red().bold(),
        format!("{} scalability issue(s):", findings.len()).red().bold()
    );

    for f in findings {
        println!(
            "{} [{}] {}:{}:{}",
            f.severity.on_red().white().bold(),
            f.rule_id.cyan().bold(),
            f.file.underline(),
            f.line,
            f.column
        );
        println!("  {} {}", "↳ Reason:".yellow(), f.message);
        println!("  {} {}\n", "↳ Code:  ".bright_black(), f.snippet.bright_white());
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inline_ignore_detection() {
        let lines = vec![
            "// scalelint-ignore SCALE-005",
            "const cache = new Map();",
            "const other = new Set(); // scalelint-ignore",
        ];
        assert!(is_line_ignored(&lines, 1, "SCALE-005"));
        assert!(!is_line_ignored(&lines, 1, "SCALE-001"));
        assert!(is_line_ignored(&lines, 2, "SCALE-005"));
    }

    #[test]
    fn test_database_call_recognition() {
        assert!(is_database_or_network_call("prisma.order.findUnique({ where: { id } })"));
        assert!(is_database_or_network_call("axios.post('https://api.internal')"));
        assert!(!is_database_or_network_call("Math.max(1, 2)"));
    }

    #[test]
    fn test_end_to_end_examples_scan() -> Result<()> {
        let mut findings = Vec::new();
        let mut ctx = ProjectContext::default();
        parse_prisma_schema(Path::new("examples/schema.prisma"), &mut ctx)?;
        scan_typescript_file(Path::new("examples/order_service.ts"), &ctx, &mut findings)?;
        scan_k8s_manifest(Path::new("examples/keda-autoscaler.yaml"), &ctx, &mut findings)?;

        // Should detect 5 issues (since SCALE-005 is ignored via comment)
        assert_eq!(findings.len(), 5);
        assert!(findings.iter().any(|f| f.rule_id == "SCALE-001"));
        assert!(findings.iter().any(|f| f.rule_id == "SCALE-003"));
        assert!(findings.iter().any(|f| f.rule_id == "SCALE-004"));
        Ok(())
    }
}
