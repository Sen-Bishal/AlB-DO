use crate::parser::ParsedComponent;
use std::path::Path;

pub struct WeightEstimator {
    base_component_weight: f64,
    import_weight: f64,
    prop_weight: f64,
}

impl WeightEstimator {
    pub fn new() -> Self {
        Self {
            base_component_weight: 500.0,
            import_weight: 100.0,
            prop_weight: 50.0,
        }
    }

    pub fn estimate(&self, component: &ParsedComponent) -> f64 {
        let mut weight = self.base_component_weight;

        weight += component.imports.len() as f64 * self.import_weight;
        weight += component.props.len() as f64 * self.prop_weight;
        weight += component.estimated_size as f64;

        if component.is_default_export {
            weight += 100.0;
        }

        weight
    }

    pub fn estimate_with_file_size(&self, component: &ParsedComponent, file_path: &Path) -> f64 {
        let mut weight = self.estimate(component);

        if let Ok(metadata) = std::fs::metadata(file_path) {
            let file_size = metadata.len() as f64;
            weight += file_size * 0.3;
        }

        weight
    }

    pub fn estimate_bitrate(&self, component: &ParsedComponent) -> f64 {
        let mut bitrate = 200.0;

        if component.name.contains("Button") || component.name.contains("Link") {
            bitrate += 300.0;
        }

        if component.name.contains("Header") || component.name.contains("Nav") {
            bitrate += 200.0;
        }

        if component.name.contains("Hero") || component.name.contains("Banner") {
            bitrate += 500.0;
        }

        if component.is_default_export {
            bitrate += 100.0;
        }

        bitrate
    }

    pub fn estimate_priority_hints(&self, component: &ParsedComponent) -> PriorityHints {
        let name_lower = component.name.to_lowercase();

        let is_above_fold = name_lower.contains("header")
            || name_lower.contains("hero")
            || name_lower.contains("nav")
            || name_lower.contains("banner");

        let is_lcp_candidate = name_lower.contains("hero")
            || name_lower.contains("banner")
            || name_lower.contains("image")
            || name_lower.contains("featured");

        let is_interactive = name_lower.contains("button")
            || name_lower.contains("form")
            || name_lower.contains("input")
            || name_lower.contains("link");

        PriorityHints {
            is_above_fold,
            is_lcp_candidate,
            is_interactive,
        }
    }
}

impl Default for WeightEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct PriorityHints {
    pub is_above_fold: bool,
    pub is_lcp_candidate: bool,
    pub is_interactive: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::EffectProfile;

    fn create_test_component(name: &str) -> ParsedComponent {
        ParsedComponent {
            name: name.to_string(),
            file_path: "test.jsx".to_string(),
            line_number: 0,
            imports: vec!["React".to_string()],
            estimated_size: 500,
            is_default_export: false,
            props: Vec::new(),
            effect_profile: EffectProfile::default(),
            source_hash: 1,
        }
    }

    #[test]
    fn test_basic_weight_estimation() {
        let estimator = WeightEstimator::new();
        let component = create_test_component("Button");

        let weight = estimator.estimate(&component);
        assert!(weight > 500.0);
    }

    #[test]
    fn test_priority_hints_header() {
        let estimator = WeightEstimator::new();
        let component = create_test_component("Header");

        let hints = estimator.estimate_priority_hints(&component);
        assert!(hints.is_above_fold);
        assert!(!hints.is_lcp_candidate);
    }

    #[test]
    fn test_priority_hints_hero() {
        let estimator = WeightEstimator::new();
        let component = create_test_component("HeroImage");

        let hints = estimator.estimate_priority_hints(&component);
        assert!(hints.is_above_fold);
        assert!(hints.is_lcp_candidate);
    }

    #[test]
    fn test_priority_hints_button() {
        let estimator = WeightEstimator::new();
        let component = create_test_component("Button");

        let hints = estimator.estimate_priority_hints(&component);
        assert!(hints.is_interactive);
    }
}
