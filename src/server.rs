use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    routing::get,
    Router,
};
use std::fs;
use std::sync::Arc;

use crate::config::Config;
use crate::manifest::Manifest;

struct AppState {
    config: Config,
    manifest: Manifest,
}

/// Serve JAM endpoint
pub async fn serve(config: Config, port: u16) -> Result<()> {
    let manifest = Manifest::load(&config.manifest_path)?;
    
    let state = Arc::new(AppState { config, manifest });
    
    let app = Router::new()
        .route("/paste/:version/*modules", get(handle_jam))
        .with_state(state);
    
    println!("Serving JAM endpoint at http://localhost:{}/paste/...", port);
    println!("URL format: /paste/{{timestamp}}/{{module+version,...}}.{{min.js|min.css}}");
    println!("Example:    /paste/2025.02.02/paste.dom+v1.0,paste.event+v2.0.min.js");
    println!("Press Ctrl+C to stop");
    
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    axum::serve(listener, app).await?;
    
    Ok(())
}

async fn handle_jam(
    State(state): State<Arc<AppState>>,
    Path((_version, modules_path)): Path<(String, String)>,
) -> (StatusCode, [(axum::http::HeaderName, &'static str); 2], String) {
    // URL format: /paste/{timestamp}/{module+version,module+version,...}.{min.js|min.css|js|css}
    // Example: /paste/1387575236/paste.dom+v1.0,paste.event+v2.0.min.js
    
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
    
    // Parse module+version pairs: "paste.dom+v1.0,paste.event+v2.0"
    let modules: Vec<&str> = modules_str
        .split(',')
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

async fn serve_js(state: &AppState, modules: &[&str]) -> (StatusCode, [(axum::http::HeaderName, &'static str); 2], String) {
    let mut content_parts = Vec::new();
    
    for module in modules {
        let all_modules = state.manifest.get_dependencies(module);
        
        for m in all_modules {
            let key = format!("js/{}.js", m.replace('.', "/"));
            if let Some(entry) = state.manifest.assets.get(&key) {
                let source_path = std::path::Path::new(&entry.source);
                if source_path.exists() {
                    if let Ok(content) = fs::read_to_string(source_path) {
                        content_parts.push(content);
                    }
                }
            }
        }
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
    
    for module in modules {
        // Look for CSS file matching module name
        let key = format!("css/{}.css", module.replace('.', "/"));
        if let Some(entry) = state.manifest.assets.get(&key) {
            let source_path = std::path::Path::new(&entry.source);
            if source_path.exists() {
                if let Ok(content) = fs::read_to_string(source_path) {
                    content_parts.push(content);
                }
            }
        }
    }
    
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css"),
         (header::CACHE_CONTROL, "public, max-age=31536000")],
        content_parts.join("\n"),
    )
}

#[allow(dead_code)]
async fn serve_bundle(_state: &AppState, modules: &[&str], version: &str, checksum: &str) -> (StatusCode, [(axum::http::HeaderName, &'static str); 2], String) {
    // Return HTML snippet with both CSS and JS tags
    let modules_str = modules.join(",");
    
    let html = format!(
        r#"<link rel="stylesheet" href="/paste/{}/{}/{}.min.css">
<script src="/paste/{}/{}/{}.min.js" defer></script>"#,
        version, checksum, modules_str,
        version, checksum, modules_str
    );
    
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html"),
         (header::CACHE_CONTROL, "public, max-age=31536000")],
        html,
    )
}
