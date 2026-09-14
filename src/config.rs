use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourcePaths {
    pub js: PathBuf,
    pub scss: PathBuf,
    pub images: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputPaths {
    pub base: PathBuf,
    pub js: PathBuf,
    pub css: PathBuf,
    pub images: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JamConfig {
    pub enabled: bool,
    pub base_path: String,
}

impl Default for JamConfig {
    fn default() -> Self {
        JamConfig {
            enabled: false,
            base_path: "/paste".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdnConfig {
    pub enabled: bool,
    pub url: String,
    pub fallback_to_local: bool,
}

impl Default for CdnConfig {
    fn default() -> Self {
        CdnConfig {
            enabled: false,
            url: String::new(),
            fallback_to_local: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Backend type: "local", "dist", or "s3"
    pub backend: String,
    /// Number of versions to keep (for dist/s3 backends)
    pub keep_versions: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            backend: "local".to_string(),
            keep_versions: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Directory every relative path in the config was resolved against.
    /// Manifest source labels are recorded relative to it, so a manifest
    /// describes the project rather than the machine that built it.
    #[serde(default)]
    pub base_dir: PathBuf,
    #[serde(default)]
    pub debug_mode: bool,
    #[serde(default = "default_true")]
    pub minify: bool,
    #[serde(default = "default_true")]
    pub source_maps: bool,
    pub source: SourcePaths,
    pub output: OutputPaths,
    pub manifest_path: PathBuf,
    #[serde(default)]
    pub include_paths: Vec<PathBuf>,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub jam: JamConfig,
    #[serde(default)]
    pub cdn: CdnConfig,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        // Resolve config path so we can derive its parent directory.
        // HOCON configs expect relative paths to resolve against the
        // config file's directory, not the process CWD.
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let base_dir = resolved.parent().unwrap_or(&cwd);
        Self::load_with_base_dir(path, base_dir)
    }

    /// Load config with an explicit base directory for path resolution.
    /// Prefer this over `load()` when the working directory is known,
    /// to avoid reliance on process-global `std::env::current_dir()`.
    pub fn load_with_base_dir(path: &Path, base_dir: &Path) -> Result<Self> {
        // Resolve config path relative to base_dir if not absolute
        let resolved_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base_dir.join(path)
        };
        let content = fs::read_to_string(&resolved_path)?;

        // Simple HOCON-like parsing (for now, use JSON subset)
        // TODO: Full HOCON support
        if resolved_path.extension().map_or(false, |e| e == "json") {
            let mut cfg: Config = serde_json::from_str(&content)?;
            // Resolve all relative paths against base_dir, matching HOCON behavior.
            // JSON configs deserialized by serde contain paths as-is from the file.
            cfg.base_dir = base_dir.to_path_buf();
            cfg.source.js = Self::resolve_path(base_dir, &cfg.source.js);
            cfg.source.scss = Self::resolve_path(base_dir, &cfg.source.scss);
            cfg.source.images = Self::resolve_path(base_dir, &cfg.source.images);
            cfg.output.base = Self::resolve_path(base_dir, &cfg.output.base);
            cfg.output.js = Self::resolve_path(base_dir, &cfg.output.js);
            cfg.output.css = Self::resolve_path(base_dir, &cfg.output.css);
            cfg.output.images = Self::resolve_path(base_dir, &cfg.output.images);
            cfg.manifest_path = Self::resolve_path(base_dir, &cfg.manifest_path);
            cfg.include_paths = cfg
                .include_paths
                .iter()
                .map(|p| Self::resolve_path(base_dir, p))
                .collect();
            return Ok(cfg);
        }

        Self::parse_hocon(&content, base_dir)
    }

    fn parse_hocon(content: &str, base_dir: &Path) -> Result<Self> {
        // Simplified HOCON parsing - extract key values
        let get_value = |key: &str| -> Option<String> {
            // HOCON last-value-wins: keep the final match
            let mut last_value = None;
            for line in content.lines() {
                let line = line.trim();
                // Check for exact key match: key must be followed by '=' or whitespace
                if line.starts_with(key) {
                    let after_key = line.chars().skip(key.len()).next();
                    if after_key == Some('=') || after_key == Some(' ') || after_key == Some('\t') {
                        if let Some(value) = line.splitn(2, '=').nth(1) {
                            let value = Self::strip_inline_comment(value.trim());
                            last_value = Some(value.trim_matches('"').to_string());
                        }
                    }
                }
            }
            last_value
        };

        let get_bool = |key: &str| -> bool { get_value(key).map_or(false, |v| v == "true") };

        // Parse include-paths array
        // HOCON last-value-wins: keep the final match for arrays
        let get_array = |key: &str| -> Vec<String> {
            let mut last_value = Vec::new();
            for line in content.lines() {
                let line = line.trim();
                // Check for exact key match: key must be followed by '=' or whitespace
                if line.starts_with(key) {
                    let after_key = line.chars().skip(key.len()).next();
                    if after_key == Some('=') || after_key == Some(' ') || after_key == Some('\t') {
                        if let Some(value) = line.splitn(2, '=').nth(1) {
                            let value = Self::strip_inline_comment(value.trim());
                            if value.starts_with('[') && value.ends_with(']') {
                                let inner = &value[1..value.len() - 1];
                                last_value = inner
                                    .split(',')
                                    .map(|s| s.trim().trim_matches('"').to_string())
                                    .filter(|s| !s.is_empty())
                                    .collect();
                            }
                        }
                    }
                }
            }
            last_value
        };

        let include_paths: Vec<PathBuf> = get_array("include-paths")
            .iter()
            .map(|p| base_dir.join(p))
            .collect();

        Ok(Config {
            base_dir: base_dir.to_path_buf(),
            debug_mode: get_bool("debug-mode"),
            minify: get_value("minify").map_or(true, |v| v != "false"),
            source_maps: get_value("source-maps").map_or(true, |v| v != "false"),
            source: SourcePaths {
                js: base_dir
                    .join(get_value("source.js").unwrap_or_else(|| "src/main/assets/js".into())),
                scss: base_dir.join(
                    get_value("source.scss").unwrap_or_else(|| "src/main/assets/scss".into()),
                ),
                images: base_dir.join(
                    get_value("source.images").unwrap_or_else(|| "src/main/assets/images".into()),
                ),
            },
            output: OutputPaths {
                base: base_dir
                    .join(get_value("output.base").unwrap_or_else(|| "src/main/public".into())),
                js: base_dir
                    .join(get_value("output.js").unwrap_or_else(|| "src/main/public/js".into())),
                css: base_dir
                    .join(get_value("output.css").unwrap_or_else(|| "src/main/public/css".into())),
                images: base_dir.join(
                    get_value("output.images").unwrap_or_else(|| "src/main/public/images".into()),
                ),
            },
            manifest_path: base_dir.join(
                get_value("manifest").unwrap_or_else(|| "src/main/public/manifest.json".into()),
            ),
            include_paths,
            storage: StorageConfig {
                backend: get_value("storage.backend").unwrap_or_else(|| "local".into()),
                keep_versions: get_value("storage.keep-versions")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10),
            },
            jam: JamConfig {
                enabled: get_bool("jam.enabled"),
                base_path: get_value("jam.base-path").unwrap_or_else(|| "/paste".into()),
            },
            cdn: CdnConfig {
                enabled: get_bool("cdn.enabled"),
                url: get_value("cdn.url").unwrap_or_default(),
                fallback_to_local: get_value("cdn.fallback-to-local")
                    .map_or(true, |v| v != "false"),
            },
        })
    }

    /// Strip inline `#` or `//` comments from an HOCON value.
    /// Respects quoted strings: `"value # not a comment"` is preserved intact.
    fn strip_inline_comment(value: &str) -> &str {
        let bytes = value.as_bytes();
        let mut in_quotes = false;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' && in_quotes && i + 1 < bytes.len() {
                // Skip escaped character inside quotes (e.g., \")
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                in_quotes = !in_quotes;
            } else if !in_quotes {
                if bytes[i] == b'#' {
                    return value[..i].trim_end();
                }
                if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    return value[..i].trim_end();
                }
            }
            i += 1;
        }
        value
    }

    /// Resolve a path against base_dir if it is relative; return as-is if absolute.
    fn resolve_path(base_dir: &Path, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_dir.join(p)
        }
    }

    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug_mode = debug;
        self
    }

    pub fn default_for(base_dir: &Path) -> Self {
        Config {
            base_dir: base_dir.to_path_buf(),
            debug_mode: false,
            minify: true,
            source_maps: true,
            source: SourcePaths {
                js: base_dir.join("src/main/assets/js"),
                scss: base_dir.join("src/main/assets/scss"),
                images: base_dir.join("src/main/assets/images"),
            },
            output: OutputPaths {
                base: base_dir.join("src/main/public"),
                js: base_dir.join("src/main/public/js"),
                css: base_dir.join("src/main/public/css"),
                images: base_dir.join("src/main/public/images"),
            },
            manifest_path: base_dir.join("src/main/public/manifest.json"),
            include_paths: Vec::new(),
            storage: StorageConfig::default(),
            jam: JamConfig {
                enabled: true,
                base_path: "/paste".into(),
            },
            cdn: CdnConfig {
                enabled: false,
                url: String::new(),
                fallback_to_local: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ============================================================
    // Tests use parse_hocon directly to avoid global state issues
    // (set_current_dir affects the entire process and causes race conditions)
    // ============================================================

    #[test]
    fn test_config_exact_key_matching() {
        // Verify that "source.js" doesn't incorrectly match "source.json-extra"
        let config_content = r#"
source.json-extra = "src/json"
source.js = "src/js"
source.scss = "src/scss"
output.base = "dist"
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        // source.js should be "src/js", NOT "src/json" (from source.json-extra)
        let js_path = config.source.js.to_string_lossy();
        assert!(
            js_path.ends_with("src/js"),
            "source.js should be 'src/js', got: {}",
            js_path
        );
        assert!(
            !js_path.contains("json"),
            "source.js should NOT contain 'json': {}",
            js_path
        );
    }

    #[test]
    fn test_config_key_with_similar_prefix() {
        // Test that "output.js" doesn't match "output.json"
        let config_content = r#"
output.base = "dist"
output.json = "dist/json"
output.js = "dist/js"
output.css = "dist/css"
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        let js_path = config.output.js.to_string_lossy();
        assert!(
            js_path.ends_with("dist/js"),
            "output.js should be 'dist/js', got: {}",
            js_path
        );
    }

    #[test]
    fn test_config_key_order_independence() {
        // Test that key matching works regardless of order in file
        // This catches the greedy prefix issue where order matters
        let config_content = r#"
source.json-extra = "src/json"
source.js = "src/js"
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();
        assert!(config.source.js.to_string_lossy().ends_with("src/js"));

        // Reversed order should give same result
        let config_content_reversed = r#"
source.js = "src/js"
source.json-extra = "src/json"
"#;
        let config2 = Config::parse_hocon(config_content_reversed, dir.path()).unwrap();
        assert!(config2.source.js.to_string_lossy().ends_with("src/js"));
    }

    #[test]
    fn test_config_array_parsing() {
        let config_content = r#"
source.js = "src/js"
source.scss = "src/scss"
output.base = "dist"
include-paths = ["../paste-elements", "../other-lib"]
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert_eq!(
            config.include_paths.len(),
            2,
            "Should have 2 include paths: {:?}",
            config.include_paths
        );

        // Verify paths are correctly joined with base_dir
        let path_strs: Vec<String> = config
            .include_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(path_strs.iter().any(|p| p.contains("paste-elements")));
        assert!(path_strs.iter().any(|p| p.contains("other-lib")));
    }

    #[test]
    fn test_config_array_similar_keys() {
        // Test that array key matching is also exact
        let config_content = r#"
include-paths-extra = ["should-not-match"]
include-paths = ["correct-path"]
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert_eq!(config.include_paths.len(), 1);
        assert!(config.include_paths[0]
            .to_string_lossy()
            .contains("correct-path"));
    }

    // ============================================================
    // Array inline comment stripping
    // ============================================================

    #[test]
    fn test_config_array_with_inline_hash_comment() {
        let config_content = r#"
include-paths = ["../paste-elements", "../other-lib"]  # SCSS includes
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert_eq!(
            config.include_paths.len(),
            2,
            "Array with inline # comment should still parse: {:?}",
            config.include_paths
        );
    }

    #[test]
    fn test_config_array_with_inline_double_slash_comment() {
        let config_content = r#"
include-paths = ["../paste-elements"]  // include path
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert_eq!(
            config.include_paths.len(),
            1,
            "Array with inline // comment should still parse: {:?}",
            config.include_paths
        );
    }

    #[test]
    fn test_config_boolean_parsing() {
        let config_content = r#"
source.js = "src/js"
output.base = "dist"
debug-mode = true
jam.enabled = true
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(config.debug_mode, "debug-mode should be true");
        assert!(config.jam.enabled, "jam.enabled should be true");
    }

    #[test]
    fn test_config_boolean_false() {
        let config_content = r#"
debug-mode = false
jam.enabled = false
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(!config.debug_mode, "debug-mode should be false");
        assert!(!config.jam.enabled, "jam.enabled should be false");
    }

    #[test]
    fn test_config_defaults() {
        // Minimal config should use defaults
        let config_content = r#"
output.base = "dist"
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        // Should have sensible defaults
        assert!(!config.debug_mode, "debug_mode should default to false");
        assert_eq!(
            config.storage.keep_versions, 10,
            "keep_versions should default to 10"
        );
        assert!(!config.jam.enabled, "jam.enabled should default to false");
        assert!(!config.cdn.enabled, "cdn.enabled should default to false");
    }

    #[test]
    fn test_config_storage_settings() {
        let config_content = r#"
storage.backend = "dist"
storage.keep-versions = 5
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert_eq!(config.storage.backend, "dist");
        assert_eq!(config.storage.keep_versions, 5);
    }

    // ============================================================
    // Config parsing: minify and source-maps values
    // ============================================================

    #[test]
    fn test_config_minify_and_source_maps_defaults() {
        // When not specified, both should default to true
        let config_content = r#"
output.base = "dist"
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(config.minify, "minify should default to true");
        assert!(config.source_maps, "source_maps should default to true");
    }

    #[test]
    fn test_config_minify_false() {
        let config_content = r#"
minify = false
source-maps = false
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(!config.minify, "minify should be false");
        assert!(!config.source_maps, "source_maps should be false");
    }

    #[test]
    fn test_config_minify_true_explicit() {
        let config_content = r#"
minify = true
source-maps = true
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(config.minify, "minify should be true");
        assert!(config.source_maps, "source_maps should be true");
    }

    #[test]
    fn test_config_sbt_generated_format() {
        // This mimics the exact config format generated by the sbt plugin
        let config_content = r#"
# Auto-generated by sbt AssetGraphPlugin
# Do not edit - configure in build.sbt instead

source.scss = "src/main/assets/scss"
source.js = "src/main/assets/js"
source.images = "src/main/assets/images"
output.base = "target/public"
output.js = "target/public/js"
output.css = "target/public/css"
output.images = "target/public/images"
manifest = "target/paste-assetgraph-manifest.json"
include-paths = ["../paste-elements", "../other-lib"]
debug-mode = false
minify = true
source-maps = true
jam.enabled = true
jam.base-path = "/paste"
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        // Verify all fields are parsed correctly
        assert!(!config.debug_mode);
        assert!(config.minify);
        assert!(config.source_maps);
        assert!(config.jam.enabled);
        assert_eq!(config.jam.base_path, "/paste");
        assert_eq!(config.include_paths.len(), 2);
        assert!(config
            .source
            .js
            .to_string_lossy()
            .ends_with("src/main/assets/js"));
        assert!(config
            .source
            .scss
            .to_string_lossy()
            .ends_with("src/main/assets/scss"));
        assert!(config
            .output
            .base
            .to_string_lossy()
            .ends_with("target/public"));
        assert!(config
            .manifest_path
            .to_string_lossy()
            .ends_with("target/paste-assetgraph-manifest.json"));
    }

    // ============================================================
    // HOCON last-value-wins semantics
    // ============================================================

    #[test]
    fn test_config_value_with_equals_sign() {
        // Values containing '=' (e.g., URLs with query params) should not be truncated
        let config_content = r#"
cdn.url = "https://cdn.example.com/assets?token=abc123&v=2"
cdn.enabled = true
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();
        assert_eq!(
            config.cdn.url, "https://cdn.example.com/assets?token=abc123&v=2",
            "Values containing '=' should not be truncated"
        );
    }

    #[test]
    fn test_config_last_value_wins() {
        let config_content = r#"
source.js = "first"
source.js = "second"
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(
            config.source.js.to_string_lossy().ends_with("second"),
            "HOCON last-value-wins: expected last value, got: {}",
            config.source.js.to_string_lossy()
        );
    }

    // ============================================================
    // Inline comment stripping
    // ============================================================

    #[test]
    fn test_config_inline_comment_hash() {
        let config_content = r#"
source.js = "src/js"  # my js files
source.scss = "src/scss"
output.base = "dist"
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(
            config.source.js.to_string_lossy().ends_with("src/js"),
            "Inline # comment should be stripped, got: {}",
            config.source.js.to_string_lossy()
        );
    }

    #[test]
    fn test_config_inline_comment_double_slash() {
        let config_content = r#"
source.js = "src/js"  // my js files
output.base = "dist"
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(
            config.source.js.to_string_lossy().ends_with("src/js"),
            "Inline // comment should be stripped, got: {}",
            config.source.js.to_string_lossy()
        );
    }

    #[test]
    fn test_config_inline_comment_inside_quotes_preserved() {
        // # inside quotes is NOT a comment
        let config_content = r#"
cdn.url = "https://example.com/path#anchor"
cdn.enabled = true
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert_eq!(
            config.cdn.url, "https://example.com/path#anchor",
            "# inside quotes should NOT be treated as comment"
        );
    }

    // ============================================================
    // cdn.fallback_to_local defaults to true when omitted
    // ============================================================

    #[test]
    fn test_config_cdn_fallback_defaults_to_true() {
        // cdn.fallback-to-local should default to true when not specified,
        // matching default_for() behavior.
        let config_content = r#"
cdn.enabled = true
cdn.url = "https://cdn.example.com"
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(
            config.cdn.fallback_to_local,
            "cdn.fallback_to_local should default to true when omitted"
        );
    }

    #[test]
    fn test_config_cdn_fallback_explicit_false() {
        let config_content = r#"
cdn.enabled = true
cdn.fallback-to-local = false
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(
            !config.cdn.fallback_to_local,
            "cdn.fallback_to_local should be false when explicitly set"
        );
    }

    // ============================================================
    // strip_inline_comment handles escaped quotes
    // ============================================================

    #[test]
    fn test_config_escaped_quote_in_value() {
        // Escaped quotes inside a value should not break comment stripping
        let config_content = r#"
cdn.url = "path with \"quoted\" part" # comment
cdn.enabled = true
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert_eq!(
            config.cdn.url, r#"path with \"quoted\" part"#,
            "Escaped quotes should be preserved, comment stripped"
        );
    }

    // ============================================================
    // JSON config path resolution against base_dir
    // ============================================================

    #[test]
    fn test_config_json_resolves_paths_against_base_dir() {
        let dir = tempdir().unwrap();
        let json_content = r#"{
            "debug_mode": false,
            "minify": true,
            "source_maps": true,
            "source": { "js": "src/js", "scss": "src/scss", "images": "src/images" },
            "output": { "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" },
            "manifest_path": "dist/manifest.json",
            "include_paths": ["../lib"],
            "storage": { "backend": "local", "keep_versions": 10 },
            "jam": { "enabled": true, "base_path": "/paste" },
            "cdn": { "enabled": false, "url": "", "fallback_to_local": true }
        }"#;

        let json_path = dir.path().join("config.json");
        std::fs::write(&json_path, json_content).unwrap();

        let config =
            Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path()).unwrap();

        // Paths should be resolved against dir.path(), not CWD
        assert!(
            config.source.js.starts_with(dir.path()),
            "JSON source.js should be resolved against base_dir, got: {:?}",
            config.source.js
        );
        assert!(
            config.manifest_path.starts_with(dir.path()),
            "JSON manifest_path should be resolved against base_dir, got: {:?}",
            config.manifest_path
        );
        assert!(
            config.include_paths[0].starts_with(dir.path()),
            "JSON include_paths should be resolved against base_dir, got: {:?}",
            config.include_paths[0]
        );
    }

    #[test]
    fn test_init_templates_parse_to_their_stated_settings() {
        // The templates are written in the same flat syntax parse_hocon reads,
        // so a key the parser does not recognize (storage.keep_versions instead
        // of storage.keep-versions) silently loses the setting. Parse each
        // template the way the CLI will.
        let dir = tempdir().unwrap();
        for project_type in ["basic", "react", "scala"] {
            init_config(project_type, dir.path()).unwrap();
            let written = fs::read_to_string(dir.path().join("paste-surface.conf")).unwrap();
            let config = Config::parse_hocon(&written, dir.path()).unwrap();
            assert!(config.minify, "{project_type}: minify should be enabled");
            assert!(
                config.base_dir.starts_with(dir.path()),
                "{project_type}: base_dir should be recorded"
            );
            for key in written
                .lines()
                .filter_map(|line| line.split('=').next())
                .map(str::trim)
            {
                if key == "storage.keep-versions" {
                    assert_eq!(
                        config.storage.keep_versions, 10,
                        "{project_type}: keep-versions should be read"
                    );
                }
                if key == "storage.backend" {
                    assert_eq!(
                        config.storage.backend, "dist",
                        "{project_type}: backend should be read"
                    );
                }
            }
        }
    }
}

pub fn init_config(project_type: &str, base_dir: &Path) -> Result<()> {
    // Use flat key=value format compatible with parse_hocon
    let config = match project_type {
        "react" => {
            r#"# Paste Surface Configuration (React/Vite)

debug-mode = false

source.js = "src"
source.scss = "src/styles"
source.images = "public/images"

output.base = "dist"
output.js = "dist/assets"
output.css = "dist/assets"
output.images = "dist/images"

manifest = "dist/manifest.json"

minify = true
source-maps = true

jam.enabled = true
jam.base-path = "/paste"
"#
        }
        "scala" => {
            r#"# Paste Surface Configuration (Scala/Twirl)

debug-mode = false

source.js = "src/main/assets/js"
source.scss = "src/main/assets/scss"
source.images = "src/main/assets/images"

output.base = "src/main/public"
output.js = "src/main/public/js"
output.css = "src/main/public/css"
output.images = "src/main/public/images"

manifest = "src/main/public/manifest.json"

minify = true
source-maps = true

jam.enabled = true
jam.base-path = "/paste"
"#
        }
        _ => {
            r#"# Paste Surface Configuration (Basic HTML)

debug-mode = false

source.js = "assets/js"
source.scss = "assets/scss"
source.images = "assets/images"

output.base = "public"
output.js = "public/js"
output.css = "public/css"
output.images = "public/images"

storage.backend = "dist"
storage.keep-versions = 10
manifest = "public/manifest.json"

minify = true
source-maps = true

jam.enabled = true
jam.base-path = "/paste"
"#
        }
    };

    let output_path = base_dir.join("paste-surface.conf");
    fs::write(&output_path, config)?;
    println!("Created {:?} for {} project", output_path, project_type);
    Ok(())
}
