//! Single-module parse + transpile helper.
//!
//! Supported media types are exactly JavaScript (`.js`), ESM JavaScript
//! (`.mjs`), TypeScript (`.ts`) and ESM TypeScript (`.mts`). JSX/TSX and
//! CommonJS TypeScript are rejected explicitly rather than guessed at.
//!
//! The output is always an ES module JavaScript string. Source maps are
//! disabled deliberately: preparation must never emit external files.

use std::sync::Arc;

use deno_ast::{
    parse_module, EmitOptions, MediaType, ModuleSpecifier, ParseParams, SourceMapOption,
    TranspileModuleOptions, TranspileOptions,
};

use crate::error::ModuleError;
use crate::path::validate_path;

/// Parse and (for TypeScript) transpile one module.
///
/// * `path` is a relative, prepared module path (validated by
///   [`crate::validate_path`]) and also supplies the media type by extension.
/// * `source` is the in-memory module text; nothing is read from disk.
///
/// JavaScript is parsed as a module to reject syntax errors, then returned
/// unchanged. TypeScript is parsed and transpiled with default `deno_ast`
/// transpile options, returning `into_source().text`. The returned string is
/// JavaScript while the caller keeps the original filename/extension, because
/// the sandbox loader maps exact prepared paths to already-prepared sources.
pub fn transpile_module(path: &str, source: &str) -> Result<String, ModuleError> {
    validate_path(path)?;
    let media_type = media_type_from_extension(path)?;

    let specifier = ModuleSpecifier::parse(&format!("sandbox:///{path}")).map_err(|_| {
        ModuleError::InvalidSpecifier {
            path: path.to_string(),
        }
    })?;

    let parsed = parse_module(ParseParams {
        specifier,
        text: Arc::<str>::from(source),
        media_type,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|err| ModuleError::ParseError {
        path: path.to_string(),
        message: err.to_string(),
    })?;

    match media_type {
        // Plain JS/ESM is already valid loader input; parsing above is the
        // only gate needed.
        MediaType::JavaScript | MediaType::Mjs => Ok(source.to_string()),
        MediaType::TypeScript | MediaType::Mts => {
            let emit_options = EmitOptions {
                source_map: SourceMapOption::None,
                inline_sources: false,
                ..Default::default()
            };
            let result = parsed
                .transpile(
                    &TranspileOptions::default(),
                    &TranspileModuleOptions::default(),
                    &emit_options,
                )
                .map_err(|err| ModuleError::TranspileError {
                    path: path.to_string(),
                    message: err.to_string(),
                })?;
            Ok(result.into_source().text)
        }
        // `media_type_from_extension` only returns the four types above.
        other => Err(ModuleError::UnsupportedMediaType {
            path: path.to_string(),
            media_type: format!("{other:?}"),
        }),
    }
}

fn media_type_from_extension(path: &str) -> Result<MediaType, ModuleError> {
    let ext = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| !ext.is_empty());
    let media_type = match ext.as_deref() {
        Some("js") => MediaType::JavaScript,
        Some("mjs") => MediaType::Mjs,
        Some("ts") => MediaType::TypeScript,
        Some("mts") => MediaType::Mts,
        Some("jsx") => MediaType::Jsx,
        Some("tsx") => MediaType::Tsx,
        Some("cts") => MediaType::Cts,
        _ => MediaType::Unknown,
    };
    match media_type {
        MediaType::JavaScript | MediaType::Mjs | MediaType::TypeScript | MediaType::Mts => {
            Ok(media_type)
        }
        other => Err(ModuleError::UnsupportedMediaType {
            path: path.to_string(),
            media_type: match other {
                MediaType::Unknown => ext.as_deref().unwrap_or("unknown").to_string(),
                _ => format!("{other:?}"),
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transpiles_typed_typescript_function_and_interface() {
        let source = r#"
interface Point {
  x: number;
  y: number;
}

export function add(a: Point, b: Point): Point {
  return { x: a.x + b.x, y: a.y + b.y };
}

export const origin: Point = { x: 0, y: 0 };
"#;
        let output = transpile_module("src/geometry.ts", source).expect("transpile");
        assert!(!output.contains("interface Point"), "{output}");
        assert!(!output.contains(": number"), "{output}");
        assert!(output.contains("function add"), "{output}");
        assert!(output.contains("origin"), "{output}");
        // No source maps are emitted.
        assert!(!output.contains("sourceMappingURL"), "{output}");
    }

    #[test]
    fn transpiles_mts_and_parses_mjs() {
        let ts = "export const n: number = 1;\n";
        let out = transpile_module("mod.mts", ts).expect("mts");
        assert!(!out.contains(": number"), "{out}");

        let js = "export const n = 1;\n";
        let out = transpile_module("mod.mjs", js).expect("mjs");
        assert_eq!(out, js);
    }

    #[test]
    fn parses_js_and_rejects_syntax_errors() {
        assert!(transpile_module("ok.js", "export const a = 1;\n").is_ok());
        for (path, source) in [
            ("bad.js", "export const = ;\n"),
            ("bad.ts", "function broken( { return 1; }\n"),
            ("bad.mts", "interface {}\n"),
        ] {
            assert!(
                matches!(
                    transpile_module(path, source),
                    Err(ModuleError::ParseError { .. })
                ),
                "expected parse error for {path}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_media_types_explicitly() {
        for path in ["a.jsx", "a.tsx", "a.cts", "a.json", "a", "a.wasm"] {
            assert!(
                matches!(
                    transpile_module(path, "export {};"),
                    Err(ModuleError::UnsupportedMediaType { .. })
                ),
                "expected unsupported media type for {path}"
            );
        }
    }

    #[test]
    fn rejects_invalid_module_paths() {
        for path in ["../x.ts", "/abs/x.ts", "a\\x.ts", "x.ts?query", "x.ts#hash"] {
            assert!(
                matches!(
                    transpile_module(path, "export {};"),
                    Err(ModuleError::InvalidPath { .. })
                ),
                "expected invalid path for {path}"
            );
        }
    }

    #[test]
    fn returns_js_unchanged_and_keeps_jsx_transform_off_path() {
        let source = "import { x } from './dep.js';\nexport default x;\n";
        assert_eq!(transpile_module("index.js", source).unwrap(), source);
    }
}

/// Collect static and literal dynamic imports using the JavaScript AST.
/// Computed imports are authorized by the runtime loader when evaluated.
pub fn import_specifiers(source: &str) -> Result<Vec<String>, ModuleError> {
    use deno_ast::swc::ast::{CallExpr, Callee, ExportAll, Expr, ImportDecl, Lit, NamedExport};
    use deno_ast::swc::ecma_visit::{Visit, VisitWith};
    struct Imports(Vec<String>);
    impl Visit for Imports {
        fn visit_import_decl(&mut self, n: &ImportDecl) {
            self.0.push(n.src.value.to_string_lossy().into_owned());
        }
        fn visit_named_export(&mut self, n: &NamedExport) {
            if let Some(src) = &n.src {
                self.0.push(src.value.to_string_lossy().into_owned());
            }
        }
        fn visit_export_all(&mut self, n: &ExportAll) {
            self.0.push(n.src.value.to_string_lossy().into_owned());
        }
        fn visit_call_expr(&mut self, n: &CallExpr) {
            if matches!(n.callee, Callee::Import(_)) {
                if let Some(arg) = n.args.first() {
                    match &*arg.expr {
                        Expr::Lit(Lit::Str(s)) => {
                            self.0.push(s.value.to_string_lossy().into_owned())
                        }
                        Expr::Tpl(t) if t.exprs.is_empty() => {
                            if let Some(s) = t.quasis.first().and_then(|q| q.cooked.as_ref()) {
                                self.0.push(s.to_string_lossy().into_owned());
                            }
                        }
                        _ => {}
                    }
                }
            }
            n.visit_children_with(self);
        }
    }
    let parsed = parse_module(ParseParams {
        specifier: ModuleSpecifier::parse("sandbox:///imports.js").expect("constant URL"),
        text: Arc::from(source),
        media_type: MediaType::JavaScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|err| ModuleError::ParseError {
        path: "imports.js".into(),
        message: err.to_string(),
    })?;
    let mut imports = Imports(Vec::new());
    parsed.program().visit_with(&mut imports);
    Ok(imports.0)
}
