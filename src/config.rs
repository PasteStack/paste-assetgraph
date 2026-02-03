use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdnConfig {
    pub enabled: bool,
    pub url: String,
    pub fallback_to_local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub debug_mode: bool,
    pub source: SourcePaths,
    pub output: OutputPaths,
    pub manifest_path: PathBuf,
    pub jam: JamConfig,
    pub cdn: CdnConfig,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        
        // Simple HOCON-like parsing (for now, use JSON subset)
        // TODO: Full HOCON support
        if path.extension().map_or(false, |e| e == "json") {
            return Ok(serde_json::from_str(&content)?);
        }
        
        // Parse HOCON-style config
        let base_dir = path.parent().unwrap_or(Path::new("."));
        Self::parse_hocon(&content, base_dir)
    }
    
    fn parse_hocon(content: &str, base_dir: &Path) -> Result<Self> {
        // Simplified HOCON parsing - extract key values
        let get_value = |key: &str| -> Option<String> {
            for line in content.lines() {
                let line = line.trim();
                if line.starts_with(key) {
                    if let Some(value) = line.split('=').nth(1) {
                        return Some(value.trim().trim_matches('"').to_string());
                    }
                }
            }
            None
        };
        
        let get_bool = |key: &str| -> bool {
            get_value(key).map_or(false, |v| v == "true")
        };
        
        Ok(Config {
            debug_mode: get_bool("debug-mode"),
            source: SourcePaths {
                js: base_dir.join(get_value("source.js").unwrap_or_else(|| "src/main/assets/js".into())),
                scss: base_dir.join(get_value("source.scss").unwrap_or_else(|| "src/main/assets/scss".into())),
                images: base_dir.join(get_value("source.images").unwrap_or_else(|| "src/main/assets/images".into())),
            },
            output: OutputPaths {
                base: base_dir.join(get_value("output.base").unwrap_or_else(|| "src/main/public".into())),
                js: base_dir.join(get_value("output.js").unwrap_or_else(|| "src/main/public/js".into())),
                css: base_dir.join(get_value("output.css").unwrap_or_else(|| "src/main/public/css".into())),
                images: base_dir.join(get_value("output.images").unwrap_or_else(|| "src/main/public/images".into())),
            },
            manifest_path: base_dir.join(get_value("manifest").unwrap_or_else(|| "src/main/public/manifest.json".into())),
            jam: JamConfig {
                enabled: get_bool("jam.enabled"),
                base_path: get_value("jam.base-path").unwrap_or_else(|| "/paste".into()),
            },
            cdn: CdnConfig {
                enabled: get_bool("cdn.enabled"),
                url: get_value("cdn.url").unwrap_or_default(),
                fallback_to_local: get_bool("cdn.fallback-to-local"),
            },
        })
    }
    
    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug_mode = debug;
        self
    }
    
    pub fn default_for(base_dir: &Path) -> Self {
        Config {
            debug_mode: false,
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

pub fn init_config(project_type: &str) -> Result<()> {
    let config = match project_type {
        "react" => r#"# Paste Surface Configuration (React/Vite)

paste.surface {
  assets {
    debug-mode = false
    
    source {
      js = "src"
      scss = "src/styles"
      images = "public/images"
    }
    
    output {
      base = "dist"
      js = "dist/assets"
      css = "dist/assets"
      images = "dist/images"
    }
    
    manifest = "dist/manifest.json"
    
    jam {
      enabled = true
      base-path = "/paste"
    }
  }
}
"#,
        "scala" => r#"# Paste Surface Configuration (Scala/Twirl)

paste.surface {
  assets {
    debug-mode = false
    
    source {
      js = "src/main/assets/js"
      scss = "src/main/assets/scss"
      images = "src/main/assets/images"
    }
    
    output {
      base = "src/main/public"
      js = "src/main/public/js"
      css = "src/main/public/css"
      images = "src/main/public/images"
    }
    
    manifest = "src/main/public/manifest.json"
    
    jam {
      enabled = true
      base-path = "/paste"
    }
  }
}
"#,
        _ => r#"# Paste Surface Configuration (Basic HTML)

paste.surface {
  assets {
    debug-mode = false
    
    source {
      js = "assets/js"
      scss = "assets/scss"
      images = "assets/images"
    }
    
    output {
      base = "public"
      js = "public/js"
      css = "public/css"
      images = "public/images"
    }
    
    manifest = "public/manifest.json"
    
    jam {
      enabled = true
      base-path = "/paste"
    }
  }
}
"#,
    };
    
    fs::write("paste-surface.conf", config)?;
    println!("Created paste-surface.conf for {} project", project_type);
    Ok(())
}
