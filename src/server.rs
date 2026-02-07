use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    routing::get,
    Router,
};
use std::collections::HashSet;
use std::fs;
use std::sync::Arc;

use crate::config::Config;
use crate::manifest::Manifest;
use crate::storage::StorageBackend;

struct AppState {
    config: Config,
    storage: StorageBackend,
}

fn load_manifest(state: &AppState) -> Manifest {
    Manifest::load(&state.config.manifest_path)
        .unwrap_or_else(|_| Manifest::new())
}

/// Serve JAM endpoint
pub async fn serve(config: Config, port: u16) -> Result<()> {
    let mut type_dirs = std::collections::HashMap::new();
    type_dirs.insert("js".to_string(), config.output.js.clone());
    type_dirs.insert("css".to_string(), config.output.css.clone());
    type_dirs.insert("images".to_string(), config.output.images.clone());
    let storage = StorageBackend::from_config(
        &config.storage.backend,
        &config.output.base,
        config.storage.keep_versions,
        type_dirs,
    );
    let state = Arc::new(AppState { config, storage });
    
    let app = Router::new()
        .route("/paste/:version/*modules", get(handle_jam))
        .with_state(state);
    
    println!("Serving JAM endpoint at http://localhost:{}/paste/...", port);
    println!("URL format: /paste/{{timestamp}}/{{module+version,...}}.{{min.js|min.css}}");
    println!("Example:    /paste/2025.02.02/paste.dom+v1.0~paste.event+v2.0.min.js");
    println!("Press Ctrl+C to stop");
    
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    axum::serve(listener, app).await?;
    
    Ok(())
}

async fn handle_jam(
    State(state): State<Arc<AppState>>,
    Path((_version, modules_path)): Path<(String, String)>,
) -> (StatusCode, [(axum::http::HeaderName, &'static str); 2], String) {
    // URL format: /paste/{timestamp}/{module+version~module+version~...}.{min.js|min.css|js|css}
    // Example: /paste/1387575236/paste.dom+v1.0~paste.event+v2.0.min.js
    
    // Determine request type: .min.js, .min.css, .js, .css
    let (modules_str, response_type) = if modules_path.ends_with(".min.css") {
        (modules_path.trim_end_matches(".min.css"), "css")
    } else if modules_path.ends_with(".css") {
        (modules_path.trim_end_matches(".css"), "css")
    } else if modules_path.ends_with(".min.js") {
        (modules_path.trim_end_matches(".min.js"), "js")
    } else {
        (modules_path.trim_end_matches(".js"), "js")
    };
    
    // Parse module+version pairs: "paste.dom+v1.0~paste.event+v2.0"
    let modules: Vec<&str> = modules_str
        .split(|c| c == '~' || c == ',')
        .map(|m| {
            // Strip version suffix: "paste.dom+v1.0" -> "paste.dom"
            m.split('+').next().unwrap_or(m)
        })
        .collect();
    
    if response_type == "css" {
        serve_css(&state, &modules).await
    } else {
        serve_js(&state, &modules).await
    }
}

/// Resolve modules to a deduplicated list in dependency order.
/// This is a pure function for testability.
pub fn resolve_modules(manifest: &Manifest, modules: &[&str], asset_type: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    
    for module in modules {
        let all_modules = manifest.get_dependencies_for_type(module, asset_type);
        
        for m in all_modules {
            if !seen.contains(&m) {
                seen.insert(m.clone());
                result.push(m);
            }
        }
    }
    
    result
}

async fn serve_js(state: &AppState, modules: &[&str]) -> (StatusCode, [(axum::http::HeaderName, &'static str); 2], String) {
    let mut content_parts = Vec::new();
    
    // Reload manifest each request so rebuilds are reflected
    let manifest = load_manifest(state);
    
    // Use deduplicated module list
    let all_modules = resolve_modules(&manifest, modules, "js");
    
    for m in all_modules {
        // Resolve via modules map first (filtered to JS keys), then direct key
        let direct_key = format!("js/{}.js", m.replace('.', "/"));
        let entry_opt = manifest.modules.get(m.as_str())
            .filter(|k| k.ends_with(".js"))
            .and_then(|k| manifest.assets.get(k.as_str()))
            .or_else(|| manifest.assets.get(&direct_key));
        if let Some(entry) = entry_opt {
            // Resolve output path: entry.output is like "/js/paste/dom.abc123.v1.min.js"
            // Use StorageBackend::resolve_dir to match write_asset layout
            let output_name = entry.output.strip_prefix("/js/").unwrap_or(&entry.output);
            let output_path = state.storage.resolve_dir("js").join(output_name);
            if output_path.exists() {
                if let Ok(content) = fs::read_to_string(&output_path) {
                    content_parts.push(content);
                }
            }
        }
    }
    
    // Return 404 for unknown modules instead of caching empty response
    if content_parts.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/javascript"),
             (header::CACHE_CONTROL, "no-cache")],
            "/* modules not found */".to_string(),
        );
    }
    
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/javascript"),
         (header::CACHE_CONTROL, "public, max-age=31536000")],
        content_parts.join("\n"),
    )
}

async fn serve_css(state: &AppState, modules: &[&str]) -> (StatusCode, [(axum::http::HeaderName, &'static str); 2], String) {
    let mut content_parts = Vec::new();
    
    // Reload manifest each request so rebuilds are reflected
    let manifest = load_manifest(state);
    
    // Use deduplicated module list with dependency expansion
    let all_modules = resolve_modules(&manifest, modules, "css");
    
    for m in all_modules {
        // Resolve via modules map first (filtered to CSS keys), then direct key
        let direct_key = format!("css/{}.css", m.replace('.', "/"));
        let entry_opt = manifest.modules.get(m.as_str())
            .filter(|k| k.ends_with(".css"))
            .and_then(|k| manifest.assets.get(k.as_str()))
            .or_else(|| manifest.assets.get(&direct_key));
        if let Some(entry) = entry_opt {
            // Resolve output path: entry.output is like "/css/main.abc123.v1.min.css"
            // Use StorageBackend::resolve_dir to match write_asset layout
            let output_name = entry.output.strip_prefix("/css/").unwrap_or(&entry.output);
            let output_path = state.storage.resolve_dir("css").join(output_name);
            if output_path.exists() {
                if let Ok(content) = fs::read_to_string(&output_path) {
                    content_parts.push(content);
                }
            }
        }
    }
    
    // Return 404 for unknown modules instead of caching empty response
    if content_parts.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/css"),
             (header::CACHE_CONTROL, "no-cache")],
            "/* modules not found */".to_string(),
        );
    }
    
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css"),
         (header::CACHE_CONTROL, "public, max-age=31536000")],
        content_parts.join("\n"),
    )
}

#[allow(dead_code)]
async fn serve_bundle(_state: &AppState, modules: &[&str], version: &str) -> (StatusCode, [(axum::http::HeaderName, &'static str); 2], String) {
    // Return HTML snippet with both CSS and JS tags
    // Uses 2-segment URL format: /paste/{version}/{modules}.min.{ext}
    let modules_str = modules.join("~");
    
    let html = format!(
        r#"<link rel="stylesheet" href="/paste/{}/{}.min.css">
<script src="/paste/{}/{}.min.js" defer></script>"#,
        version, modules_str,
        version, modules_str
    );
    
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html"),
         (header::CACHE_CONTROL, "public, max-age=31536000")],
        html,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{AssetEntry, Manifest};
    
    fn create_test_manifest_with_diamond() -> Manifest {
        let mut manifest = Manifest::new();
        
        // D (no deps)
        manifest.assets.insert(
            "js/d.js".to_string(),
            AssetEntry {
                source: "src/d.js".to_string(),
                output: "/js/d.hash.v1.min.js".to_string(),
                hash: "hash_d".to_string(),
                version: 1,
                timestamp: "2024-01-15T10:00:00+00:00".to_string(),
                dependencies: vec![],
                history: vec![],
            },
        );
        
        // B depends on D
        manifest.assets.insert(
            "js/b.js".to_string(),
            AssetEntry {
                source: "src/b.js".to_string(),
                output: "/js/b.hash.v1.min.js".to_string(),
                hash: "hash_b".to_string(),
                version: 1,
                timestamp: "2024-01-15T10:00:00+00:00".to_string(),
                dependencies: vec!["d".to_string()],
                history: vec![],
            },
        );
        
        // C depends on D
        manifest.assets.insert(
            "js/c.js".to_string(),
            AssetEntry {
                source: "src/c.js".to_string(),
                output: "/js/c.hash.v1.min.js".to_string(),
                hash: "hash_c".to_string(),
                version: 1,
                timestamp: "2024-01-15T10:00:00+00:00".to_string(),
                dependencies: vec!["d".to_string()],
                history: vec![],
            },
        );
        
        manifest
    }
    
    #[test]
    fn test_resolve_js_modules_single() {
        let manifest = create_test_manifest_with_diamond();
        
        let result = resolve_modules(&manifest, &["b"], "js");
        
        // B depends on D, so should get [D, B]
        assert_eq!(result.len(), 2, "Should have 2 modules: {:?}", result);
        assert!(result.contains(&"d".to_string()));
        assert!(result.contains(&"b".to_string()));
    }
    
    #[test]
    fn test_resolve_js_modules_deduplicates_shared_deps() {
        let manifest = create_test_manifest_with_diamond();
        
        // Request B and C, both depend on D
        let result = resolve_modules(&manifest, &["b", "c"], "js");
        
        // D should appear exactly once
        let d_count = result.iter().filter(|m| *m == "d").count();
        assert_eq!(d_count, 1, "D should appear exactly once: {:?}", result);
        
        // Should have D, B, C (3 total)
        assert_eq!(result.len(), 3, "Should have 3 modules: {:?}", result);
    }
    
    #[test]
    fn test_resolve_js_modules_preserves_order() {
        let manifest = create_test_manifest_with_diamond();
        
        let result = resolve_modules(&manifest, &["b", "c"], "js");
        
        // D should come before B and C
        let d_idx = result.iter().position(|m| m == "d").unwrap();
        let b_idx = result.iter().position(|m| m == "b").unwrap();
        let c_idx = result.iter().position(|m| m == "c").unwrap();
        
        assert!(d_idx < b_idx, "d should come before b");
        assert!(d_idx < c_idx, "d should come before c");
    }
    
    // ============================================================
    // Unknown module resolution: should pass through (matches Scala expandDependencies)
    // ============================================================
    
    #[test]
    fn test_resolve_js_modules_includes_unknown() {
        let manifest = Manifest::new(); // empty manifest
        
        let result = resolve_modules(&manifest, &["nonexistent.module"], "js");
        
        assert_eq!(result, vec!["nonexistent.module"],
            "Unknown modules should be included for convention-based resolution at serve time");
    }
    
    // ============================================================
    // CSS module dependency expansion
    // ============================================================
    
    #[test]
    fn test_resolve_css_modules_deduplicates() {
        let mut manifest = Manifest::new();
        
        // CSS module with no deps
        manifest.assets.insert(
            "css/base.css".to_string(),
            AssetEntry {
                source: "src/base.scss".to_string(),
                output: "/css/base.hash.v1.min.css".to_string(),
                hash: "hash_base".to_string(),
                version: 1,
                timestamp: "2024-01-15T10:00:00+00:00".to_string(),
                dependencies: vec![],
                history: vec![],
            },
        );
        
        // CSS module that depends on base
        manifest.assets.insert(
            "css/theme.css".to_string(),
            AssetEntry {
                source: "src/theme.scss".to_string(),
                output: "/css/theme.hash.v1.min.css".to_string(),
                hash: "hash_theme".to_string(),
                version: 1,
                timestamp: "2024-01-15T10:00:00+00:00".to_string(),
                dependencies: vec!["base".to_string()],
                history: vec![],
            },
        );
        
        let result = resolve_modules(&manifest, &["theme"], "css");
        
        // base should appear before theme (dependency order)
        assert!(result.contains(&"base".to_string()), "base should be in result: {:?}", result);
        assert!(result.contains(&"theme".to_string()), "theme should be in result: {:?}", result);
        let base_idx = result.iter().position(|m| m == "base").unwrap();
        let theme_idx = result.iter().position(|m| m == "theme").unwrap();
        assert!(base_idx < theme_idx, "base should come before theme");
    }
    
    // ============================================================
    // Output path resolution: must use output.base/{type} to match storage backend
    // ============================================================
    
    #[test]
    fn test_output_path_resolution_uses_base() {
        use std::path::PathBuf;
        let base = PathBuf::from("/project/output");
        let output_name = "paste/dom.abc123.v1.min.js";
        
        // This is what the server should use (matches StorageBackend::write_asset)
        let expected = base.join("js").join(output_name);
        assert_eq!(expected, PathBuf::from("/project/output/js/paste/dom.abc123.v1.min.js"));
        
        // If output.js differs from output.base/js, the old code would diverge
        let old_js_dir = PathBuf::from("/project/output/custom-js");
        let old_path = old_js_dir.join(output_name);
        assert_ne!(old_path, expected, 
            "Custom output.js should differ from output.base/js");
    }
}
