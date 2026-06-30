use crate::estimator::WeightEstimator;
use crate::ir::{build_canonical_ir_from_parsed, CanonicalIrDocument};
use crate::parser::{ComponentParser, ParsedComponent};
use crate::runtime::eval::component::{import_candidates, normalize_specifier};
use crate::types::*;
use crate::RenderCompiler;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct ProjectScanner {
    parser: ComponentParser,
    estimator: WeightEstimator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    Lenient,
    Strict,
}

#[derive(Debug, Clone)]
pub struct ScanFailure {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ScanReport {
    pub components: Vec<ParsedComponent>,
    pub failures: Vec<ScanFailure>,
}

impl ProjectScanner {
    pub fn new() -> Self {
        Self {
            parser: ComponentParser::new(),
            estimator: WeightEstimator::new(),
        }
    }

    pub fn scan_directory(&self, path: &Path) -> Result<Vec<ParsedComponent>> {
        let report = self.scan_directory_with_mode(path, ScanMode::Lenient)?;
        for failure in &report.failures {
            eprintln!(
                "Warning: Failed to parse {:?}: {}",
                failure.path, failure.message
            );
        }
        Ok(report.components)
    }

    pub fn scan_directory_with_mode(&self, path: &Path, mode: ScanMode) -> Result<ScanReport> {
        let mut components = Vec::new();
        let mut failures = Vec::new();

        for entry in WalkDir::new(path)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let file_path = entry.path();

            if file_path.is_file() && self.is_component_file(file_path) {
                match self.parser.parse_file(file_path) {
                    Ok(mut comps) => components.append(&mut comps),
                    Err(err) => failures.push(ScanFailure {
                        path: file_path.to_path_buf(),
                        message: err.to_string(),
                    }),
                }
            }
        }

        if matches!(mode, ScanMode::Strict) && !failures.is_empty() {
            let mut detail = String::new();
            for failure in &failures {
                detail.push_str(
                    format!("\n- {}: {}", failure.path.display(), failure.message).as_str(),
                );
            }
            return Err(CompilerError::AnalysisFailed(format!(
                "strict scan rejected {} parse failure(s) under '{}':{}",
                failures.len(),
                path.display(),
                detail
            )));
        }

        Ok(ScanReport {
            components,
            failures,
        })
    }

    pub fn build_compiler(&self, components: Vec<ParsedComponent>) -> RenderCompiler {
        let mut compiler = RenderCompiler::new();
        let mut component_map: HashMap<String, ComponentId> = HashMap::new();

        for parsed in &components {
            let mut component = Component::new(ComponentId::new(0), parsed.name.clone());

            component.weight = self.estimator.estimate(parsed);
            component.bitrate = self.estimator.estimate_bitrate(parsed);
            component.file_path = parsed.file_path.clone();
            component.line_number = parsed.line_number;

            let hints = self.estimator.estimate_priority_hints(parsed);
            component.is_above_fold = hints.is_above_fold;
            component.is_lcp_candidate = hints.is_lcp_candidate;
            component.is_interactive = parsed.is_interactive;
            component.is_client_interactive = parsed.is_client_interactive;
            component.effect_profile = parsed.effect_profile;
            component.source_hash = parsed.source_hash;
            component.is_module_only = parsed.is_module_only;

            let id = compiler.add_component(component);
            component_map.insert(parsed.name.clone(), id);
        }

        // Path-keyed index of every node by its normalized module path. This is
        // what lets a component depend on a *non-component* module (data/util)
        // it imports by a name that matches no component name.
        let mut path_map: HashMap<String, ComponentId> = HashMap::new();
        for parsed in &components {
            if let Some(&id) = component_map.get(&parsed.name) {
                path_map.insert(normalize_specifier(&parsed.file_path), id);
            }
        }

        for parsed in &components {
            let Some(&from_id) = component_map.get(&parsed.name) else {
                continue;
            };

            // Primary: resolve each import specifier to a file PATH relative to
            // the importer, then look it up in the path-keyed index. Catches
            // data/util imports the name match below cannot.
            for source in &parsed.import_sources {
                if let Some(to_id) = resolve_import_to_id(&parsed.file_path, source, &path_map) {
                    if to_id != from_id {
                        compiler.add_dependency(from_id, to_id).ok();
                    }
                }
            }

            // Fallback: the legacy name match (import binding name == component
            // name). Kept for bare/aliased specifiers that don't resolve to a
            // project file path; union with the path edges is idempotent.
            for import in &parsed.imports {
                if let Some(&to_id) = component_map.get(import) {
                    compiler.add_dependency(from_id, to_id).ok();
                }
            }
        }

        compiler
    }

    pub fn build_canonical_ir(&self, components: &[ParsedComponent]) -> CanonicalIrDocument {
        build_canonical_ir_from_parsed(components)
    }

    pub fn scan_and_build(&self, path: &Path) -> Result<RenderCompiler> {
        let components = self.scan_directory(path)?;
        Ok(self.build_compiler(components))
    }

    pub fn scan_and_build_canonical_ir(&self, path: &Path) -> Result<CanonicalIrDocument> {
        let components = self.scan_directory(path)?;
        Ok(self.build_canonical_ir(&components))
    }

    fn is_component_file(&self, path: &Path) -> bool {
        // Phase P · post-P wire-through — skip ambient TS declaration
        // files. Same exclusion is mirrored in
        // `runtime::eval::component::is_component_module` so both
        // the scanner and the renderer see the same set of files.
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(".d.ts") || name.ends_with(".d.tsx") {
            return false;
        }
        if let Some(ext) = path.extension() {
            let ext_str = ext.to_str().unwrap_or("");
            matches!(ext_str, "jsx" | "tsx" | "js" | "ts")
        } else {
            false
        }
    }
}

impl Default for ProjectScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve one import specifier (`from "..."`) against the importer's file path
/// to a node id, probing the same extension/`index` candidates the runtime
/// module loader uses. Returns `None` for bare/npm specifiers (no leading `.`)
/// and for paths that match no project node.
fn resolve_import_to_id(
    importer_file_path: &str,
    source: &str,
    path_map: &HashMap<String, ComponentId>,
) -> Option<ComponentId> {
    // Only project-relative specifiers resolve to a file path; bare specifiers
    // (`zod`, `@/x`) are npm/alias and fall to the name-based fallback.
    if !(source.starts_with("./") || source.starts_with("../")) {
        return None;
    }

    let importer_dir = Path::new(importer_file_path).parent().unwrap_or(Path::new(""));
    let base = normalize_specifier(importer_dir.join(source));

    import_candidates(&base)
        .into_iter()
        .find_map(|candidate| path_map.get(&candidate).copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ComponentParser;
    use std::fs;
    use std::io::Write;

    /// A component importing a non-component data module by a name that matches
    /// no component (`getIssue`) must still get a dependency edge — resolved by
    /// PATH against the data module's module-only node. This is the exact
    /// Halation P2 shape (route → `content/essays`).
    #[test]
    fn build_compiler_wires_data_module_dep_by_path() {
        let parser = ComponentParser::new();

        let route_src = r#"
            import { getIssue } from "../content/essays";
            export default function Issue() {
                return <main>{getIssue().title}</main>;
            }
        "#;
        let data_src = r#"
            export const essays = [{ slug: "a", title: "A" }];
            export function getIssue() { return essays[0]; }
        "#;

        let mut parsed = parser
            .parse_source(route_src, "src/routes/index.tsx")
            .unwrap();
        parsed.extend(parser.parse_source(data_src, "src/content/essays.ts").unwrap());

        let scanner = ProjectScanner::new();
        let compiler = scanner.build_compiler(parsed);
        let graph = compiler.graph();

        let issue = graph
            .components()
            .into_iter()
            .find(|c| c.name == "Issue")
            .expect("Issue node present");
        let essays = graph
            .components()
            .into_iter()
            .find(|c| c.is_module_only)
            .expect("essays module-only node present");

        assert!(
            graph.get_dependencies(&issue.id).contains(&essays.id),
            "Issue must depend on the essays data module (path-resolved edge)"
        );
    }

    #[test]
    fn test_scan_directory() {
        let temp_dir = std::env::temp_dir().join("test_scan");
        fs::create_dir_all(&temp_dir).ok();

        let test_file = temp_dir.join("Button.jsx");
        let mut file = fs::File::create(&test_file).unwrap();
        writeln!(
            file,
            "function Button() {{ return <button>Click</button>; }}"
        )
        .unwrap();

        let scanner = ProjectScanner::new();
        let components = scanner.scan_directory(&temp_dir).unwrap();

        assert!(!components.is_empty());

        fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_is_component_file() {
        let scanner = ProjectScanner::new();

        assert!(scanner.is_component_file(Path::new("Button.jsx")));
        assert!(scanner.is_component_file(Path::new("App.tsx")));
        assert!(!scanner.is_component_file(Path::new("style.css")));
        assert!(!scanner.is_component_file(Path::new("README.md")));
    }

    #[test]
    fn test_scan_directory_with_mode_lenient_collects_failures() {
        let temp_dir = std::env::temp_dir().join("test_scan_lenient");
        fs::create_dir_all(&temp_dir).ok();

        let valid_file = temp_dir.join("App.jsx");
        let mut valid = fs::File::create(&valid_file).unwrap();
        writeln!(
            valid,
            "export default function App() {{ return <main>ok</main>; }}"
        )
        .unwrap();

        let invalid_file = temp_dir.join("Broken.jsx");
        let mut invalid = fs::File::create(&invalid_file).unwrap();
        writeln!(
            invalid,
            "export default function Broken() {{ return <main>; }}"
        )
        .unwrap();

        let scanner = ProjectScanner::new();
        let report = scanner
            .scan_directory_with_mode(&temp_dir, ScanMode::Lenient)
            .unwrap();

        assert_eq!(report.failures.len(), 1);
        assert!(!report.components.is_empty());

        fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_scan_directory_with_mode_strict_fails_on_parse_error() {
        let temp_dir = std::env::temp_dir().join("test_scan_strict");
        fs::create_dir_all(&temp_dir).ok();

        let invalid_file = temp_dir.join("Broken.jsx");
        let mut invalid = fs::File::create(&invalid_file).unwrap();
        writeln!(
            invalid,
            "export default function Broken() {{ return <main>; }}"
        )
        .unwrap();

        let scanner = ProjectScanner::new();
        let err = scanner
            .scan_directory_with_mode(&temp_dir, ScanMode::Strict)
            .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("strict scan rejected"));
        assert!(message.contains("Broken.jsx"));

        fs::remove_dir_all(&temp_dir).ok();
    }
}
