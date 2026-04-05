use crate::parser::ParsedComponent;
use crate::runtime::eval::render_from_components_dir;
use crate::scanner::ProjectScanner;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct ShowcaseRenderRequest {
    pub components_root: PathBuf,
    pub entry_module: String,
    pub props_json: String,
    pub page_title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShowcaseTimings {
    pub scan_ms: f64,
    pub graph_build_ms: f64,
    pub optimize_ms: f64,
    pub render_ms: f64,
    pub total_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShowcaseGraphStats {
    pub total_components: usize,
    pub total_dependencies: usize,
    pub root_components: usize,
    pub leaf_components: usize,
    pub max_dependencies_per_component: usize,
    pub max_dependents_per_component: usize,
    pub critical_path_len: usize,
    pub parallel_batches: usize,
    pub total_weight_kb: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShowcaseDependencyHash {
    pub component_name: String,
    pub file_path: String,
    pub import_count: usize,
    pub resolved_dependency_count: usize,
    pub dependency_hash: String,
    pub imports: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShowcaseStats {
    pub timings: ShowcaseTimings,
    pub graph: ShowcaseGraphStats,
    pub dependency_hashes: Vec<ShowcaseDependencyHash>,
}

#[derive(Debug, Clone)]
pub struct ShowcaseArtifact {
    pub html_document: String,
    pub stats: ShowcaseStats,
}

pub fn build_showcase_artifact(
    request: &ShowcaseRenderRequest,
) -> Result<ShowcaseArtifact, Box<dyn std::error::Error + Send + Sync>> {
    let total_start = Instant::now();
    let props: Value = serde_json::from_str(&request.props_json)?;

    let scanner = ProjectScanner::new();
    let scan_start = Instant::now();
    let parsed_components = scanner.scan_directory(&request.components_root)?;
    let scan_ms = scan_start.elapsed().as_secs_f64() * 1000.0;

    let build_start = Instant::now();
    let compiler = scanner.build_compiler(parsed_components.clone());
    let graph_build_ms = build_start.elapsed().as_secs_f64() * 1000.0;

    let optimize_start = Instant::now();
    let optimization = compiler.optimize()?;
    let optimize_ms = optimize_start.elapsed().as_secs_f64() * 1000.0;

    let render_start = Instant::now();
    let rendered_html = render_from_components_dir(
        &request.components_root,
        request.entry_module.as_str(),
        &props,
    )?;
    let render_ms = render_start.elapsed().as_secs_f64() * 1000.0;

    let timings = ShowcaseTimings {
        scan_ms,
        graph_build_ms,
        optimize_ms,
        render_ms,
        total_ms: total_start.elapsed().as_secs_f64() * 1000.0,
    };
    let graph = build_graph_stats(&compiler, &optimization);
    let dependency_hashes = build_dependency_hashes(&parsed_components);

    let stats = ShowcaseStats {
        timings,
        graph,
        dependency_hashes,
    };

    let document = build_showcase_document(request, &props, &rendered_html, &stats)?;
    Ok(ShowcaseArtifact {
        html_document: document,
        stats,
    })
}

pub fn render_showcase_document(
    request: &ShowcaseRenderRequest,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    Ok(build_showcase_artifact(request)?.html_document)
}

fn build_showcase_document(
    request: &ShowcaseRenderRequest,
    props: &Value,
    rendered_html: &str,
    stats: &ShowcaseStats,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let title = escape_html_text(request.page_title.as_str());
    let props_script = serde_json::to_string_pretty(props)?;
    let stats_script = serde_json::to_string_pretty(stats)?;
    let root_dir = escape_html_text(request.components_root.to_string_lossy().as_ref());
    let entry = escape_html_text(request.entry_module.as_str());
    let hash_rows = stats
        .dependency_hashes
        .iter()
        .map(render_dependency_hash_row)
        .collect::<Vec<String>>()
        .join("\n");

    Ok(format!(
        "<!doctype html>\n\
<html lang=\"en\">\n\
<head>\n\
  <meta charset=\"utf-8\" />\n\
  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n\
  <title>{title}</title>\n\
  <style>\n\
    :root {{ color-scheme: light; }}\n\
    * {{ box-sizing: border-box; }}\n\
    body {{ margin: 0; font-family: \"Segoe UI\", sans-serif; background: #f5f7fb; color: #10131a; }}\n\
    .shell {{ max-width: 1200px; margin: 0 auto; padding: 24px; }}\n\
    .meta {{ margin-bottom: 18px; padding: 12px 14px; border-radius: 10px; background: #ffffff; border: 1px solid #d8dde8; }}\n\
    .meta h1 {{ margin: 0 0 8px 0; font-size: 20px; }}\n\
    .meta p {{ margin: 2px 0; font-size: 13px; color: #3f4a62; }}\n\
    .panel {{ margin-top: 16px; padding: 14px; border-radius: 12px; border: 1px solid #d8dde8; background: #ffffff; }}\n\
    .panel h2 {{ margin: 0 0 12px 0; font-size: 16px; }}\n\
    .preview {{ padding: 24px; border-radius: 12px; border: 1px solid #d8dde8; background: #ffffff; box-shadow: 0 8px 24px rgba(15, 23, 42, 0.07); }}\n\
    .stats-grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 10px; }}\n\
    .stat-card {{ padding: 10px; border-radius: 10px; border: 1px solid #d8dde8; background: #f8fafd; }}\n\
    .stat-card .label {{ display: block; font-size: 12px; color: #3f4a62; }}\n\
    .stat-card .value {{ display: block; margin-top: 3px; font-size: 18px; font-weight: 700; }}\n\
    .hash-table {{ width: 100%; border-collapse: collapse; font-size: 12px; }}\n\
    .hash-table th, .hash-table td {{ border-bottom: 1px solid #e5e9f2; padding: 8px; text-align: left; vertical-align: top; }}\n\
    .hash-table code {{ font-family: \"Consolas\", \"Monaco\", monospace; }}\n\
    .code-panel {{ margin-top: 12px; padding: 10px; border-radius: 10px; border: 1px solid #d8dde8; background: #f7f9fc; overflow: auto; }}\n\
    .code-panel code {{ font-family: \"Consolas\", \"Monaco\", monospace; font-size: 12px; white-space: pre; }}\n\
  </style>\n\
</head>\n\
<body>\n\
  <main class=\"shell\">\n\
    <section class=\"meta\">\n\
      <h1>{title}</h1>\n\
      <p><strong>Entry:</strong> {entry}</p>\n\
      <p><strong>Components root:</strong> {root_dir}</p>\n\
      <p><strong>Renderer:</strong> ALBEDO AST fallback (JSX/TSX source)</p>\n\
    </section>\n\
    <section class=\"panel\">\n\
      <h2>Performance Stats (ms)</h2>\n\
      <div class=\"stats-grid\">\n\
        <div class=\"stat-card\"><span class=\"label\">Scan Components</span><span class=\"value\">{:.2}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Build Graph</span><span class=\"value\">{:.2}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Optimize Graph</span><span class=\"value\">{:.2}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Render HTML</span><span class=\"value\">{:.2}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">End-to-End</span><span class=\"value\">{:.2}</span></div>\n\
      </div>\n\
    </section>\n\
    <section class=\"panel\">\n\
      <h2>Graph Snapshot</h2>\n\
      <div class=\"stats-grid\">\n\
        <div class=\"stat-card\"><span class=\"label\">Components</span><span class=\"value\">{}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Dependencies</span><span class=\"value\">{}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Critical Path</span><span class=\"value\">{}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Parallel Batches</span><span class=\"value\">{}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Root Components</span><span class=\"value\">{}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Leaf Components</span><span class=\"value\">{}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Max Dependencies</span><span class=\"value\">{}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Max Dependents</span><span class=\"value\">{}</span></div>\n\
        <div class=\"stat-card\"><span class=\"label\">Total Weight (KB)</span><span class=\"value\">{:.2}</span></div>\n\
      </div>\n\
    </section>\n\
    <section class=\"preview\" id=\"app-root\">\n\
{rendered_html}\n\
    </section>\n\
    <section class=\"panel\">\n\
      <h2>Dependency Hashes</h2>\n\
      <table class=\"hash-table\">\n\
        <thead>\n\
          <tr><th>Component</th><th>Resolved/Imports</th><th>Dependency Hash</th><th>Imports</th></tr>\n\
        </thead>\n\
        <tbody>\n\
{hash_rows}\n\
        </tbody>\n\
      </table>\n\
    </section>\n\
    <section class=\"panel\">\n\
      <h2>Data Payloads</h2>\n\
      <div class=\"code-panel\"><code id=\"initial-props\"></code></div>\n\
      <div class=\"code-panel\"><code id=\"metrics-json\"></code></div>\n\
    </section>\n\
  </main>\n\
  <script>\n\
    const props = {props_script};\n\
    const stats = {stats_script};\n\
    document.getElementById(\"initial-props\").textContent = JSON.stringify(props, null, 2);\n\
    document.getElementById(\"metrics-json\").textContent = JSON.stringify(stats, null, 2);\n\
  </script>\n\
</body>\n\
</html>\n"
        ,
        stats.timings.scan_ms,
        stats.timings.graph_build_ms,
        stats.timings.optimize_ms,
        stats.timings.render_ms,
        stats.timings.total_ms,
        stats.graph.total_components,
        stats.graph.total_dependencies,
        stats.graph.critical_path_len,
        stats.graph.parallel_batches,
        stats.graph.root_components,
        stats.graph.leaf_components,
        stats.graph.max_dependencies_per_component,
        stats.graph.max_dependents_per_component,
        stats.graph.total_weight_kb
    ))
}

fn escape_html_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn build_graph_stats(
    compiler: &crate::RenderCompiler,
    optimization: &crate::types::OptimizationResult,
) -> ShowcaseGraphStats {
    let graph = compiler.graph();
    let mut total_dependencies = 0usize;
    let mut root_components = 0usize;
    let mut leaf_components = 0usize;
    let mut max_dependencies_per_component = 0usize;
    let mut max_dependents_per_component = 0usize;

    for component_id in graph.component_ids() {
        let dependencies = graph.get_dependencies(&component_id);
        let dependents = graph.get_dependents(&component_id);
        let dependency_count = dependencies.len();
        let dependent_count = dependents.len();

        total_dependencies += dependency_count;
        if dependency_count == 0 {
            root_components += 1;
        }
        if dependent_count == 0 {
            leaf_components += 1;
        }

        max_dependencies_per_component = max_dependencies_per_component.max(dependency_count);
        max_dependents_per_component = max_dependents_per_component.max(dependent_count);
    }

    ShowcaseGraphStats {
        total_components: graph.len(),
        total_dependencies,
        root_components,
        leaf_components,
        max_dependencies_per_component,
        max_dependents_per_component,
        critical_path_len: optimization.critical_path.len(),
        parallel_batches: optimization.parallel_batches.len(),
        total_weight_kb: optimization.metrics.total_weight_kb,
    }
}

fn build_dependency_hashes(parsed_components: &[ParsedComponent]) -> Vec<ShowcaseDependencyHash> {
    let known_components = parsed_components
        .iter()
        .map(|component| component.name.clone())
        .collect::<HashSet<_>>();

    let mut output = parsed_components
        .iter()
        .map(|component| {
            let mut imports = component.imports.clone();
            imports.sort();
            imports.dedup();

            let resolved_dependency_count = imports
                .iter()
                .filter(|import_name| known_components.contains(import_name.as_str()))
                .count();

            let hash_basis = format!(
                "{}|{}|{}",
                normalize_path(component.file_path.as_str()),
                component.name,
                imports.join(",")
            );
            let dependency_hash = fnv1a_64_hex(hash_basis.as_bytes());

            ShowcaseDependencyHash {
                component_name: component.name.clone(),
                file_path: normalize_path(component.file_path.as_str()),
                import_count: imports.len(),
                resolved_dependency_count,
                dependency_hash,
                imports,
            }
        })
        .collect::<Vec<_>>();

    output.sort_by(|left, right| {
        left.file_path
            .cmp(&right.file_path)
            .then_with(|| left.component_name.cmp(&right.component_name))
    });
    output
}

fn render_dependency_hash_row(hash: &ShowcaseDependencyHash) -> String {
    let imports_joined = if hash.imports.is_empty() {
        "-".to_string()
    } else {
        hash.imports
            .iter()
            .map(|value| escape_html_text(value))
            .collect::<Vec<String>>()
            .join(", ")
    };

    format!(
        "          <tr><td><strong>{}</strong><br /><code>{}</code></td><td>{}/{}</td><td><code>{}</code></td><td>{}</td></tr>",
        escape_html_text(hash.component_name.as_str()),
        escape_html_text(hash.file_path.as_str()),
        hash.resolved_dependency_count,
        hash.import_count,
        escape_html_text(hash.dependency_hash.as_str()),
        imports_joined
    )
}

fn normalize_path(value: &str) -> String {
    value.replace('\\', "/")
}

fn fnv1a_64_hex(input: &[u8]) -> String {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let mut hash = OFFSET_BASIS;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_render_showcase_document_supports_tsx_files() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("Button.tsx"),
            "export default function Button(props) { return <button>{props.label}</button>; }",
        )
        .unwrap();
        fs::write(
            temp.path().join("App.tsx"),
            "import Button from './Button'; export default function App(props) { return <main><h1>{props.title}</h1><Button label={props.cta} /></main>; }",
        )
        .unwrap();

        let request = ShowcaseRenderRequest {
            components_root: temp.path().to_path_buf(),
            entry_module: "App.tsx".to_string(),
            props_json: r#"{"title":"ALBEDO","cta":"Launch"}"#.to_string(),
            page_title: "TSX Showcase".to_string(),
        };
        let artifact = build_showcase_artifact(&request).unwrap();
        let html = artifact.html_document;

        assert!(html.contains("<h1>ALBEDO</h1>"));
        assert!(html.contains("<button>Launch</button>"));
        assert!(html.contains("TSX Showcase"));
        assert!(html.contains("Performance Stats"));
        assert!(html.contains("Dependency Hashes"));
        assert!(artifact.stats.graph.total_components >= 2);
        assert!(artifact.stats.dependency_hashes.len() >= 2);
        assert!(artifact
            .stats
            .dependency_hashes
            .iter()
            .all(|hash| hash.dependency_hash.len() == 16));
    }
}
