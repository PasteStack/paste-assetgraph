use anyhow::Result;
use chrono::Utc;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;
use walkdir::WalkDir;

use crate::config::Config;
use crate::manifest::Manifest;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BuildMode {
    /// Basic HTML site or Scala/Twirl
    Standard,
    /// React/Vite project
    React,
}

pub struct AssetGraph {
    config: Config,
    mode: BuildMode,
    js_deps: HashMap<String, Vec<String>>,
    module_map: HashMap<String, PathBuf>,
}

impl AssetGraph {
    pub fn new(config: Config) -> Self {
        AssetGraph {
            config,
            mode: BuildMode::Standard,
            js_deps: HashMap::new(),
            module_map: HashMap::new(),
        }
    }
    
    pub fn set_mode(&mut self, mode: BuildMode) {
        self.mode = mode;
    }
    
    pub fn build(&mut self) -> Result<Manifest> {
        // Load existing manifest for version history
        let mut manifest = Manifest::load(&self.config.manifest_path)
            .unwrap_or_else(|_| Manifest::new());
        manifest.generated = Utc::now().to_rfc3339();
        
        // Process JS files
        self.scan_js_files()?;
        let js_order = self.topological_sort()?;
        
        for module in &js_order {
            if let Some(source_path) = self.module_map.get(module) {
                self.process_js(source_path.clone(), module, &mut manifest)?;
            }
        }
        
        // Process SCSS files
        if self.config.source.scss.exists() {
            for entry in WalkDir::new(&self.config.source.scss)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map_or(false, |ext| ext == "scss"))
            {
                let path = entry.path();
                // Skip partials (files starting with _)
                if path.file_name()
                    .and_then(|n| n.to_str())
                    .map_or(false, |n| n.starts_with('_'))
                {
                    continue;
                }
                self.process_scss(path, &mut manifest)?;
            }
        }
        
        // Save manifest (both JSON and bincode)
        manifest.save(&self.config.manifest_path)?;
        
        Ok(manifest)
    }
    
    fn scan_js_files(&mut self) -> Result<()> {
        let requires_re = Regex::new(r"@requires\s+([\w./]+)")?;
        let module_re = Regex::new(r"@module\s+([\w.]+)")?;
        
        if !self.config.source.js.exists() {
            return Ok(());
        }
        
        for entry in WalkDir::new(&self.config.source.js)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "js"))
        {
            let path = entry.path();
            let content = fs::read_to_string(path)?;
            
            // Find module name
            let module_name = if let Some(caps) = module_re.captures(&content) {
                caps.get(1).unwrap().as_str().to_string()
            } else {
                // Derive from path
                path.strip_prefix(&self.config.source.js)?
                    .with_extension("")
                    .to_string_lossy()
                    .replace('/', ".")
            };
            
            // Find dependencies
            let deps: Vec<String> = requires_re
                .captures_iter(&content)
                .map(|c| c.get(1).unwrap().as_str().to_string())
                .collect();
            
            self.js_deps.insert(module_name.clone(), deps);
            self.module_map.insert(module_name, path.to_path_buf());
        }
        
        Ok(())
    }
    
    fn topological_sort(&self) -> Result<Vec<String>> {
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        let mut temp = HashSet::new();
        
        fn visit(
            module: &str,
            deps: &HashMap<String, Vec<String>>,
            module_map: &HashMap<String, PathBuf>,
            visited: &mut HashSet<String>,
            temp: &mut HashSet<String>,
            result: &mut Vec<String>,
        ) -> Result<()> {
            if temp.contains(module) {
                anyhow::bail!("Circular dependency detected: {}", module);
            }
            if visited.contains(module) {
                return Ok(());
            }
            
            temp.insert(module.to_string());
            
            if let Some(module_deps) = deps.get(module) {
                for dep in module_deps {
                    if module_map.contains_key(dep) {
                        visit(dep, deps, module_map, visited, temp, result)?;
                    }
                }
            }
            
            temp.remove(module);
            visited.insert(module.to_string());
            result.push(module.to_string());
            
            Ok(())
        }
        
        for module in self.js_deps.keys() {
            visit(module, &self.js_deps, &self.module_map, &mut visited, &mut temp, &mut result)?;
        }
        
        Ok(result)
    }
    
    fn process_js(&self, source_path: PathBuf, module: &str, manifest: &mut Manifest) -> Result<()> {
        let content = fs::read_to_string(&source_path)?;
        let hash = format!("{:x}", md5::compute(content.as_bytes()));
        
        let key = format!("js/{}.js", module.replace('.', "/"));
        let rel_path = module.replace('.', "/");
        let output_base = format!("/js/{}", rel_path);
        
        let deps = self.js_deps.get(module).cloned().unwrap_or_default();
        
        let entry = manifest.update_asset(
            &key,
            source_path.to_string_lossy().as_ref(),
            &hash,
            &output_base,
            "js",
            deps,
        );
        
        // Determine output path
        let output_name = entry.output.trim_start_matches("/js/");
        let output_path = self.config.output.js.join(output_name);
        
        // Minify unless debug mode
        let output_content = if self.config.debug_mode {
            content
        } else {
            // TODO: Use swc for minification
            // For now, just use the original content
            content
        };
        
        // Write output
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output_path, output_content)?;
        
        Ok(())
    }
    
    fn process_scss(&self, source_path: &Path, manifest: &mut Manifest) -> Result<()> {
        let content = fs::read_to_string(source_path)?;
        
        // Compile SCSS using grass
        let css_content = grass::from_string(
            content,
            &grass::Options::default()
                .load_path(&self.config.source.scss),
        )?;
        
        let hash = format!("{:x}", md5::compute(css_content.as_bytes()));
        
        let rel_path = source_path.strip_prefix(&self.config.source.scss)?;
        let base_name = rel_path.with_extension("").to_string_lossy().to_string();
        let key = format!("css/{}.css", base_name);
        let output_base = format!("/css/{}", base_name);
        
        let entry = manifest.update_asset(
            &key,
            source_path.to_string_lossy().as_ref(),
            &hash,
            &output_base,
            "css",
            vec![],
        );
        
        // Determine output path
        let output_name = entry.output.trim_start_matches("/css/");
        let output_path = self.config.output.css.join(output_name);
        
        // Minify unless debug mode
        let output_content = if self.config.debug_mode {
            css_content
        } else {
            // TODO: Use lightningcss for minification
            css_content
        };
        
        // Write output
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output_path, output_content)?;
        
        Ok(())
    }
}

/// Watch for file changes and rebuild
pub async fn watch(config: Config) -> Result<()> {
    let (tx, rx) = channel();
    
    let mut watcher: RecommendedWatcher = Watcher::new(
        tx,
        notify::Config::default().with_poll_interval(Duration::from_secs(1)),
    )?;
    
    println!("Watching for changes in:");
    println!("  {:?}", config.source.js);
    println!("  {:?}", config.source.scss);
    println!("Press Ctrl+C to stop");
    
    watcher.watch(&config.source.js, RecursiveMode::Recursive)?;
    watcher.watch(&config.source.scss, RecursiveMode::Recursive)?;
    
    // Initial build
    let mut graph = AssetGraph::new(config.clone());
    graph.build()?;
    println!("Initial build complete");
    
    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                if event.paths.iter().any(|p| {
                    p.extension().map_or(false, |e| e == "js" || e == "scss" || e == "css")
                }) {
                    println!("\nFile changed: {:?}", event.paths);
                    let mut graph = AssetGraph::new(config.clone());
                    if let Err(e) = graph.build() {
                        eprintln!("Build error: {}", e);
                    } else {
                        println!("Rebuild complete");
                    }
                }
            }
            Ok(Err(e)) => eprintln!("Watch error: {}", e),
            Err(e) => eprintln!("Channel error: {}", e),
        }
    }
}
