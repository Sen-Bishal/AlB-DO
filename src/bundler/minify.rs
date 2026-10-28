//! Whitespace-and-comment minification for the framework's own browser scripts.
//!
//! `assets/*.js` are written to be read: long comments explaining why each
//! branch exists, generous spacing. That is right for the source and wrong for
//! the wire — every visitor downloads the commentary. This re-emits a parsed
//! script with swc's codegen in `minify` mode: comments and insignificant
//! whitespace go, identifiers stay as written. Nothing is renamed or
//! restructured, so the behaviour is the parse tree's, byte for byte — the
//! printer only decides how much space to put between tokens.
//!
//! ## Scope
//!
//! **Only the framework's own scripts.** npm chunks are not passed through
//! here: they carry their authors' licence headers, and stripping those is not
//! a decision a byte count gets to make (see `bundler::defines`).
//!
//! ## Failure is the original
//!
//! A script that does not parse is returned unchanged rather than dropped or
//! half-printed. The test below asserts that every shipped asset minifies, so
//! that fallback is a safety net, not a code path anyone relies on.

use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::Program;
use swc_ecma_codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax};

/// `source`, minified — or `None` if it does not parse cleanly.
#[must_use]
pub fn try_minify_script(source: &str) -> Option<String> {
    let source_map: Lrc<SourceMap> = Lrc::default();
    let file = source_map.new_source_file(
        FileName::Custom("albedo-asset.js".to_string()).into(),
        source.to_string(),
    );
    let mut parser = Parser::new(
        Syntax::Es(EsSyntax::default()),
        StringInput::from(&*file),
        None,
    );
    let program: Program = parser.parse_program().ok()?;
    if !parser.take_errors().is_empty() {
        return None;
    }

    let mut buffer = Vec::with_capacity(source.len() / 2);
    {
        let writer = JsWriter::new(source_map.clone(), "\n", &mut buffer, None);
        let mut emitter = Emitter {
            cfg: CodegenConfig::default().with_minify(true),
            cm: source_map,
            comments: None,
            wr: writer,
        };
        emitter.emit_program(&program).ok()?;
    }
    String::from_utf8(buffer).ok()
}

/// `source`, minified, or unchanged if it does not parse.
#[must_use]
pub fn minify_script(source: &str) -> String {
    try_minify_script(source).unwrap_or_else(|| source.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every script the server hands a browser. Kept in step with
    /// `albedo-server`'s `handlers::albedo_assets` and `handlers::dev`.
    const SHIPPED: &[(&str, &str)] = &[
        ("albedo-runtime.js", include_str!("../../assets/albedo-runtime.js")),
        ("bincode.js", include_str!("../../assets/bincode.js")),
        ("albedo-link-forms.js", include_str!("../../assets/albedo-link-forms.js")),
        ("albedo-hydration.js", include_str!("../../assets/albedo-hydration.js")),
        ("albedo-wt-bootstrap.js", include_str!("../../assets/albedo-wt-bootstrap.js")),
        ("phosphor.js", include_str!("../../assets/phosphor.js")),
        ("albedo-client.js", include_str!("../../assets/albedo-client.js")),
        ("albedo-error-overlay.js", include_str!("../../assets/albedo-error-overlay.js")),
        ("albedo-hmr-apply.js", include_str!("../../assets/albedo-hmr-apply.js")),
        ("albedo-dev-stream.js", include_str!("../../assets/albedo-dev-stream.js")),
    ];

    /// The fallback must never fire for anything we ship: if it did, that
    /// asset would silently go out unminified.
    #[test]
    fn every_shipped_asset_minifies_and_reparses() {
        for (name, source) in SHIPPED {
            let minified = try_minify_script(source)
                .unwrap_or_else(|| panic!("{name} did not parse — it would ship unminified"));
            assert!(
                minified.len() < source.len(),
                "{name}: {} → {} bytes",
                source.len(),
                minified.len()
            );
            assert!(
                try_minify_script(&minified).is_some(),
                "{name}: the minified output must itself parse"
            );
            assert!(!minified.contains("TODO.md"), "{name}: a comment survived");
        }
    }

    /// Identifiers and string contents are untouched — only the spacing and
    /// the commentary go.
    #[test]
    fn minification_changes_layout_not_meaning() {
        let source = "// explain\nconst answer = 40 + 2; /* why */\nfunction greet(name) {\n  return `hi ${name} — ok`;\n}\nexport { answer, greet };\n";
        let minified = minify_script(source);
        assert!(!minified.contains("explain") && !minified.contains("why"), "{minified}");
        assert!(minified.contains("answer") && minified.contains("greet"), "{minified}");
        assert!(minified.contains("`hi ${name} — ok`"), "strings are kept: {minified}");
        assert!(minified.len() < source.len());
    }

    /// Writes every shipped asset and its minified form to `ALBEDO_MINIFY_OUT`
    /// for measuring. A measurement, not a check: run it explicitly.
    #[test]
    #[ignore = "writes files for measurement; run explicitly"]
    fn write_minified_assets_for_measurement() {
        let Ok(out) = std::env::var("ALBEDO_MINIFY_OUT") else {
            return;
        };
        for (name, source) in SHIPPED {
            std::fs::write(format!("{out}/{name}"), source).unwrap();
            std::fs::write(format!("{out}/{name}.min"), minify_script(source)).unwrap();
        }
    }

    #[test]
    fn a_script_that_does_not_parse_is_returned_unchanged() {
        let broken = "function (";
        assert_eq!(try_minify_script(broken), None);
        assert_eq!(minify_script(broken), broken);
    }
}

