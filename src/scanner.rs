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

        // Sorted: components are numbered in scan order, and a directory's
        // listing order belongs to the filesystem (NTFS alphabetical, ext4 hash
        // order). Unsorted, one project got different component ids on Windows
        // and Linux — the golden manifest test failed on ubuntu/macos CI only.
        for entry in WalkDir::new(path)
            .follow_links(true)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let file_path = entry.path();

            // 15.6 · the root-level middleware is a request hook, not a
            // component. Its default export is a function, so the parser would
            // otherwise list it as one and tier it — `B middleware  imports npm`
            // in the build report, and a manifest entry for a thing that is
            // never rendered. Only this scan skips it: `ComponentProject` still
            // loads the file, because that is where its declaration is read.
            //
            // 15.5 · `jobs.ts` is the same shape and was missed the same way —
            // it built as `B __module__jobs`, so the file was tiered, bundled
            // and never run. **Any root entry the framework runs itself belongs
            // in this list**; the next one will be missed too if it is added
            // anywhere else.
            let is_framework_entry = file_path.parent() == Some(path)
                && file_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        crate::middleware::declare::ENTRY_NAMES.contains(&name)
                            || crate::jobs::declare::ENTRY_NAMES.contains(&name)
                    });

            if file_path.is_file() && !is_framework_entry && self.is_component_file(file_path) {
                // Through `read_module_source`, not `parse_file`: a Markdown page
                // is parsed as the JSX module it lowers to, and a page that will
                // not lower is a failure here like any other unparseable file.
                let parsed = crate::markdown::read_module_source(file_path)
                    .map_err(|err| CompilerError::AnalysisFailed(err.to_string()))
                    .and_then(|source| {
                        self.parser
                            .parse_source(&source, file_path.to_str().unwrap_or("unknown"))
                    });
                match parsed {
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
        // T-0045 · each parsed file's own id, in `components` order. The name
        // map above holds only the LAST component per name, so two modules
        // exporting `Page` (or `Counter`) used to collapse into one: every
        // `Page`'s import edges landed on the last `Page`, and the path index
        // filed `components/a/Counter.tsx` under some other `Counter`.
        let mut ids: Vec<ComponentId> = Vec::with_capacity(components.len());
        let mut name_count: HashMap<String, usize> = HashMap::new();

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
            component.state_escapes = parsed.state_escapes;
            component.reads_principal = parsed.reads_principal;
            component.reads_route_params = parsed.reads_route_params;
            component.imports_npm =
                crate::parser::imports_unresolvable_specifier(&parsed.import_sources);
            component.effect_profile = parsed.effect_profile;
            component.source_hash = parsed.source_hash;
            component.is_module_only = parsed.is_module_only;

            let id = compiler.add_component(component);
            component_map.insert(parsed.name.clone(), id);
            *name_count.entry(parsed.name.clone()).or_default() += 1;
            ids.push(id);
        }

        // Path-keyed index of every node by its normalized module path. This is
        // what lets a component depend on a *non-component* module (data/util)
        // it imports by a name that matches no component name.
        let mut path_map: HashMap<String, ComponentId> = HashMap::new();
        for (parsed, &id) in components.iter().zip(&ids) {
            path_map.insert(normalize_specifier(&parsed.file_path), id);
        }

        for (parsed, &from_id) in components.iter().zip(&ids) {

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
            // T-0045: only when the name is unique — a shared name would be a
            // guess, and a wrong edge is worse than a missing one (the path
            // edges above already cover every relative import).
            for import in &parsed.imports {
                if name_count.get(import).copied() != Some(1) {
                    continue;
                }
                if let Some(&to_id) = component_map.get(import) {
                    if to_id != from_id {
                        compiler.add_dependency(from_id, to_id).ok();
                    }
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

    fn is_component_file(&self, path: &Path) -> bool {
        // Phase P · post-P wire-through — skip ambient TS declaration
        // files. Same exclusion is mirrored in
        // `runtime::eval::component::is_component_module` so both
        // the scanner and the renderer see the same set of files.
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(".d.ts") || name.ends_with(".d.tsx") {
            return false;
        }
        // A test file is not a component — see `crate::testing::is_test_module`.
        if crate::testing::is_test_module(path) {
            return false;
        }
        if crate::markdown::is_markdown_page(path) {
            return true;
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
        // Markdown is a component only as a page — under `routes/`.
        assert!(scanner.is_component_file(Path::new("src/routes/docs/intro.md")));
        assert!(!scanner.is_component_file(Path::new("src/components/NOTES.md")));
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

    /// T-0045 · two route modules both exporting `Page`, each importing its own
    /// `Counter` (a third `Counter` exists elsewhere). Each `Page` must depend
    /// on exactly the `Counter` its file imports. Before: the name-keyed map
    /// collapsed same-named components, so one `Page` had no edges and the
    /// other had the wrong `Counter`.
    #[test]
    fn same_named_components_keep_their_own_dependency_edges() {
        let parser = ComponentParser::new();
        let counter = |label: &str| {
            format!(
                "export default function Counter() {{ return <button>{label}</button>; }}"
            )
        };
        let page = |path: &str| {
            format!(
                "import Counter from \"{path}\";\nexport default function Page() {{ return <main><Counter /></main>; }}"
            )
        };
        let mut parsed = Vec::new();
        for (source, file) in [
            (counter("a"), "src/components/a/Counter.tsx"),
            (counter("b"), "src/components/b/Counter.tsx"),
            (counter("root"), "src/components/Counter.tsx"),
            (page("../components/a/Counter"), "src/routes/islanda.tsx"),
            (page("../components/b/Counter"), "src/routes/islandb.tsx"),
        ] {
            parsed.extend(parser.parse_source(&source, file).unwrap());
        }

        let compiler = ProjectScanner::new().build_compiler(parsed);
        let graph = compiler.graph();
        let node = |file: &str| {
            graph
                .components()
                .into_iter()
                .find(|c| c.file_path == file)
                .unwrap_or_else(|| panic!("{file} has a node"))
        };
        for (route, own) in [
            ("src/routes/islanda.tsx", "src/components/a/Counter.tsx"),
            ("src/routes/islandb.tsx", "src/components/b/Counter.tsx"),
        ] {
            let deps: Vec<String> = graph
                .get_dependencies(&node(route).id)
                .into_iter()
                .filter_map(|id| graph.get(&id))
                .map(|c| c.file_path)
                .collect();
            assert_eq!(deps, vec![own.to_string()], "{route} depends on {deps:?}");
        }
    }
}
