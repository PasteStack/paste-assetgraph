use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::Path;

/// Single version of an asset (for rollback history)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetVersion {
    pub hash: String,
    pub version: u32,
    pub output: String,
    pub timestamp: String,
}

/// Single asset entry with version history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetEntry {
    pub source: String,
    pub output: String,
    pub hash: String,
    pub version: u32,
    pub timestamp: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub history: Vec<AssetVersion>,
}

/// Asset manifest with hash-versioned paths
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub generated: String,
    pub assets: HashMap<String, AssetEntry>,
}

impl Manifest {
    /// Create new empty manifest
    pub fn new() -> Self {
        let now: DateTime<Utc> = Utc::now();
        Manifest {
            version: now.format("%Y.%m.%d.%H%M").to_string(),
            generated: now.to_rfc3339(),
            assets: HashMap::new(),
        }
    }
    
    /// Load manifest from file (JSON or bincode)
    pub fn load(path: &Path) -> Result<Self> {
        // Try bincode first (faster)
        let bin_path = path.with_extension("bin");
        if bin_path.exists() {
            let file = File::open(&bin_path)?;
            let reader = BufReader::new(file);
            if let Ok(manifest) = bincode::deserialize_from(reader) {
                return Ok(manifest);
            }
        }
        
        // Fall back to JSON
        if path.exists() {
            let content = fs::read_to_string(path)?;
            return Ok(serde_json::from_str(&content)?);
        }
        
        Ok(Self::new())
    }
    
    /// Save manifest to both JSON and bincode formats
    pub fn save(&self, path: &Path) -> Result<()> {
        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        
        // Save JSON (human readable)
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        
        // Save bincode (fast loading)
        let bin_path = path.with_extension("bin");
        let file = File::create(&bin_path)?;
        let writer = BufWriter::new(file);
        bincode::serialize_into(writer, self)?;
        
        Ok(())
    }
    
    /// Update or create asset entry, preserving version history
    /// Output format: {name}.{hash}.v{version}.min.{ext}
    pub fn update_asset(
        &mut self,
        key: &str,
        source: &str,
        hash: &str,
        output_base: &str,
        ext: &str,
        dependencies: Vec<String>,
    ) -> &AssetEntry {
        let now: DateTime<Utc> = Utc::now();
        let timestamp = now.to_rfc3339();
        
        let existing = self.assets.get(key);
        
        // Check if content changed
        if let Some(entry) = existing {
            if entry.hash == hash {
                return self.assets.get(key).unwrap();
            }
        }
        
        let new_version = existing.map_or(1, |e| e.version + 1);
        
        // Build versioned output path
        let output = format!("{}.{}.v{}.min.{}", output_base, hash, new_version, ext);
        
        // Preserve history for rolling deploys (keep last 5)
        let mut history = existing
            .map(|e| {
                let mut h = e.history.clone();
                h.push(AssetVersion {
                    hash: e.hash.clone(),
                    version: e.version,
                    output: e.output.clone(),
                    timestamp: e.history.last()
                        .map(|v| v.timestamp.clone())
                        .unwrap_or_else(|| self.generated.clone()),
                });
                if h.len() > 5 {
                    h.drain(0..h.len() - 5);
                }
                h
            })
            .unwrap_or_default();
        
        let entry = AssetEntry {
            source: source.to_string(),
            output,
            hash: hash.to_string(),
            version: new_version,
            timestamp: timestamp.clone(),
            dependencies,
            history,
        };
        
        self.assets.insert(key.to_string(), entry);
        self.assets.get(key).unwrap()
    }
    
    /// Get output path for specific version (for rollback)
    pub fn get_version(&self, key: &str, version: Option<u32>) -> Option<&str> {
        let entry = self.assets.get(key)?;
        
        match version {
            None => Some(&entry.output),
            Some(v) if v == entry.version => Some(&entry.output),
            Some(v) => entry.history.iter()
                .find(|h| h.version == v)
                .map(|h| h.output.as_str()),
        }
    }
    
    /// Resolve source path to versioned output path
    pub fn resolve(&self, source_path: &str, debug: bool) -> Option<&str> {
        if debug {
            self.assets.get(source_path).map(|e| e.source.as_str())
        } else {
            self.assets.get(source_path).map(|e| e.output.as_str())
        }
    }
    
    /// Generate JAM URL for bundled modules
    /// Format: /paste/{group_timestamp}/{module+version,module+version,...}.{css|js}
    /// 
    /// The timestamp is based on when THIS GROUP of modules last changed,
    /// not the global build time. This enables cache reuse across deploys
    /// when the specific modules haven't changed.
    pub fn jam_url(&self, modules: &[&str], asset_type: &str, debug: bool) -> String {
        // Collect module entries and find latest timestamp for this group
        let mut latest_timestamp: i64 = 0;
        let mut module_versions: Vec<String> = Vec::new();
        
        for m in modules {
            let key = format!("{}/{}.{}", asset_type, m.replace('.', "/"), asset_type);
            if let Some(entry) = self.assets.get(&key) {
                module_versions.push(format!("{}+v{}.0", m, entry.version));
                
                // Parse timestamp and track the latest
                if let Ok(ts) = entry.timestamp.parse::<i64>() {
                    if ts > latest_timestamp {
                        latest_timestamp = ts;
                    }
                }
            }
        }
        
        // If no valid timestamps, use current manifest version
        let group_timestamp = if latest_timestamp > 0 {
            latest_timestamp.to_string()
        } else {
            self.version.clone()
        };
        
        let suffix = if debug {
            format!(".{}", asset_type)
        } else {
            format!(".min.{}", asset_type)
        };
        
        format!("/paste/{}/{}{}", group_timestamp, module_versions.join(","), suffix)
    }
    
    /// Generate JAM URL for JS modules
    pub fn jam_js_url(&self, modules: &[&str], debug: bool) -> String {
        self.jam_url(modules, "js", debug)
    }
    
    /// Generate JAM URL for CSS modules  
    pub fn jam_css_url(&self, modules: &[&str], debug: bool) -> String {
        self.jam_url(modules, "css", debug)
    }
    
    /// Get all dependencies for a module (recursive)
    pub fn get_dependencies(&self, module: &str) -> Vec<String> {
        let mut visited = std::collections::HashSet::new();
        let mut result = Vec::new();
        
        fn visit(
            manifest: &Manifest,
            module: &str,
            visited: &mut std::collections::HashSet<String>,
            result: &mut Vec<String>,
        ) {
            if visited.contains(module) {
                return;
            }
            visited.insert(module.to_string());
            
            let key = format!("js/{}.js", module.replace('.', "/"));
            if let Some(entry) = manifest.assets.get(&key) {
                for dep in &entry.dependencies {
                    visit(manifest, dep, visited, result);
                }
                result.push(module.to_string());
            }
        }
        
        visit(self, module, &mut visited, &mut result);
        result
    }
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}
