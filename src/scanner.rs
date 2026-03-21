use crate::estimator::WeightEstimator;
use crate::ir::{build_canonical_ir_from_parsed, CanonicalIrDocument};
use crate::parser::{ComponentParser, ParsedComponent};
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
            component.is_interactive = hints.is_interactive;
            component.effect_profile = parsed.effect_profile;
            component.source_hash = parsed.source_hash;

            let id = compiler.add_component(component);
            component_map.insert(parsed.name.clone(), id);
        }

        for parsed in &components {
            if let Some(&from_id) = component_map.get(&parsed.name) {
                for import in &parsed.imports {
                    if let Some(&to_id) = component_map.get(import) {
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

    pub fn scan_and_build_canonical_ir(&self, path: &Path) -> Result<CanonicalIrDocument> {
        let components = self.scan_directory(path)?;
        Ok(self.build_canonical_ir(&components))
    }

    fn is_component_file(&self, path: &Path) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

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
