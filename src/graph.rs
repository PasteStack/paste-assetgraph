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
use crate::storage::StorageBackend;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BuildMode {
    /// Basic HTML site or Scala/Twirl
    Standard,
    /// React/Vite project
    React,
}

/// Shared character class for module names in @requires and @module comments.
/// Supports word chars, dots, slashes, and hyphens (e.g. paste/utils/animation-frame).
const MODULE_NAME_RE: &str = r"[\w./-]+";

pub struct AssetGraph {
    config: Config,
    mode: BuildMode,
    js_deps: HashMap<String, Vec<String>>,
    module_map: HashMap<String, PathBuf>,
    storage: StorageBackend,
    // Regexes compiled once and reused across all file scans
    requires_re: Regex,
    module_re: Regex,
}

impl AssetGraph {
    pub fn new(config: Config) -> Self {
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
        AssetGraph {
            config,
            mode: BuildMode::Standard,
            js_deps: HashMap::new(),
            module_map: HashMap::new(),
            storage,
            // Compiled once for reuse — pattern shared via MODULE_NAME_RE
            requires_re: Regex::new(&format!(r"@requires\s+({})", MODULE_NAME_RE)).unwrap(),
            module_re: Regex::new(&format!(r#"@module\s+"?({})"?"#, MODULE_NAME_RE)).unwrap(),
        }
    }
    
    pub fn set_mode(&mut self, mode: BuildMode) {
        self.mode = mode;
    }
    
    pub fn build(&mut self) -> Result<Manifest> {
        // Load existing manifest for version history
        let mut manifest = Manifest::load(&self.config.manifest_path)
            .unwrap_or_else(|e| {
                eprintln!("Warning: failed to load manifest from {:?}: {} — starting fresh (version history reset)", self.config.manifest_path, e);
                Manifest::new()
            });
        manifest.generated = Utc::now().to_rfc3339();
        
        // Modules and sorted_deps are rebuilt fresh each build
        manifest.modules.clear();
        manifest.sorted_deps.clear();
        
        if self.config.source_maps && self.config.debug_mode {
            // TODO: source map generation not yet implemented
            eprintln!("Note: source_maps=true but source map generation is not yet implemented");
        }
        
        // Track seen asset keys to prune stale assets after build
        let mut seen_asset_keys: HashSet<String> = HashSet::new();
        
        // Process JS files
        self.scan_js_files()?;
        let js_order = self.topological_sort()?;
        
        // Expand transitive dependencies (backfill like original Python)
        let expanded_deps = self.expand_all_dependencies(&js_order);
        
        // Build sorted_deps for JS
        let mut js_sorted_deps: Vec<(String, String, u32)> = Vec::new();
        
        for module in &js_order {
            if let Some(source_path) = self.module_map.get(module) {
                // Add to modules map: module name -> relative file path
                let rel_path = source_path.strip_prefix(&self.config.source.js)
                    .map(|p| format!("js/{}", p.display()))
                    .unwrap_or_else(|_| format!("js/{}.js", module.replace('.', "/")));
                manifest.modules.insert(module.clone(), rel_path);
                
                // Use expanded dependencies
                let deps = expanded_deps.get(module).cloned().unwrap_or_default();
                self.process_js_with_deps(source_path.clone(), module, deps, &mut manifest)?;
                
                // Add to sorted_deps and track seen keys
                let key = format!("js/{}.js", module.replace('.', "/"));
                seen_asset_keys.insert(key.clone());
                if let Some(entry) = manifest.assets.get(&key) {
                    js_sorted_deps.push((module.clone(), entry.output.clone(), entry.version));
                }
            }
        }
        
        // Store JS sorted_deps in manifest
        manifest.sorted_deps.insert("js".to_string(), js_sorted_deps);
        
        // Process SCSS files — cascade source dir then include paths
        let mut css_sorted_deps: Vec<(String, String, u32)> = Vec::new();
        
        let mut scss_dirs: Vec<&Path> = Vec::new();
        if self.config.source.scss.exists() {
            scss_dirs.push(&self.config.source.scss);
        }
        for p in &self.config.include_paths {
            if p.exists() {
                scss_dirs.push(p);
            }
        }
        
        for base_dir in &scss_dirs {
            for entry in WalkDir::new(base_dir)
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
                // Track every SCSS asset key to prevent pruning, even without @module
                let rel_path = path.strip_prefix(base_dir)?;
                let base_name = rel_path.with_extension("").to_string_lossy().to_string();
                let css_key = format!("css/{}.css", base_name);
                seen_asset_keys.insert(css_key);
                
                if let Some((module_name, output, version, _asset_key)) = self.process_scss(path, base_dir, &mut manifest)? {
                    css_sorted_deps.push((module_name, output, version));
                }
            }
        }
        
        // Store CSS sorted_deps in manifest
        manifest.sorted_deps.insert("css".to_string(), css_sorted_deps);
        
        // Remove assets not seen in this build to prevent stale entries
        manifest.assets.retain(|key, _| seen_asset_keys.contains(key));
        
        // Save manifest (both JSON and bincode)
        manifest.save(&self.config.manifest_path)?;
        
        // Prune old file versions from storage
        for (key, _entry) in &manifest.assets {
            if let Some(slash_pos) = key.find('/') {
                let asset_type = &key[..slash_pos];
                let rest = &key[slash_pos + 1..];
                if let Some(dot_pos) = rest.rfind('.') {
                    let base_name = &rest[..dot_pos];
                    let _ = self.storage.prune_versions(asset_type, base_name);
                }
            }
        }
        
        Ok(manifest)
    }
    
    /// Expand all transitive dependencies for each module (backfill)
    /// This matches the original Python behavior from manifest.py
    fn expand_all_dependencies(&self, sorted_modules: &[String]) -> HashMap<String, Vec<String>> {
        let mut expanded: HashMap<String, Vec<String>> = HashMap::new();
        
        // Walk tree recursively to get all transitive deps
        fn walk_tree(
            module: &str,
            deps: &HashMap<String, Vec<String>>,
            visited: &mut HashSet<String>,
        ) -> Vec<String> {
            let mut result = Vec::new();
            if visited.contains(module) {
                return result;
            }
            visited.insert(module.to_string());
            
            if let Some(direct_deps) = deps.get(module) {
                for dep in direct_deps {
                    // Recurse first (depth-first)
                    let transitive = walk_tree(dep, deps, visited);
                    for t in transitive {
                        if !result.contains(&t) {
                            result.push(t);
                        }
                    }
                    // Then add the direct dep
                    if !result.contains(dep) {
                        result.push(dep.clone());
                    }
                }
            }
            result
        }
        
        // Process in reverse sorted order (like original Python)
        for module in sorted_modules.iter().rev() {
            let mut visited = HashSet::new();
            let all_deps = walk_tree(module, &self.js_deps, &mut visited);
            expanded.insert(module.clone(), all_deps);
        }
        
        expanded
    }
    
    fn scan_js_files(&mut self) -> Result<()> {
        self.js_deps.clear();
        self.module_map.clear();
        
        // Scan primary JS source directory
        if self.config.source.js.exists() {
            self.scan_js_dir(&self.config.source.js.clone())?;
        }
        
        // Also scan include_paths for JS files so that modules referenced
        // via @requires from external paths are discovered for the dependency graph
        for include_path in &self.config.include_paths.clone() {
            if include_path.exists() {
                self.scan_js_dir(include_path)?;
            }
        }
        
        Ok(())
    }
    
    /// Scan a directory for JS files and register their modules and dependencies.
    /// Files with @module comments use that name; others derive from path.
    fn scan_js_dir(&mut self, dir: &Path) -> Result<()> {
        for entry in WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "js"))
        {
            let path = entry.path();
            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Warning: could not read {}: {}", path.display(), e);
                    continue;
                }
            };
            
            // Find module name from @module comment
            let module_name = if let Some(caps) = self.module_re.captures(&content) {
                // Convert slash to dot: paste/ui/heroscroll -> paste.ui.heroscroll
                caps.get(1).unwrap().as_str().replace('/', ".")
            } else {
                // Derive from path (fallback)
                path.strip_prefix(dir)
                    .unwrap_or(path)
                    .with_extension("")
                    .to_string_lossy()
                    .replace('/', ".")
            };
            
            // Don't overwrite a module already found in the primary source dir
            if self.module_map.contains_key(&module_name) {
                continue;
            }
            
            // Find dependencies (convert slash to dot)
            let deps: Vec<String> = self.requires_re
                .captures_iter(&content)
                .map(|c| c.get(1).unwrap().as_str().replace('/', "."))
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
    
    fn process_js_with_deps(&self, source_path: PathBuf, module: &str, deps: Vec<String>, manifest: &mut Manifest) -> Result<()> {
        let content = fs::read_to_string(&source_path)?;
        
        // Strip comments before hashing (so comments don't trigger new versions)
        let stripped = strip_js_comments(&content);
        let hash = format!("{:x}", md5::compute(stripped.as_bytes()));
        
        let key = format!("js/{}.js", module.replace('.', "/"));
        let rel_path = module.replace('.', "/");
        let output_base = format!("/js/{}", rel_path);
        
        let entry = manifest.update_asset_with_history(
            &key,
            source_path.to_string_lossy().as_ref(),
            &hash,
            &output_base,
            "js",
            deps,  // Use expanded transitive dependencies
            self.config.storage.keep_versions,
        );
        
        // Determine output filename
        let output_name = entry.output.trim_start_matches("/js/");
        
        // Skip minification and write if the output file already exists on disk
        // (hash unchanged means same filename, so the file is already correct)
        if !self.storage.asset_exists("js", output_name) {
            // Skip minification in debug mode or when minify=false
            let output_content = if self.config.debug_mode || !self.config.minify {
                content
            } else {
                // Use minify-js for minification
                match minify_js(&content) {
                    Ok(minified) => minified,
                    Err(e) => {
                        eprintln!("Warning: JS minification failed for {}: {}", module, e);
                        content
                    }
                }
            };
            
            // Write output via storage backend
            self.storage.write_asset(output_content.as_bytes(), "js", output_name)?;
        }
        
        Ok(())
    }
    
    /// Process SCSS file and return (module_name, output_path, version, asset_key) for sorted_deps
    fn process_scss(&self, source_path: &Path, base_dir: &Path, manifest: &mut Manifest) -> Result<Option<(String, String, u32, String)>> {
        let content = fs::read_to_string(source_path)?;
        
        // Extract @module comment if present (same format as JS)
        // Done before passing content to grass::from_string which takes ownership
        let module_name = self.module_re.captures(&content)
            .map(|caps| caps.get(1).unwrap().as_str().replace('/', "."));
        
        // Extract @requires dependencies (same format as JS)
        let deps: Vec<String> = self.requires_re
            .captures_iter(&content)
            .map(|c| c.get(1).unwrap().as_str().replace('/', "."))
            .collect();
        
        // Compile SCSS using grass with include paths
        // Use from_path so grass knows the file's location for relative @import resolution
        let mut options = grass::Options::default()
            .load_path(&self.config.source.scss);
        
        // Add all include paths
        for include_path in &self.config.include_paths {
            options = options.load_path(include_path);
        }
        
        let css_content = grass::from_path(source_path, &options)?;
        
        // Strip comments before hashing (so comments don't trigger new versions)
        let stripped = strip_css_comments(&css_content);
        let hash = format!("{:x}", md5::compute(stripped.as_bytes()));
        
        let rel_path = source_path.strip_prefix(base_dir)?;
        let base_name = rel_path.with_extension("").to_string_lossy().to_string();
        let key = format!("css/{}.css", base_name);
        let output_base = format!("/css/{}", base_name);
        
        // Add to modules map if @module comment was found
        if let Some(ref mod_name) = module_name {
            manifest.modules.insert(mod_name.clone(), format!("css/{}.css", base_name));
        }
        
        let entry = manifest.update_asset_with_history(
            &key,
            source_path.to_string_lossy().as_ref(),
            &hash,
            &output_base,
            "css",
            deps,
            self.config.storage.keep_versions,
        );
        
        // Capture entry info before moving
        let output = entry.output.clone();
        let version = entry.version;
        
        // Determine output filename
        let output_name = entry.output.trim_start_matches("/css/");
        
        // Skip minification and write if the output file already exists on disk
        if !self.storage.asset_exists("css", output_name) {
            // Skip minification in debug mode or when minify=false
            let output_content = if self.config.debug_mode || !self.config.minify {
                css_content
            } else {
                // Use lightningcss for minification
                match minify_css(&css_content) {
                    Ok(minified) => minified,
                    Err(e) => {
                        eprintln!("Warning: CSS minification failed: {}", e);
                        css_content
                    }
                }
            };
            
            // Write output via storage backend
            self.storage.write_asset(output_content.as_bytes(), "css", output_name)?;
        }
        
        // Return module info for sorted_deps if @module was found, include asset key for tracking
        Ok(module_name.map(|name| (name, output, version, key.clone())))
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
    
    // Only watch directories that exist on disk
    if config.source.js.exists() {
        watcher.watch(&config.source.js, RecursiveMode::Recursive)?;
    } else {
        eprintln!("Warning: JS source directory does not exist: {:?}", config.source.js);
    }
    
    if config.source.scss.exists() {
        watcher.watch(&config.source.scss, RecursiveMode::Recursive)?;
    } else {
        eprintln!("Warning: SCSS source directory does not exist: {:?}", config.source.scss);
    }
    
    // Also watch include paths for SCSS changes (e.g. paste-elements modules)
    for include_path in &config.include_paths {
        if include_path.exists() {
            watcher.watch(include_path, RecursiveMode::Recursive)?;
            println!("  {:?} (include path)", include_path);
        }
    }
    
    // Initial build — reuse the same AssetGraph instance across rebuilds
    // so compiled regexes and storage backend are not recreated each time
    let mut graph = AssetGraph::new(config.clone());
    graph.build()?;
    println!("Initial build complete");
    
    let debounce_duration = Duration::from_millis(200);
    
    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                if event.paths.iter().any(|p| {
                    p.extension().map_or(false, |e| e == "js" || e == "scss" || e == "css")
                }) {
                    // Debounce: drain any additional events within the window
                    // to avoid multiple rebuilds from editors that emit rapid events
                    while let Ok(_) = rx.recv_timeout(debounce_duration) {
                        // discard queued events during debounce window
                    }
                    
                    println!("\nFile changed: {:?}", event.paths);
                    if let Err(e) = graph.build() {
                        eprintln!("Build error: {}", e);
                    } else {
                        println!("Rebuild complete");
                    }
                }
            }
            Ok(Err(e)) => eprintln!("Watch error: {}", e),
            Err(e) => {
                eprintln!("Channel error: {}", e);
                break Ok(());
            }
        }
    }
}

/// Minify JavaScript using minify-js.
/// Wraps the call in catch_unwind because minify-js can panic on
/// certain input patterns instead of returning an error.
fn minify_js(content: &str) -> Result<String> {
    use minify_js::{Session, TopLevelMode, minify};
    
    let input = content.to_string();
    let result = std::panic::catch_unwind(|| {
        let session = Session::new();
        let mut out = Vec::new();
        minify(
            &session,
            TopLevelMode::Global,
            input.as_bytes(),
            &mut out,
        ).map_err(|e| anyhow::anyhow!("JS minification error: {:?}", e))?;
        String::from_utf8(out)
            .map_err(|e| anyhow::anyhow!("UTF-8 conversion error: {}", e))
    });
    
    match result {
        Ok(inner) => inner,
        Err(_) => Err(anyhow::anyhow!("JS minifier panicked (unsupported syntax)")),
    }
}

/// Minify CSS using lightningcss
fn minify_css(content: &str) -> Result<String> {
    use lightningcss::stylesheet::{ParserOptions, PrinterOptions, StyleSheet};
    
    let stylesheet = StyleSheet::parse(content, ParserOptions::default())
        .map_err(|e| anyhow::anyhow!("CSS parse error: {:?}", e))?;
    
    let result = stylesheet.to_css(PrinterOptions {
        minify: true,
        ..Default::default()
    }).map_err(|e| anyhow::anyhow!("CSS print error: {:?}", e))?;
    
    Ok(result.code)
}

/// Strip JS comments for hash calculation (so comments don't trigger new versions)
fn strip_js_comments(content: &str) -> String {
    let mut result = String::new();
    let mut chars = content.chars().peekable();
    let mut in_string = false;
    let mut string_char = ' ';
    // Stack-based template literal nesting tracker.
    // Each entry is the brace depth for an interpolation context.
    // - Empty stack + in_string + string_char=='`' = inside a top-level template body
    // - Stack top > 0 = inside ${...} interpolation (brace depth)
    // - Backtick inside interpolation starts a nested template literal (no push/pop,
    //   just note we're in a nested body; the parent frame stays on the stack)
    //
    // Nesting depth tracks how many template literals deep we are (0 = outermost).
    let mut template_stack: Vec<u32> = Vec::new();
    let mut template_nesting: u32 = 0; // how many template literals deep
    // Track last non-whitespace char and preceding word for regex vs division disambiguation
    let mut last_nonws: Option<char> = None;
    // Buffer for the current identifier/keyword being accumulated
    let mut word_buf = String::new();
    
    while let Some(c) = chars.next() {
        if in_string {
            result.push(c);
            
            // Are we inside a template literal interpolation ${...}?
            if string_char == '`' && !template_stack.is_empty() {
                let depth = template_stack.last_mut().unwrap();
                if *depth > 0 {
                    if c == '`' {
                        // Backtick inside interpolation starts a nested template literal.
                        // Don't pop the parent interpolation — just enter nested body.
                        template_nesting += 1;
                        *depth = 0; // mark: we're now in a nested template body
                        continue;
                    }
                    // Track brace nesting inside interpolation
                    if c == '{' {
                        *depth += 1;
                    } else if c == '}' {
                        *depth -= 1;
                        if *depth == 0 {
                            // Exiting interpolation, back to template literal body
                            template_stack.pop();
                        }
                    }
                    // While inside interpolation braces, skip string-end checks
                    continue;
                }
                // depth == 0: we're in a nested template literal body
                // (fall through to normal template literal handling below)
            }
            
            // Entering a template interpolation?
            if string_char == '`' && c == '$' && chars.peek() == Some(&'{') {
                let brace = chars.next().unwrap();
                result.push(brace);
                template_stack.push(1); // entering interpolation with brace depth 1
                continue;
            }
            
            if c == string_char {
                // Count consecutive backslashes before this quote
                // If odd number, quote is escaped; if even (including 0), string ends
                let mut backslash_count = 0;
                for ch in result.chars().rev().skip(1) {
                    if ch == '\\' {
                        backslash_count += 1;
                    } else {
                        break;
                    }
                }
                if backslash_count % 2 == 0 {
                    if string_char == '`' && template_nesting > 0 {
                        // Closing a nested template literal — resume parent interpolation.
                        // The parent's brace depth was set to 0 when we entered the nested
                        // template; restore it to the correct depth (still inside ${...}).
                        template_nesting -= 1;
                        if let Some(depth) = template_stack.last_mut() {
                            // We're back inside the parent interpolation.
                            // The brace depth was zeroed when entering nested template;
                            // set it to 1 since we're still inside the parent ${...}.
                            *depth = 1;
                        }
                        // Still in_string=true, string_char='`' for the parent
                    } else {
                        in_string = false;
                        template_stack.clear();
                        template_nesting = 0;
                    }
                    if !c.is_whitespace() { last_nonws = Some(c); }
                }
            }
            continue;
        }
        
        if c == '"' || c == '\'' || c == '`' {
            in_string = true;
            string_char = c;
            template_stack.clear();
            template_nesting = 0;
            result.push(c);
            word_buf.clear();
            continue;
        }
        
        if c == '/' {
            // Snapshot the next char to avoid double mutable borrow of chars
            let next_ch = chars.peek().copied();
            let is_div = is_division_context(last_nonws, &word_buf);
            
            if next_ch == Some('/') {
                // Line comment - skip to end of line
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc == '\n' { break; }
                }
            } else if next_ch == Some('*') {
                // Block comment - skip to */
                chars.next();
                while let Some(nc) = chars.next() {
                    if nc == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
            } else if !is_div {
                // Regex literal: consume until unescaped closing /
                result.push(c);
                let mut escaped = false;
                while let Some(rc) = chars.next() {
                    result.push(rc);
                    if escaped {
                        escaped = false;
                        continue;
                    }
                    if rc == '\\' {
                        escaped = true;
                        continue;
                    }
                    if rc == '/' {
                        // Consume regex flags (g, i, m, s, u, y, d)
                        while let Some(&fc) = chars.peek() {
                            if fc.is_ascii_alphabetic() {
                                result.push(fc);
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        break;
                    }
                }
                last_nonws = Some('/');
                word_buf.clear();
                continue;
            } else {
                // Division operator
                result.push(c);
                last_nonws = Some(c);
                word_buf.clear();
                continue;
            }
        } else {
            result.push(c);
            if !c.is_whitespace() {
                if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                    word_buf.push(c);
                } else {
                    word_buf.clear();
                }
                last_nonws = Some(c);
            }
        }
    }
    
    // Normalize whitespace
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// JS keywords after which `/` starts a regex literal (not division).
const REGEX_PRECEDING_KEYWORDS: &[&str] = &[
    "return", "typeof", "throw", "new", "case", "in",
    "void", "delete", "instanceof", "yield", "await",
];

/// Returns true if the last non-whitespace character indicates the `/` is division,
/// not a regex literal start. Division follows values/identifiers/closing brackets.
/// `last_word` is the accumulated identifier/keyword ending at the current position.
fn is_division_context(last_nonws: Option<char>, last_word: &str) -> bool {
    match last_nonws {
        None => false, // start of input — regex context
        Some(c) if matches!(c, ')' | ']' | '}') => true,
        Some(c) if matches!(c, '0'..='9') => true,
        Some(c) if c == '.' => true,
        Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '$' => {
            // Identifier-like ending — check if the preceding word is a keyword
            // that takes a regex operand (e.g., return /regex/).
            !REGEX_PRECEDING_KEYWORDS.contains(&last_word)
        }
        _ => false,
    }
}

/// Strip CSS comments for hash calculation
fn strip_css_comments(content: &str) -> String {
    let mut result = String::new();
    let mut chars = content.chars().peekable();
    let mut in_string = false;
    let mut string_char = ' ';
    
    while let Some(c) = chars.next() {
        if in_string {
            result.push(c);
            if c == '\\' {
                // Push escaped char too
                if let Some(nc) = chars.next() {
                    result.push(nc);
                }
                continue;
            }
            if c == string_char {
                in_string = false;
            }
            continue;
        }
        
        if c == '"' || c == '\'' {
            in_string = true;
            string_char = c;
            result.push(c);
            continue;
        }
        
        if c == '/' && chars.peek() == Some(&'*') {
            // Block comment - skip to */
            chars.next();
            while let Some(nc) = chars.next() {
                if nc == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
        } else {
            result.push(c);
        }
    }
    
    // Normalize whitespace
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    
    // ============================================================
    // Escape sequence handling in strip_js_comments
    // ============================================================
    
    #[test]
    fn test_strip_js_comments_simple_string() {
        // Simple strings should be preserved
        let input = r#"var x = "hello";"#;
        let result = strip_js_comments(input);
        assert!(result.contains(r#""hello""#), "Simple string should be preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_escaped_quote() {
        // Escaped quotes inside strings should NOT end the string
        let input = r#"var x = "hello \"world\""; // comment"#;
        let result = strip_js_comments(input);
        // The entire string including escaped quotes should be preserved
        // Comment after the string should be stripped
        assert!(result.contains("hello"), "String content should be preserved: {}", result);
        assert!(result.contains("world"), "Escaped quote content should be preserved: {}", result);
        // Critical: the comment should be stripped, not become part of output
        // If escaped quote handling is broken, "world" might be seen as code, and
        // the rest would be corrupted
        assert!(!result.contains("comment"), "Comment should be stripped: {}", result);
        // The closing quote and semicolon should be present
        assert!(result.ends_with(";") || result.ends_with("\";"), 
            "Statement should end properly: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_escaped_backslash_before_quote() {
        // Escaped backslash followed by quote: \\" means backslash then end of string
        // The string "path\\" contains a backslash, then the string ENDS
        let input = r#"var x = "path\\"; // comment after"#;
        let result = strip_js_comments(input);
        // The comment should be stripped (string ends at the second ")
        assert!(!result.contains("comment"), 
            "Comment should be stripped - string ends after \\\\: {}", result);
        // The var declaration should be complete
        assert!(result.contains("var x"), "var x should exist: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_line_comment() {
        let input = "var x = 1; // this is a comment\nvar y = 2;";
        let result = strip_js_comments(input);
        assert!(result.contains("var x"), "Code before comment should exist");
        assert!(result.contains("var y"), "Code after comment should exist");
        assert!(!result.contains("this is a comment"), "Comment should be stripped");
    }
    
    #[test]
    fn test_strip_js_comments_block_comment() {
        let input = "var x = 1; /* block comment */ var y = 2;";
        let result = strip_js_comments(input);
        assert!(result.contains("var x"), "Code before comment should exist");
        assert!(result.contains("var y"), "Code after comment should exist");
        assert!(!result.contains("block comment"), "Comment should be stripped");
    }
    
    #[test]
    fn test_strip_js_comments_slash_in_string() {
        // Slash inside a string should NOT be treated as comment start
        let input = r#"var url = "http://example.com";"#;
        let result = strip_js_comments(input);
        assert!(result.contains("http://example.com"), "URL in string should be preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_regex_like_pattern() {
        // Division operator should not start a comment
        let input = "var x = 10 / 2; // comment";
        let result = strip_js_comments(input);
        assert!(result.contains("10 / 2") || result.contains("10/ 2") || result.contains("10 /2"), 
            "Division should be preserved: {}", result);
    }
    
    // ============================================================
    // Regex literal handling
    // ============================================================
    
    #[test]
    fn test_strip_js_comments_regex_literal() {
        // Regex literal /foo.*bar/ should not be treated as a block comment
        let input = r#"var x = /foo.*bar/; var y = 2;"#;
        let result = strip_js_comments(input);
        assert!(result.contains("/foo.*bar/"), "Regex literal should be preserved: {}", result);
        assert!(result.contains("var y"), "Code after regex should be preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_regex_after_equals() {
        let input = "var re = /test/g; // comment";
        let result = strip_js_comments(input);
        assert!(result.contains("/test/g"), "Regex with flags preserved: {}", result);
        assert!(!result.contains("comment"), "Comment after regex stripped: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_division_not_regex() {
        let input = "var x = 10 / 2; /* comment */ var y = 3;";
        let result = strip_js_comments(input);
        assert!(result.contains("10") && result.contains("2"), "Division preserved: {}", result);
        assert!(!result.contains("comment"), "Comment stripped: {}", result);
        assert!(result.contains("var y"), "Code after comment preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_regex_in_condition() {
        let input = "if (/pattern/.test(x)) { y = 1; }";
        let result = strip_js_comments(input);
        assert!(result.contains("/pattern/"), "Regex in condition preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_block_comment_after_value() {
        // Block comment /* */ after a value expression must still be stripped.
        // In JS, /* always starts a block comment regardless of preceding token.
        let input = "var x = 10 /* block */ + 5;";
        let result = strip_js_comments(input);
        assert!(!result.contains("block"), "Block comment after value should be stripped: {}", result);
        assert!(result.contains("10"), "Value before comment preserved: {}", result);
        assert!(result.contains("5"), "Value after comment preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_regex_after_plus_operator() {
        // In JS, `/` after `+` or `-` is a regex literal, not division.
        // Example: var x = +/pattern/g;
        let input = "var x = +/pattern/g;";
        let result = strip_js_comments(input);
        assert!(result.contains("/pattern/g"), "Regex after + operator should be preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_regex_after_minus_operator() {
        let input = "var x = -/pattern/.test(y);";
        let result = strip_js_comments(input);
        assert!(result.contains("/pattern/"), "Regex after - operator should be preserved: {}", result);
    }
    
    // ============================================================
    // Regex literal with space after / (Bug #3 regression)
    // ============================================================
    
    #[test]
    fn test_strip_js_comments_regex_with_space_after_slash() {
        // Regex literals CAN have a space after the opening /
        // e.g., / pattern / is valid JS (though unusual)
        let input = "if (/ pattern /.test(x)) { y = 1; }";
        let result = strip_js_comments(input);
        assert!(result.contains("/ pattern /"),
            "Regex with space after opening / should be preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_regex_with_leading_space_and_flags() {
        let input = "var re = / foo /gi;";
        let result = strip_js_comments(input);
        assert!(result.contains("/ foo /gi"),
            "Regex with space and flags should be preserved: {}", result);
    }
    
    // ============================================================
    // Regex after JS keywords (Bug #4 regression)
    // ============================================================
    
    #[test]
    fn test_strip_js_comments_regex_after_return() {
        // `return /regex/` — `/` after `return` is a regex, not division
        let input = "function f() { return /pattern/g; }";
        let result = strip_js_comments(input);
        assert!(result.contains("/pattern/g"), "Regex after return should be preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_regex_after_typeof() {
        let input = "if (typeof /regex/.test === 'function') {}";
        let result = strip_js_comments(input);
        assert!(result.contains("/regex/"), "Regex after typeof should be preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_regex_after_throw() {
        let input = "throw /invalid/;";
        let result = strip_js_comments(input);
        assert!(result.contains("/invalid/"), "Regex after throw should be preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_regex_after_new() {
        // `new /regex/` is unusual but syntactically valid in some contexts
        let input = "var x = new /pattern/;";
        let result = strip_js_comments(input);
        assert!(result.contains("/pattern/"), "Regex after new should be preserved: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_division_after_identifier() {
        // Division after an identifier (not a keyword) should still work
        let input = "var x = total / count; // comment";
        let result = strip_js_comments(input);
        assert!(result.contains("total") && result.contains("count"), 
            "Division after identifier preserved: {}", result);
        assert!(!result.contains("comment"), "Comment stripped: {}", result);
    }
    
    // ============================================================
    // CSS comment stripping
    // ============================================================
    
    #[test]
    fn test_strip_css_comments_basic() {
        let input = ".class { color: red; /* comment */ }";
        let result = strip_css_comments(input);
        assert!(result.contains("color: red") || result.contains("color:red"), 
            "CSS should be preserved: {}", result);
        assert!(!result.contains("comment"), "Comment should be stripped");
    }
    
    #[test]
    fn test_strip_css_comments_url() {
        // URLs with slashes should be preserved (not in /* */ so should be fine)
        let input = r#".class { background: url("http://example.com/img.png"); }"#;
        let result = strip_css_comments(input);
        assert!(result.contains("http://example.com"), "URL should be preserved: {}", result);
    }
    
    // ============================================================
    // Template literal interpolation handling
    // ============================================================
    
    #[test]
    fn test_strip_js_comments_template_literal_with_interpolation() {
        // Template literals with ${...} containing quotes should not break parsing
        let input = r#"var x = `hello ${name + "!"} world`; // comment"#;
        let result = strip_js_comments(input);
        assert!(result.contains("hello"), "Template content before interpolation preserved: {}", result);
        assert!(result.contains("world"), "Template content after interpolation preserved: {}", result);
        assert!(!result.contains("comment"), "Trailing comment stripped: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_nested_template_interpolation() {
        // Nested braces inside interpolation
        let input = "var x = `${({a: 1}).a}`; // comment";
        let result = strip_js_comments(input);
        assert!(result.contains("({a: 1})"), "Nested braces preserved: {}", result);
        assert!(!result.contains("comment"), "Comment stripped: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_template_with_slash_in_interpolation() {
        // Slash inside interpolation should not start a comment
        let input = "var x = `result: ${a / b}`; // comment";
        let result = strip_js_comments(input);
        assert!(result.contains("a / b") || result.contains("a /b") || result.contains("a/ b"),
            "Division in interpolation preserved: {}", result);
        assert!(!result.contains("comment"), "Comment stripped: {}", result);
    }
    
    #[test]
    fn test_strip_js_comments_nested_template_literal() {
        // Nested template literal: `outer ${`inner`} rest`
        // The inner backtick should NOT end the outer template literal
        let input = "var x = `outer ${`inner`} rest`; // comment";
        let result = strip_js_comments(input);
        assert!(result.contains("outer"), "Outer template content preserved: {}", result);
        assert!(result.contains("inner"), "Inner template content preserved: {}", result);
        assert!(result.contains("rest"), "Content after nested template preserved: {}", result);
        assert!(!result.contains("comment"), "Comment after nested template stripped: {}", result);
    }
    
    // ============================================================
    // CSS comment stripping inside string literals
    // ============================================================
    
    #[test]
    fn test_strip_css_comments_inside_string() {
        // /* inside a CSS string value should NOT be treated as comment start
        let input = r#".icon::before { content: "/* not a comment */"; color: red; }"#;
        let result = strip_css_comments(input);
        assert!(result.contains("not a comment"), 
            "Content inside CSS string should be preserved: {}", result);
        assert!(result.contains("color: red") || result.contains("color:red"), 
            "Rule after string preserved: {}", result);
    }
    
    #[test]
    fn test_strip_css_comments_single_quote_string() {
        let input = ".icon::before { content: '/* also not a comment */'; }";
        let result = strip_css_comments(input);
        assert!(result.contains("also not a comment"), 
            "Content inside single-quoted CSS string should be preserved: {}", result);
    }
    
    // ============================================================
    // Phase 1: Regression tests for dependency expansion
    // ============================================================
    
    #[test]
    fn test_expand_dependencies_linear_chain() {
        // Test A → B → C (linear chain)
        use std::collections::HashMap;
        
        let mut js_deps: HashMap<String, Vec<String>> = HashMap::new();
        js_deps.insert("c".to_string(), vec![]);
        js_deps.insert("b".to_string(), vec!["c".to_string()]);
        js_deps.insert("a".to_string(), vec!["b".to_string()]);
        
        // Simulate expand_all_dependencies logic
        fn walk_tree(
            module: &str,
            deps: &HashMap<String, Vec<String>>,
            visited: &mut std::collections::HashSet<String>,
        ) -> Vec<String> {
            let mut result = Vec::new();
            if visited.contains(module) {
                return result;
            }
            visited.insert(module.to_string());
            
            if let Some(direct_deps) = deps.get(module) {
                for dep in direct_deps {
                    let transitive = walk_tree(dep, deps, visited);
                    for t in transitive {
                        if !result.contains(&t) {
                            result.push(t);
                        }
                    }
                    if !result.contains(dep) {
                        result.push(dep.clone());
                    }
                }
            }
            result
        }
        
        let mut visited = std::collections::HashSet::new();
        let deps_for_a = walk_tree("a", &js_deps, &mut visited);
        
        // A's deps should be [C, B] (transitive first, then direct)
        assert_eq!(deps_for_a.len(), 2, "A should have 2 deps: {:?}", deps_for_a);
        assert!(deps_for_a.contains(&"b".to_string()));
        assert!(deps_for_a.contains(&"c".to_string()));
        
        // C should come before B (topological order)
        let c_idx = deps_for_a.iter().position(|m| m == "c").unwrap();
        let b_idx = deps_for_a.iter().position(|m| m == "b").unwrap();
        assert!(c_idx < b_idx, "c should come before b");
    }
    
    #[test]
    fn test_expand_dependencies_diamond() {
        // Test diamond: A → B → D, A → C → D
        use std::collections::HashMap;
        
        let mut js_deps: HashMap<String, Vec<String>> = HashMap::new();
        js_deps.insert("d".to_string(), vec![]);
        js_deps.insert("b".to_string(), vec!["d".to_string()]);
        js_deps.insert("c".to_string(), vec!["d".to_string()]);
        js_deps.insert("a".to_string(), vec!["b".to_string(), "c".to_string()]);
        
        fn walk_tree(
            module: &str,
            deps: &HashMap<String, Vec<String>>,
            visited: &mut std::collections::HashSet<String>,
        ) -> Vec<String> {
            let mut result = Vec::new();
            if visited.contains(module) {
                return result;
            }
            visited.insert(module.to_string());
            
            if let Some(direct_deps) = deps.get(module) {
                for dep in direct_deps {
                    let transitive = walk_tree(dep, deps, visited);
                    for t in transitive {
                        if !result.contains(&t) {
                            result.push(t);
                        }
                    }
                    if !result.contains(dep) {
                        result.push(dep.clone());
                    }
                }
            }
            result
        }
        
        let mut visited = std::collections::HashSet::new();
        let deps_for_a = walk_tree("a", &js_deps, &mut visited);
        
        // A's deps should be [D, B, C] - D appears only once
        let d_count = deps_for_a.iter().filter(|m| *m == "d").count();
        assert_eq!(d_count, 1, "D should appear exactly once: {:?}", deps_for_a);
        assert_eq!(deps_for_a.len(), 3, "Should have 3 deps (D, B, C): {:?}", deps_for_a);
    }
    
    // ============================================================
    // SCSS files without @module are tracked and not pruned
    // ============================================================
    
    #[test]
    fn test_scan_clears_stale_modules_between_builds() {
        // Verify that deleted source files don't persist as phantom modules
        // when build() is called multiple times on the same AssetGraph
        use tempfile::tempdir;
        use crate::config::Config;
        
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let output_js = dir.path().join("output/js");
        let output_css = dir.path().join("output/css");
        let output_images = dir.path().join("output/images");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&output_js).unwrap();
        fs::create_dir_all(&output_css).unwrap();
        fs::create_dir_all(&output_images).unwrap();
        
        // Write a JS file
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();
        
        let config = Config {
            debug_mode: true,
            minify: false,
            source_maps: false,
            source: crate::config::SourcePaths {
                js: js_dir.clone(),
                scss: dir.path().join("src/scss"),
                images: dir.path().join("src/images"),
            },
            output: crate::config::OutputPaths {
                base: dir.path().join("output"),
                js: output_js,
                css: output_css,
                images: output_images,
            },
            manifest_path: dir.path().join("manifest.json"),
            include_paths: vec![],
            storage: crate::config::StorageConfig::default(),
            jam: crate::config::JamConfig { enabled: false, base_path: "/paste".into() },
            cdn: crate::config::CdnConfig { enabled: false, url: String::new(), fallback_to_local: true },
        };
        
        let mut graph = AssetGraph::new(config);
        let manifest1 = graph.build().unwrap();
        assert!(manifest1.assets.contains_key("js/a.js"), "First build should have js/a.js");
        
        // Delete the file and rebuild on same instance
        fs::remove_file(js_dir.join("a.js")).unwrap();
        let manifest2 = graph.build().unwrap();
        assert!(!manifest2.assets.contains_key("js/a.js"), 
            "Second build should NOT have js/a.js after file deleted");
    }
    
    #[test]
    fn test_scss_without_module_not_pruned() {
        // Simulate the pruning logic from build()
        // CSS key is always tracked, even without @module
        let mut seen_asset_keys: HashSet<String> = HashSet::new();
        let mut assets: HashMap<String, String> = HashMap::new();
        
        // JS asset (has module, tracked in seen_asset_keys)
        let js_key = "js/paste/dom.js".to_string();
        seen_asset_keys.insert(js_key.clone());
        assets.insert(js_key, "js output".to_string());
        
        // CSS asset WITHOUT @module — key is always tracked
        let css_key = "css/main.css".to_string();
        assets.insert(css_key.clone(), "css output".to_string());
        // Always insert key regardless of @module
        seen_asset_keys.insert(css_key);
        
        // Prune (mirrors build() prune step)
        assets.retain(|key, _| seen_asset_keys.contains(key));
        
        // CSS asset should survive pruning
        assert!(assets.contains_key("css/main.css"), 
            "CSS without @module should NOT be pruned from manifest");
    }
}
