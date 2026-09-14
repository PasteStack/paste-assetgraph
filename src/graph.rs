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

/// Identity of the pipeline that turns source bytes into output bytes.
///
/// Output filenames carry a content hash, and a file whose name already exists
/// is never rewritten, so the hash must cover the transformation as well as the
/// source: otherwise a changed printer (or a debug build after a minified one)
/// silently keeps serving the bytes the previous pipeline wrote. Change this
/// whenever the JS or CSS printer, or its options, change.
const TRANSFORM_SIGNATURE: &str = "js:oxc-codegen-0.52;css:lightningcss-1.0.0-alpha.70";

/// Statement separator for concatenated JavaScript. A bare newline lets one
/// file's trailing expression merge with the next file's leading parenthesis
/// into a call; an explicit empty statement terminates it. Matches the
/// separator the Scala combo handler uses for the same reason.
pub const JS_CONCAT_SEPARATOR: &str = "\n;\n";

/// Directory names that hold build output or vendored dependencies rather than
/// sources.
///
/// The project's own output directories come from its config and are filtered by
/// path, but an include path points at another project, whose output directory
/// this build knows nothing about. Walking into it turns compiled assets into
/// modules — a minified copy of a module is registered a second time under a
/// name derived from its output path — so the graph gains entries no source
/// declares.
const BUILD_OUTPUT_DIRS: [&str; 4] = ["target", "dist", "node_modules", ".git"];

/// True for a directory inside the walk that holds build output. The walk root
/// itself is never skipped: a caller may deliberately point an include path at a
/// built distribution.
fn is_build_output(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .map_or(false, |name| BUILD_OUTPUT_DIRS.contains(&name))
}

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
                // Add to modules map: declared module name -> manifest asset key.
                // Consumers (the JAM server, the Scala runtime, this rollup) look
                // the value up in `assets`, so it must be a key of that map and
                // never a source path.
                let key = format!("js/{}.js", module.replace('.', "/"));
                record_module(&mut manifest, module, &key, "js");

                // Use expanded dependencies
                let deps = expanded_deps.get(module).cloned().unwrap_or_default();
                self.process_js_with_deps(source_path.clone(), module, deps, &mut manifest)?;

                // Add to sorted_deps and track seen keys
                seen_asset_keys.insert(key.clone());
                if let Some(entry) = manifest.assets.get(&key) {
                    js_sorted_deps.push((module.clone(), entry.output.clone(), entry.version));
                }
            }
        }

        // Store JS sorted_deps in manifest
        manifest
            .sorted_deps
            .insert("js".to_string(), js_sorted_deps);

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
            // sort_by_file_name: directory order is filesystem-dependent, and it
            // decides both sorted_deps order and the manifest bytes.
            for entry in WalkDir::new(base_dir)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(|e| !is_build_output(e))
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map_or(false, |ext| ext == "scss"))
            {
                let path = entry.path();
                // Skip partials (files starting with _)
                if path
                    .file_name()
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

                if let Some((module_name, output, version, _asset_key)) =
                    self.process_scss(path, base_dir, &mut manifest)?
                {
                    css_sorted_deps.push((module_name, output, version));
                }
            }
        }

        // Store CSS sorted_deps in manifest
        manifest
            .sorted_deps
            .insert("css".to_string(), css_sorted_deps);

        // Remove assets not seen in this build to prevent stale entries
        manifest
            .assets
            .retain(|key, _| seen_asset_keys.contains(key));

        if self.storage.is_dist() {
            manifest.make_deterministic();
            self.write_static_rollup(&manifest, "js", "assetgraph.js")?;
            self.write_static_rollup(&manifest, "css", "assetgraph.css")?;
            // Committed output carries the reproducible manifest only.
            manifest.save_json(&self.config.manifest_path)?;
            Manifest::remove_cache(&self.config.manifest_path)?;
        } else {
            // Save manifest (both JSON and bincode)
            manifest.save(&self.config.manifest_path)?;
        }

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

    /// Project-relative label for an asset's source file, with forward slashes.
    ///
    /// The label identifies the file rather than the machine that built it, so a
    /// manifest built in one checkout matches a manifest built in another, and a
    /// consumer that reads sources (a debug response, an inlined bootstrap) can
    /// resolve it from the project root.
    ///
    /// A file pulled in from an include path outside the project is labelled
    /// relative to that include path, prefixed with the include directory's own
    /// name, so it stays both stable and recognizable.
    fn source_label(&self, path: &Path) -> String {
        let relative = path
            .strip_prefix(&self.config.base_dir)
            .ok()
            .map(Path::to_path_buf)
            .or_else(|| self.include_path_label(path))
            .or_else(|| path.file_name().map(PathBuf::from))
            .unwrap_or_default();
        relative.to_string_lossy().replace('\\', "/")
    }

    fn include_path_label(&self, path: &Path) -> Option<PathBuf> {
        self.config.include_paths.iter().find_map(|include_path| {
            let relative = path.strip_prefix(include_path).ok()?;
            let root = include_path.file_name().map(PathBuf::from)?;
            Some(root.join(relative))
        })
    }

    /// Content hash for an asset: the stripped source plus the identity of the
    /// pipeline that will print it. See TRANSFORM_SIGNATURE.
    fn content_hash(&self, stripped: &str) -> String {
        let transform = if self.emits_verbatim() {
            "verbatim"
        } else {
            "minified"
        };
        let mut input = String::with_capacity(stripped.len() + TRANSFORM_SIGNATURE.len() + 16);
        input.push_str(TRANSFORM_SIGNATURE);
        input.push('\n');
        input.push_str(transform);
        input.push('\n');
        input.push_str(stripped);
        format!("{:x}", md5::compute(input.as_bytes()))
    }

    /// True when output is written as-is instead of through a printer.
    fn emits_verbatim(&self) -> bool {
        self.config.debug_mode || !self.config.minify
    }

    /// Asset keys of one type in the order a single concatenated file must load
    /// them: every dependency before its dependent, and stable across builds
    /// and platforms.
    fn rollup_asset_keys(&self, manifest: &Manifest, asset_type: &str) -> Vec<String> {
        let prefix = format!("{asset_type}/");
        let mut ordered: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        // Declared modules first, in dependency order. sorted_deps carries the
        // build's own order; any module missing from it is appended by name so
        // the seed itself never depends on map iteration order.
        let mut seed: Vec<String> = manifest
            .sorted_deps
            .get(asset_type)
            .into_iter()
            .flatten()
            .map(|(module, _, _)| module.clone())
            .collect();
        let mut declared: Vec<&String> = manifest.modules.keys().collect();
        declared.sort();
        for module in declared {
            if !seed.contains(module) {
                seed.push(module.clone());
            }
        }

        for module in seed {
            for dependency in manifest.get_dependencies_for_type(&module, asset_type) {
                if let Some(key) = manifest.asset_key_for_module(&dependency, asset_type) {
                    if key.starts_with(&prefix) && seen.insert(key.clone()) {
                        ordered.push(key);
                    }
                }
            }
        }

        // Assets with no declared module name (a stylesheet without @module)
        // follow, in key order.
        let mut remaining: Vec<String> = manifest
            .assets
            .keys()
            .filter(|key| key.starts_with(&prefix) && !seen.contains(*key))
            .cloned()
            .collect();
        remaining.sort();
        ordered.extend(remaining);
        ordered
    }

    fn write_static_rollup(
        &self,
        manifest: &Manifest,
        asset_type: &str,
        filename: &str,
    ) -> Result<()> {
        let parts = self
            .rollup_asset_keys(manifest, asset_type)
            .into_iter()
            .filter_map(|key| manifest.assets.get(&key).map(|entry| entry.output.clone()))
            .map(|output| {
                let relative =
                    output
                        .strip_prefix(&format!("/{asset_type}/"))
                        .ok_or_else(|| {
                            anyhow::anyhow!("invalid {} rollup path: {}", asset_type, output)
                        })?;
                let path = self.storage.resolve_dir(asset_type).join(relative);
                fs::read_to_string(&path).map_err(|e| {
                    anyhow::anyhow!("failed to read {} for the rollup: {}", path.display(), e)
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let separator = if asset_type == "js" {
            JS_CONCAT_SEPARATOR
        } else {
            "\n"
        };
        self.storage
            .write_asset(parts.join(separator).as_bytes(), asset_type, filename)?;
        Ok(())
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
        let generated_dirs = [
            self.config.output.base.clone(),
            self.config.output.js.clone(),
            self.config.output.css.clone(),
            self.config.output.images.clone(),
        ];
        for entry in WalkDir::new(dir)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| !is_build_output(e))
            .filter_map(|e| e.ok())
            .filter(|e| {
                !generated_dirs
                    .iter()
                    .any(|directory| e.path().starts_with(directory))
            })
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
            let deps: Vec<String> = self
                .requires_re
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

        // Sorted roots: HashMap iteration order is randomized per process, and
        // this order reaches sorted_deps, the rollups, and the manifest bytes.
        let mut roots: Vec<&String> = self.js_deps.keys().collect();
        roots.sort();
        for module in roots {
            visit(
                module,
                &self.js_deps,
                &self.module_map,
                &mut visited,
                &mut temp,
                &mut result,
            )?;
        }

        Ok(result)
    }

    fn process_js_with_deps(
        &self,
        source_path: PathBuf,
        module: &str,
        deps: Vec<String>,
        manifest: &mut Manifest,
    ) -> Result<()> {
        let content = fs::read_to_string(&source_path)?;

        // Strip comments before hashing (so comments don't trigger new versions)
        let stripped = strip_js_comments(&content);
        let hash = self.content_hash(&stripped);

        let key = format!("js/{}.js", module.replace('.', "/"));
        let rel_path = module.replace('.', "/");
        let output_base = format!("/js/{}", rel_path);

        let entry = manifest.update_asset_with_history(
            &key,
            &self.source_label(&source_path),
            &hash,
            &output_base,
            "js",
            deps, // Use expanded transitive dependencies
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
                // Use oxc parser + codegen for minification
                match minify_js(&content) {
                    Ok(minified) => minified,
                    Err(e) => {
                        eprintln!("Warning: JS minification failed for {}: {}", module, e);
                        content
                    }
                }
            };

            // Write output via storage backend
            self.storage
                .write_asset(output_content.as_bytes(), "js", output_name)?;
        }

        Ok(())
    }

    /// Process SCSS file and return (module_name, output_path, version, asset_key) for sorted_deps
    fn process_scss(
        &self,
        source_path: &Path,
        base_dir: &Path,
        manifest: &mut Manifest,
    ) -> Result<Option<(String, String, u32, String)>> {
        let content = fs::read_to_string(source_path)?;

        // Extract @module comment if present (same format as JS)
        // Done before passing content to grass::from_string which takes ownership
        let module_name = self
            .module_re
            .captures(&content)
            .map(|caps| caps.get(1).unwrap().as_str().replace('/', "."));

        // Extract @requires dependencies (same format as JS)
        let deps: Vec<String> = self
            .requires_re
            .captures_iter(&content)
            .map(|c| c.get(1).unwrap().as_str().replace('/', "."))
            .collect();

        // Compile SCSS using grass with include paths
        // Use from_path so grass knows the file's location for relative @import resolution
        let mut options = grass::Options::default().load_path(&self.config.source.scss);

        // Add all include paths
        for include_path in &self.config.include_paths {
            options = options.load_path(include_path);
        }

        let css_content = grass::from_path(source_path, &options)?;

        // Strip comments before hashing (so comments don't trigger new versions)
        let stripped = strip_css_comments(&css_content);
        let hash = self.content_hash(&stripped);

        let rel_path = source_path.strip_prefix(base_dir)?;
        let base_name = rel_path.with_extension("").to_string_lossy().to_string();
        let key = format!("css/{}.css", base_name);
        let output_base = format!("/css/{}", base_name);

        // Add to modules map if @module comment was found
        if let Some(ref mod_name) = module_name {
            record_module(manifest, mod_name, &key, "css");
        }

        let entry = manifest.update_asset_with_history(
            &key,
            &self.source_label(source_path),
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
            self.storage
                .write_asset(output_content.as_bytes(), "css", output_name)?;
        }

        // Return module info for sorted_deps if @module was found, include asset key for tracking
        Ok(module_name.map(|name| (name, output, version, key.clone())))
    }
}

/// Record a declared module name against its manifest asset key.
///
/// The map holds one entry per name, and the same name can be declared by a
/// script and by a stylesheet. Keep the entry a consumer cannot reconstruct: a
/// key that follows the convention is derivable from the name alone, while a
/// stylesheet declaring a name unrelated to its path is not, and losing that
/// entry would make the asset unreachable by name. Type-aware resolution
/// (Manifest::asset_key_for_module) falls back to the convention key, so the
/// entry that is dropped here is still resolvable.
fn record_module(manifest: &mut Manifest, module: &str, key: &str, asset_type: &str) {
    let conventional = format!("{}/{}.{}", asset_type, module.replace('.', "/"), asset_type);
    let derivable = key == conventional;
    match manifest.modules.get(module) {
        Some(existing) if existing == key => {}
        Some(_) if derivable => {}
        _ => {
            manifest.modules.insert(module.to_string(), key.to_string());
        }
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
        eprintln!(
            "Warning: JS source directory does not exist: {:?}",
            config.source.js
        );
    }

    if config.source.scss.exists() {
        watcher.watch(&config.source.scss, RecursiveMode::Recursive)?;
    } else {
        eprintln!(
            "Warning: SCSS source directory does not exist: {:?}",
            config.source.scss
        );
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
                    p.extension()
                        .map_or(false, |e| e == "js" || e == "scss" || e == "css")
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

/// Minify JavaScript using oxc's parser + codegen (whitespace-only).
///
/// We deliberately use the codegen printer rather than a full compressor:
/// codegen faithfully prints the parsed AST, so `function () {}` is emitted
/// as a compact `function(){}` and is NEVER rewritten into an arrow function.
/// This avoids the minify-js 0.6 bug where constructor functions became arrows
/// and `new` threw "e is not a constructor", breaking paste.js's OOP system.
///
/// The printer must never raise the syntax level of the input either: in
/// minify mode oxc's codegen prints plain string literals as template
/// literals (`` `x` `` is one byte shorter to escape than `"x"`), which is
/// ES2015 syntax and a hard parse error on the ES5 engines paste.js ships
/// polyfills for. `restore_string_literals` rewrites every substitution-free,
/// untagged template literal in the printed output back to a quoted string.
///
/// On any parse error (or panic) we return Err so the caller falls back to the
/// original, unminified source instead of emitting broken output.
fn minify_js(content: &str) -> Result<String> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<String> {
        let printed = print_js(content)?;
        restore_string_literals(&printed)
    }));

    match result {
        Ok(inner) => inner,
        Err(_) => Err(anyhow::anyhow!("JS minifier panicked (unsupported syntax)")),
    }
}

/// Parse `content` as a classic script and print it through oxc's codegen in minify mode.
fn print_js(content: &str) -> Result<String> {
    use oxc::allocator::Allocator;
    use oxc::codegen::{Codegen, CodegenOptions};
    use oxc::parser::Parser;
    use oxc::span::SourceType;

    let allocator = Allocator::default();
    // paste.js modules are classic scripts (global `paste.define(...)`),
    // not ES modules, so parse in script mode.
    let parsed = Parser::new(&allocator, content, SourceType::cjs()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(anyhow::anyhow!(
            "JS parse produced {} diagnostic(s); refusing to emit minified output",
            parsed.errors.len()
        ));
    }
    let printed = Codegen::new()
        .with_options(CodegenOptions {
            minify: true,
            ..CodegenOptions::default()
        })
        .build(&parsed.program);
    Ok(printed.code)
}

/// Rewrite template literals that carry no substitutions and no tag back to
/// double-quoted string literals. Such a literal is exactly equivalent to a
/// string whose value is its cooked text, so the rewrite is semantics-preserving;
/// tagged templates are left alone because their tag observes the raw text.
///
/// One position needs care: a template literal that is a whole expression
/// statement is not a directive, but the quoted string replacing it would be —
/// `` `use strict` `` is a no-op statement while `"use strict"` at the head of a
/// function body switches that function to strict mode. Such a replacement is
/// parenthesized so it stays an ordinary expression statement.
fn restore_string_literals(code: &str) -> Result<String> {
    use oxc::allocator::Allocator;
    use oxc::ast::ast::{ExpressionStatement, TaggedTemplateExpression, TemplateLiteral};
    use oxc::ast::visit::walk;
    use oxc::ast::Visit;
    use oxc::parser::Parser;
    use oxc::span::{GetSpan, SourceType};

    struct Collector {
        replacements: Vec<(u32, u32, String)>,
        statement_expressions: HashSet<u32>,
    }

    impl<'a> Visit<'a> for Collector {
        fn visit_expression_statement(&mut self, it: &ExpressionStatement<'a>) {
            self.statement_expressions
                .insert(it.expression.span().start);
            walk::walk_expression_statement(self, it);
        }

        fn visit_tagged_template_expression(&mut self, it: &TaggedTemplateExpression<'a>) {
            // Visit the tag (it may contain ordinary strings) but not the quasi.
            self.visit_expression(&it.tag);
        }

        fn visit_template_literal(&mut self, it: &TemplateLiteral<'a>) {
            if it.expressions.is_empty() && it.quasis.len() == 1 {
                if let Some(cooked) = &it.quasis[0].value.cooked {
                    let literal = quote_js_string(cooked);
                    let replacement = if self.statement_expressions.contains(&it.span.start) {
                        format!("({literal})")
                    } else {
                        literal
                    };
                    self.replacements
                        .push((it.span.start, it.span.end, replacement));
                    return;
                }
            }
            walk::walk_template_literal(self, it);
        }
    }

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, SourceType::cjs()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(anyhow::anyhow!(
            "minified output failed to re-parse; refusing to emit it"
        ));
    }
    let mut collector = Collector {
        replacements: Vec::new(),
        statement_expressions: HashSet::new(),
    };
    collector.visit_program(&parsed.program);

    let mut out = code.to_string();
    collector
        .replacements
        .sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, literal) in collector.replacements {
        out.replace_range(start as usize..end as usize, &literal);
    }
    Ok(out)
}

/// Print `value` as an ES5 double-quoted string literal.
fn quote_js_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Line terminators that ES5 forbids inside a string literal.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Minify CSS using lightningcss
fn minify_css(content: &str) -> Result<String> {
    use lightningcss::stylesheet::{ParserOptions, PrinterOptions, StyleSheet};

    let stylesheet = StyleSheet::parse(content, ParserOptions::default())
        .map_err(|e| anyhow::anyhow!("CSS parse error: {:?}", e))?;

    let result = stylesheet
        .to_css(PrinterOptions {
            minify: true,
            ..Default::default()
        })
        .map_err(|e| anyhow::anyhow!("CSS print error: {:?}", e))?;

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
                    if !c.is_whitespace() {
                        last_nonws = Some(c);
                    }
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
                    if nc == '\n' {
                        break;
                    }
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
    "return",
    "typeof",
    "throw",
    "new",
    "case",
    "in",
    "void",
    "delete",
    "instanceof",
    "yield",
    "await",
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
    // JS minification (oxc) — regression for the minify-js arrow bug
    // ============================================================

    #[test]
    fn test_minify_js_preserves_constructor_functions() {
        // paste.js OOP uses `function`s invoked with `new`. The old minify-js
        // crate rewrote these into arrow functions, which cannot be `new`'d
        // ("e is not a constructor"). oxc codegen must keep them as functions.
        let input = r#"
            var oop = {};
            oop.Class = {
                create: function (proto) {
                    var Ctor = function () {
                        if (this.init) { this.init.apply(this, arguments); }
                    };
                    Ctor.prototype = proto;
                    return Ctor;
                }
            };
            var Animal = oop.Class.create({ init: function (name) { this.name = name; } });
            var a = new Animal("rex");
        "#;

        let out = minify_js(input).expect("minification should succeed");

        assert!(
            out.contains("function"),
            "functions must be preserved: {}",
            out
        );
        assert!(
            !out.contains("=>"),
            "constructor functions must NOT become arrow functions: {}",
            out
        );
        assert!(
            out.contains("new Animal"),
            "`new` call must be preserved: {}",
            out
        );
        assert!(
            out.len() < input.len(),
            "minified output should be smaller than input"
        );
    }

    #[test]
    fn test_minify_js_output_is_valid_js() {
        // Re-minifying the output must also succeed, proving the emitted code
        // is syntactically valid JavaScript.
        let input = "var f = function (x) { return x * 2; }; var y = f(21);";
        let out = minify_js(input).expect("first minification should succeed");
        let out2 = minify_js(&out).expect("re-minifying valid output should succeed");
        assert!(out2.contains("function"));
        assert!(!out2.contains("=>"));
    }

    #[test]
    fn test_minify_js_removes_whitespace_and_comments() {
        let input = "var x = 1; // a line comment\n\n   var y = 2;   /* block */  var z = x + y;";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            !out.contains("// a line comment"),
            "line comment should be removed: {}",
            out
        );
        assert!(
            !out.contains("/* block */"),
            "block comment should be removed: {}",
            out
        );
        assert!(
            !out.contains("  "),
            "runs of spaces should be collapsed: {}",
            out
        );
    }

    #[test]
    fn test_minify_js_never_raises_syntax_level_of_es5_input() {
        // oxc's minify-mode printer prefers backticks for plain strings; on the
        // ES5 engines paste.js supports a template literal is a parse error for
        // the whole file. ES5 in must mean ES5 out.
        let input = r#"paste.define('paste.oop', ['paste.util'], function (oop, util) {
            "use strict";
            var msg = 'He said "hi"', path = "a\\b", multi = 'line1\nline2';
            return { msg: msg, path: path, multi: multi, sep: "\u2028" };
        });"#;
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            !out.contains('`'),
            "no template literal may be introduced: {}",
            out
        );
        assert!(
            !out.contains("=>"),
            "no arrow function may be introduced: {}",
            out
        );
        assert!(
            out.contains(r#""paste.oop""#) || out.contains("'paste.oop'"),
            "module name stays a string: {}",
            out
        );
        assert!(
            !out.contains('\u{2028}'),
            "U+2028 must be escaped, not emitted raw: {}",
            out
        );
        // The restored strings must still be exactly the same values.
        let reparsed = minify_js(&out).expect("output must re-parse");
        assert_eq!(
            reparsed, out,
            "restoring string literals must be idempotent"
        );
    }

    #[test]
    fn test_minify_js_keeps_genuine_template_literals() {
        // Sources that really use ES2015 keep it: substitutions and tags are
        // semantics the rewrite cannot express as a plain string.
        let input = "var a = `x${y}z`; var b = tag`raw\\n`; var c = `plain`;";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            out.contains("`x${y}z`"),
            "substitution template must be kept: {}",
            out
        );
        assert!(
            out.contains("tag`raw\\n`"),
            "tagged template must be kept verbatim: {}",
            out
        );
        assert!(
            out.contains(r#""plain""#),
            "substitution-free untagged template becomes a string: {}",
            out
        );
    }

    #[test]
    fn test_minify_js_does_not_introduce_a_strict_mode_directive() {
        // `use strict` as a template literal is an ordinary no-op statement.
        // Printing it as a bare string would make it a directive and switch the
        // enclosing function to strict mode — a semantic change, not a rewrite.
        let input = "function f() { `use strict`; return this === undefined; }";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            !out.contains('`'),
            "template literal should be rewritten: {out}"
        );
        assert!(
            !out.contains("{\"use strict\"") && !out.contains("{ \"use strict\""),
            "the rewrite must not become a directive: {out}"
        );
        assert!(
            out.contains("(\"use strict\")"),
            "the statement should stay an expression: {out}"
        );
        let reparsed = minify_js(&out).expect("output must re-parse");
        assert_eq!(reparsed, out, "the rewrite must be idempotent");
    }

    #[test]
    fn test_minify_js_keeps_an_existing_strict_mode_directive() {
        // A real directive must survive untouched.
        let input = "function f() { \"use strict\"; return 1; }";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            out.contains("\"use strict\""),
            "existing directive must be preserved: {out}"
        );
        assert!(
            !out.contains("(\"use strict\")"),
            "an existing directive must not be parenthesized: {out}"
        );
    }

    #[test]
    fn test_quote_js_string_escapes_for_es5() {
        assert_eq!(quote_js_string("plain"), r#""plain""#);
        assert_eq!(quote_js_string(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_js_string("a\\b"), r#""a\\b""#);
        assert_eq!(quote_js_string("l1\nl2\r\t"), r#""l1\nl2\r\t""#);
        assert_eq!(
            quote_js_string("\u{2028}\u{2029}\u{1}"),
            r#""\u2028\u2029\u0001""#
        );
    }

    // ============================================================
    // Escape sequence handling in strip_js_comments
    // ============================================================

    #[test]
    fn test_strip_js_comments_simple_string() {
        // Simple strings should be preserved
        let input = r#"var x = "hello";"#;
        let result = strip_js_comments(input);
        assert!(
            result.contains(r#""hello""#),
            "Simple string should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_escaped_quote() {
        // Escaped quotes inside strings should NOT end the string
        let input = r#"var x = "hello \"world\""; // comment"#;
        let result = strip_js_comments(input);
        // The entire string including escaped quotes should be preserved
        // Comment after the string should be stripped
        assert!(
            result.contains("hello"),
            "String content should be preserved: {}",
            result
        );
        assert!(
            result.contains("world"),
            "Escaped quote content should be preserved: {}",
            result
        );
        // Critical: the comment should be stripped, not become part of output
        // If escaped quote handling is broken, "world" might be seen as code, and
        // the rest would be corrupted
        assert!(
            !result.contains("comment"),
            "Comment should be stripped: {}",
            result
        );
        // The closing quote and semicolon should be present
        assert!(
            result.ends_with(";") || result.ends_with("\";"),
            "Statement should end properly: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_escaped_backslash_before_quote() {
        // Escaped backslash followed by quote: \\" means backslash then end of string
        // The string "path\\" contains a backslash, then the string ENDS
        let input = r#"var x = "path\\"; // comment after"#;
        let result = strip_js_comments(input);
        // The comment should be stripped (string ends at the second ")
        assert!(
            !result.contains("comment"),
            "Comment should be stripped - string ends after \\\\: {}",
            result
        );
        // The var declaration should be complete
        assert!(result.contains("var x"), "var x should exist: {}", result);
    }

    #[test]
    fn test_strip_js_comments_line_comment() {
        let input = "var x = 1; // this is a comment\nvar y = 2;";
        let result = strip_js_comments(input);
        assert!(result.contains("var x"), "Code before comment should exist");
        assert!(result.contains("var y"), "Code after comment should exist");
        assert!(
            !result.contains("this is a comment"),
            "Comment should be stripped"
        );
    }

    #[test]
    fn test_strip_js_comments_block_comment() {
        let input = "var x = 1; /* block comment */ var y = 2;";
        let result = strip_js_comments(input);
        assert!(result.contains("var x"), "Code before comment should exist");
        assert!(result.contains("var y"), "Code after comment should exist");
        assert!(
            !result.contains("block comment"),
            "Comment should be stripped"
        );
    }

    #[test]
    fn test_strip_js_comments_slash_in_string() {
        // Slash inside a string should NOT be treated as comment start
        let input = r#"var url = "http://example.com";"#;
        let result = strip_js_comments(input);
        assert!(
            result.contains("http://example.com"),
            "URL in string should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_regex_like_pattern() {
        // Division operator should not start a comment
        let input = "var x = 10 / 2; // comment";
        let result = strip_js_comments(input);
        assert!(
            result.contains("10 / 2") || result.contains("10/ 2") || result.contains("10 /2"),
            "Division should be preserved: {}",
            result
        );
    }

    // ============================================================
    // Regex literal handling
    // ============================================================

    #[test]
    fn test_strip_js_comments_regex_literal() {
        // Regex literal /foo.*bar/ should not be treated as a block comment
        let input = r#"var x = /foo.*bar/; var y = 2;"#;
        let result = strip_js_comments(input);
        assert!(
            result.contains("/foo.*bar/"),
            "Regex literal should be preserved: {}",
            result
        );
        assert!(
            result.contains("var y"),
            "Code after regex should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_regex_after_equals() {
        let input = "var re = /test/g; // comment";
        let result = strip_js_comments(input);
        assert!(
            result.contains("/test/g"),
            "Regex with flags preserved: {}",
            result
        );
        assert!(
            !result.contains("comment"),
            "Comment after regex stripped: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_division_not_regex() {
        let input = "var x = 10 / 2; /* comment */ var y = 3;";
        let result = strip_js_comments(input);
        assert!(
            result.contains("10") && result.contains("2"),
            "Division preserved: {}",
            result
        );
        assert!(!result.contains("comment"), "Comment stripped: {}", result);
        assert!(
            result.contains("var y"),
            "Code after comment preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_regex_in_condition() {
        let input = "if (/pattern/.test(x)) { y = 1; }";
        let result = strip_js_comments(input);
        assert!(
            result.contains("/pattern/"),
            "Regex in condition preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_block_comment_after_value() {
        // Block comment /* */ after a value expression must still be stripped.
        // In JS, /* always starts a block comment regardless of preceding token.
        let input = "var x = 10 /* block */ + 5;";
        let result = strip_js_comments(input);
        assert!(
            !result.contains("block"),
            "Block comment after value should be stripped: {}",
            result
        );
        assert!(
            result.contains("10"),
            "Value before comment preserved: {}",
            result
        );
        assert!(
            result.contains("5"),
            "Value after comment preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_regex_after_plus_operator() {
        // In JS, `/` after `+` or `-` is a regex literal, not division.
        // Example: var x = +/pattern/g;
        let input = "var x = +/pattern/g;";
        let result = strip_js_comments(input);
        assert!(
            result.contains("/pattern/g"),
            "Regex after + operator should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_regex_after_minus_operator() {
        let input = "var x = -/pattern/.test(y);";
        let result = strip_js_comments(input);
        assert!(
            result.contains("/pattern/"),
            "Regex after - operator should be preserved: {}",
            result
        );
    }

    // ============================================================
    // Regex literal with space after /
    // ============================================================

    #[test]
    fn test_strip_js_comments_regex_with_space_after_slash() {
        // Regex literals CAN have a space after the opening /
        // e.g., / pattern / is valid JS (though unusual)
        let input = "if (/ pattern /.test(x)) { y = 1; }";
        let result = strip_js_comments(input);
        assert!(
            result.contains("/ pattern /"),
            "Regex with space after opening / should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_regex_with_leading_space_and_flags() {
        let input = "var re = / foo /gi;";
        let result = strip_js_comments(input);
        assert!(
            result.contains("/ foo /gi"),
            "Regex with space and flags should be preserved: {}",
            result
        );
    }

    // ============================================================
    // Regex after JS keywords
    // ============================================================

    #[test]
    fn test_strip_js_comments_regex_after_return() {
        // `return /regex/` — `/` after `return` is a regex, not division
        let input = "function f() { return /pattern/g; }";
        let result = strip_js_comments(input);
        assert!(
            result.contains("/pattern/g"),
            "Regex after return should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_regex_after_typeof() {
        let input = "if (typeof /regex/.test === 'function') {}";
        let result = strip_js_comments(input);
        assert!(
            result.contains("/regex/"),
            "Regex after typeof should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_regex_after_throw() {
        let input = "throw /invalid/;";
        let result = strip_js_comments(input);
        assert!(
            result.contains("/invalid/"),
            "Regex after throw should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_regex_after_new() {
        // `new /regex/` is unusual but syntactically valid in some contexts
        let input = "var x = new /pattern/;";
        let result = strip_js_comments(input);
        assert!(
            result.contains("/pattern/"),
            "Regex after new should be preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_division_after_identifier() {
        // Division after an identifier (not a keyword) should still work
        let input = "var x = total / count; // comment";
        let result = strip_js_comments(input);
        assert!(
            result.contains("total") && result.contains("count"),
            "Division after identifier preserved: {}",
            result
        );
        assert!(!result.contains("comment"), "Comment stripped: {}", result);
    }

    // ============================================================
    // CSS comment stripping
    // ============================================================

    #[test]
    fn test_strip_css_comments_basic() {
        let input = ".class { color: red; /* comment */ }";
        let result = strip_css_comments(input);
        assert!(
            result.contains("color: red") || result.contains("color:red"),
            "CSS should be preserved: {}",
            result
        );
        assert!(!result.contains("comment"), "Comment should be stripped");
    }

    #[test]
    fn test_strip_css_comments_url() {
        // URLs with slashes should be preserved (not in /* */ so should be fine)
        let input = r#".class { background: url("http://example.com/img.png"); }"#;
        let result = strip_css_comments(input);
        assert!(
            result.contains("http://example.com"),
            "URL should be preserved: {}",
            result
        );
    }

    // ============================================================
    // Template literal interpolation handling
    // ============================================================

    #[test]
    fn test_strip_js_comments_template_literal_with_interpolation() {
        // Template literals with ${...} containing quotes should not break parsing
        let input = r#"var x = `hello ${name + "!"} world`; // comment"#;
        let result = strip_js_comments(input);
        assert!(
            result.contains("hello"),
            "Template content before interpolation preserved: {}",
            result
        );
        assert!(
            result.contains("world"),
            "Template content after interpolation preserved: {}",
            result
        );
        assert!(
            !result.contains("comment"),
            "Trailing comment stripped: {}",
            result
        );
    }

    #[test]
    fn test_strip_js_comments_nested_template_interpolation() {
        // Nested braces inside interpolation
        let input = "var x = `${({a: 1}).a}`; // comment";
        let result = strip_js_comments(input);
        assert!(
            result.contains("({a: 1})"),
            "Nested braces preserved: {}",
            result
        );
        assert!(!result.contains("comment"), "Comment stripped: {}", result);
    }

    #[test]
    fn test_strip_js_comments_template_with_slash_in_interpolation() {
        // Slash inside interpolation should not start a comment
        let input = "var x = `result: ${a / b}`; // comment";
        let result = strip_js_comments(input);
        assert!(
            result.contains("a / b") || result.contains("a /b") || result.contains("a/ b"),
            "Division in interpolation preserved: {}",
            result
        );
        assert!(!result.contains("comment"), "Comment stripped: {}", result);
    }

    #[test]
    fn test_strip_js_comments_nested_template_literal() {
        // Nested template literal: `outer ${`inner`} rest`
        // The inner backtick should NOT end the outer template literal
        let input = "var x = `outer ${`inner`} rest`; // comment";
        let result = strip_js_comments(input);
        assert!(
            result.contains("outer"),
            "Outer template content preserved: {}",
            result
        );
        assert!(
            result.contains("inner"),
            "Inner template content preserved: {}",
            result
        );
        assert!(
            result.contains("rest"),
            "Content after nested template preserved: {}",
            result
        );
        assert!(
            !result.contains("comment"),
            "Comment after nested template stripped: {}",
            result
        );
    }

    // ============================================================
    // CSS comment stripping inside string literals
    // ============================================================

    #[test]
    fn test_strip_css_comments_inside_string() {
        // /* inside a CSS string value should NOT be treated as comment start
        let input = r#".icon::before { content: "/* not a comment */"; color: red; }"#;
        let result = strip_css_comments(input);
        assert!(
            result.contains("not a comment"),
            "Content inside CSS string should be preserved: {}",
            result
        );
        assert!(
            result.contains("color: red") || result.contains("color:red"),
            "Rule after string preserved: {}",
            result
        );
    }

    #[test]
    fn test_strip_css_comments_single_quote_string() {
        let input = ".icon::before { content: '/* also not a comment */'; }";
        let result = strip_css_comments(input);
        assert!(
            result.contains("also not a comment"),
            "Content inside single-quoted CSS string should be preserved: {}",
            result
        );
    }

    // ============================================================
    // Dependency expansion
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
        assert_eq!(
            deps_for_a.len(),
            2,
            "A should have 2 deps: {:?}",
            deps_for_a
        );
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
        assert_eq!(
            deps_for_a.len(),
            3,
            "Should have 3 deps (D, B, C): {:?}",
            deps_for_a
        );
    }

    // ============================================================
    // SCSS files without @module are tracked and not pruned
    // ============================================================

    #[test]
    fn test_scan_clears_stale_modules_between_builds() {
        // Verify that deleted source files don't persist as phantom modules
        // when build() is called multiple times on the same AssetGraph
        use crate::config::Config;
        use tempfile::tempdir;

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
            base_dir: dir.path().to_path_buf(),
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
            jam: crate::config::JamConfig {
                enabled: false,
                base_path: "/paste".into(),
            },
            cdn: crate::config::CdnConfig {
                enabled: false,
                url: String::new(),
                fallback_to_local: true,
            },
        };

        let mut graph = AssetGraph::new(config);
        let manifest1 = graph.build().unwrap();
        assert!(
            manifest1.assets.contains_key("js/a.js"),
            "First build should have js/a.js"
        );

        // Delete the file and rebuild on same instance
        fs::remove_file(js_dir.join("a.js")).unwrap();
        let manifest2 = graph.build().unwrap();
        assert!(
            !manifest2.assets.contains_key("js/a.js"),
            "Second build should NOT have js/a.js after file deleted"
        );
    }

    /// Config for a dist-backend build rooted at `dir`, with sources in
    /// src/js + src/scss and output in dist/.
    fn dist_config(dir: &Path, minify: bool) -> Config {
        let output = dir.join("dist");
        Config {
            base_dir: dir.to_path_buf(),
            debug_mode: false,
            minify,
            source_maps: false,
            source: crate::config::SourcePaths {
                js: dir.join("src/js"),
                scss: dir.join("src/scss"),
                images: dir.join("src/images"),
            },
            output: crate::config::OutputPaths {
                base: output.clone(),
                js: output.join("js"),
                css: output.join("css"),
                images: output.join("images"),
            },
            manifest_path: output.join("manifest.json"),
            include_paths: vec![],
            storage: crate::config::StorageConfig {
                backend: "dist".into(),
                keep_versions: 3,
            },
            jam: crate::config::JamConfig {
                enabled: false,
                base_path: "/paste".into(),
            },
            cdn: crate::config::CdnConfig {
                enabled: false,
                url: String::new(),
                fallback_to_local: true,
            },
        }
    }

    #[test]
    fn test_dist_build_emits_static_rollups() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let scss_dir = dir.path().join("src/scss");
        let output = dir.path().join("dist");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();

        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();
        fs::write(
            scss_dir.join("main.scss"),
            "// @module main\n.main { color: red; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        graph.build().unwrap();

        let js_rollup = fs::read_to_string(output.join("js/assetgraph.js")).unwrap();
        let css_rollup = fs::read_to_string(output.join("css/assetgraph.css")).unwrap();
        assert!(
            js_rollup.contains("var a=1"),
            "JS rollup should carry the module output: {js_rollup}"
        );
        assert!(
            css_rollup.contains(".main"),
            "CSS rollup should carry the compiled stylesheet: {css_rollup}"
        );
    }

    #[test]
    fn test_dist_rollup_orders_dependencies_before_dependents() {
        use tempfile::tempdir;

        // Alphabetical key order would place "a" (which requires "z") first, and
        // the concatenated file would then use z before it exists.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("a.js"),
            "// @module a\n// @requires z\nvar a = z + 1;",
        )
        .unwrap();
        fs::write(js_dir.join("z.js"), "// @module z\nvar z = 1;").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        graph.build().unwrap();

        let rollup = fs::read_to_string(dir.path().join("dist/js/assetgraph.js")).unwrap();
        let z_at = rollup.find("var z=1").expect("z module in rollup");
        let a_at = rollup.find("var a=z+1").expect("a module in rollup");
        assert!(
            z_at < a_at,
            "dependency must precede its dependent: {rollup}"
        );
        assert!(
            rollup.contains(JS_CONCAT_SEPARATOR),
            "modules must be separated by a terminating statement: {rollup}"
        );
    }

    #[test]
    fn test_dist_rollup_separator_keeps_adjacent_modules_apart() {
        use tempfile::tempdir;

        // Two files that are each valid alone: without a statement terminator
        // between them the second file's leading "(" continues the first file's
        // trailing expression into a call.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("first.js"),
            "// @module first\nvar first = (function () { return 1; })()",
        )
        .unwrap();
        fs::write(
            js_dir.join("second.js"),
            "// @module second\n(function () { var x = 2; })();",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        graph.build().unwrap();

        let rollup = fs::read_to_string(dir.path().join("dist/js/assetgraph.js")).unwrap();
        let first_at = rollup.find("var first").expect("first module in rollup");
        let boundary = rollup[first_at..].find(JS_CONCAT_SEPARATOR);
        assert!(
            boundary.is_some(),
            "unminified modules must still be separated: {rollup}"
        );
    }

    #[test]
    fn test_dist_manifest_bytes_are_stable_across_builds() {
        use tempfile::tempdir;

        // HashMap iteration order is randomized per process, so a manifest that
        // depends on it differs between builds of identical sources. Rebuilding
        // in-process cannot catch that; each build runs as its own process.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();
        for name in ["alpha", "beta", "gamma", "delta"] {
            fs::write(
                js_dir.join(format!("{name}.js")),
                format!("// @module {name}\n// @requires shared\nvar {name} = shared;"),
            )
            .unwrap();
        }
        fs::write(
            js_dir.join("shared.js"),
            "// @module shared\nvar shared = 1;",
        )
        .unwrap();
        fs::write(
            scss_dir.join("main.scss"),
            "// @module main\n.main { color: red; }",
        )
        .unwrap();

        let manifest_path = dir.path().join("dist/manifest.json");
        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        graph.build().unwrap();
        let first = fs::read_to_string(&manifest_path).unwrap();

        // A fresh graph re-scans and re-orders everything from disk.
        let mut rebuilt = AssetGraph::new(dist_config(dir.path(), true));
        rebuilt.build().unwrap();
        let second = fs::read_to_string(&manifest_path).unwrap();

        assert_eq!(
            first, second,
            "a dist manifest must be byte-identical for identical sources"
        );
        assert!(
            !first.contains(dir.path().to_string_lossy().as_ref()),
            "a dist manifest must not record machine-specific paths: {first}"
        );
    }

    #[test]
    fn test_dist_output_carries_no_bincode_cache() {
        use tempfile::tempdir;

        // The cache is written from hash maps, so its bytes vary between runs.
        // Distributed output must not contain it, and a cache left by an earlier
        // build must be removed — load() prefers it over the manifest.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("app.js"), "// @module app\nvar app = 1;").unwrap();
        let cache = dir.path().join("dist/manifest.bin");
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, b"stale cache").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        graph.build().unwrap();

        assert!(dir.path().join("dist/manifest.json").is_file());
        assert!(
            !cache.exists(),
            "a dist build must not leave a bincode cache behind"
        );
    }

    #[test]
    fn test_source_label_is_project_relative() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("app.js"), "// @module app\nvar app = 1;").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest = graph.build().unwrap();

        let entry = manifest.assets.get("js/app.js").expect("app asset");
        assert_eq!(entry.source, "src/js/app.js");
        // The label resolves from the project root, which is what a debug
        // response or an inlined bootstrap reads.
        assert!(
            dir.path().join(&entry.source).is_file(),
            "source label should resolve: {}",
            entry.source
        );
    }

    #[test]
    fn test_source_label_for_an_include_path_names_the_include_directory() {
        use tempfile::tempdir;

        // A module pulled in from outside the project cannot be project-relative;
        // it is labelled relative to the include directory, which is stable
        // across machines, instead of by an absolute path.
        let project = tempdir().unwrap();
        let library = tempdir().unwrap();
        let js_dir = project.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(library.path().join("modules/widget")).unwrap();
        fs::write(
            js_dir.join("app.js"),
            "// @module app\n// @requires paste.ui.widget\nvar app = 1;",
        )
        .unwrap();
        fs::write(
            library.path().join("modules/widget/widget.js"),
            "// @module paste/ui/widget\nvar widget = 1;",
        )
        .unwrap();

        let mut config = dist_config(project.path(), true);
        config.include_paths = vec![library.path().to_path_buf()];
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        let entry = manifest
            .assets
            .get("js/paste/ui/widget.js")
            .expect("include path asset");
        let library_name = library
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            entry.source,
            format!("{library_name}/modules/widget/widget.js")
        );
        assert!(
            !entry.source.starts_with('/'),
            "no absolute path: {}",
            entry.source
        );
    }

    #[test]
    fn test_include_path_build_output_is_not_scanned_as_source() {
        use tempfile::tempdir;

        // An include path points at another project, whose output directory this
        // build knows nothing about. Walking into it registers compiled copies as
        // modules, so the graph gains entries no source declares.
        let project = tempdir().unwrap();
        let library = tempdir().unwrap();
        let js_dir = project.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(library.path().join("modules/widget")).unwrap();
        fs::create_dir_all(library.path().join("target/example/js/paste")).unwrap();
        fs::create_dir_all(library.path().join("node_modules/vendor")).unwrap();
        fs::write(js_dir.join("app.js"), "// @module app\nvar app = 1;").unwrap();
        fs::write(
            library.path().join("modules/widget/widget.js"),
            "// @module paste/ui/widget\nvar widget = 1;",
        )
        .unwrap();
        fs::write(
            library.path().join("modules/widget/widget.scss"),
            "/* @module paste.ui.widget */\n.widget { color: red; }\n",
        )
        .unwrap();
        fs::write(
            library.path().join("target/example/js/paste/widget.min.js"),
            "var widget=1;",
        )
        .unwrap();
        fs::write(
            library
                .path()
                .join("target/example/js/paste/widget.min.scss"),
            ".widget { color: blue; }\n",
        )
        .unwrap();
        fs::write(
            library.path().join("node_modules/vendor/vendor.js"),
            "var vendor = 1;",
        )
        .unwrap();

        let mut config = dist_config(project.path(), true);
        config.include_paths = vec![library.path().to_path_buf()];
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        assert!(
            manifest.assets.contains_key("js/paste/ui/widget.js"),
            "declared source module is scanned"
        );
        let built: Vec<&String> = manifest
            .assets
            .keys()
            .filter(|key| key.contains("target/") || key.contains("node_modules/"))
            .collect();
        assert!(
            built.is_empty(),
            "build output must not enter the graph: {built:?}"
        );
    }

    #[test]
    fn test_declared_modules_map_to_manifest_asset_keys() {
        use tempfile::tempdir;

        // A declared name that does not match the file path must still resolve:
        // consumers look the value up in `assets`.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("site.js"),
            "// @module paste.site\nvar site = 1;",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest = graph.build().unwrap();

        let key = manifest
            .modules
            .get("paste.site")
            .expect("declared module in manifest");
        assert_eq!(key, "js/paste/site.js");
        assert!(
            manifest.assets.contains_key(key),
            "modules map must name an existing asset: {key}"
        );
    }

    #[test]
    fn test_one_name_declared_for_both_types_stays_resolvable() {
        use tempfile::tempdir;

        // The modules map has one entry per name. A script and a stylesheet may
        // declare the same name, and both must remain resolvable by type.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            js_dir.join("widget.js"),
            "// @module widget\nvar widget = 1;",
        )
        .unwrap();
        fs::write(
            scss_dir.join("widget.scss"),
            "// @module widget\n.widget { color: red; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest = graph.build().unwrap();

        assert_eq!(
            manifest.asset_key_for_module("widget", "js").as_deref(),
            Some("js/widget.js")
        );
        assert_eq!(
            manifest.asset_key_for_module("widget", "css").as_deref(),
            Some("css/widget.css")
        );
    }

    #[test]
    fn test_declared_name_unrelated_to_its_path_keeps_its_mapping() {
        use tempfile::tempdir;

        // A stylesheet whose declared name does not match its path is the case
        // the modules map exists for: the convention key cannot find it, so the
        // mapping must survive a same-name script declaration.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            js_dir.join("theme.js"),
            "// @module site/theme\nvar theme = 1;",
        )
        .unwrap();
        fs::write(
            scss_dir.join("theme.scss"),
            "// @module site/theme\n.theme { color: red; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest = graph.build().unwrap();

        // css/theme.css is not derivable from "site.theme", so it is the entry
        // the map must keep; the script resolves through the convention key.
        assert_eq!(
            manifest.modules.get("site.theme").map(String::as_str),
            Some("css/theme.css")
        );
        assert_eq!(
            manifest
                .asset_key_for_module("site.theme", "css")
                .as_deref(),
            Some("css/theme.css")
        );
        assert_eq!(
            manifest.asset_key_for_module("site.theme", "js").as_deref(),
            Some("js/site/theme.js")
        );
    }

    #[test]
    fn test_output_is_rewritten_when_the_transform_changes() {
        use tempfile::tempdir;

        // The same source printed by a different pipeline is different bytes, so
        // it must not reuse the previous output file name.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("app.js"),
            "// @module app\nvar app = function () { return 1; };",
        )
        .unwrap();

        let mut minifying = AssetGraph::new(dist_config(dir.path(), true));
        let minified = minifying.build().unwrap();
        let mut copying = AssetGraph::new(dist_config(dir.path(), false));
        let verbatim = copying.build().unwrap();

        let minified_output = minified.assets.get("js/app.js").unwrap().output.clone();
        let verbatim_output = verbatim.assets.get("js/app.js").unwrap().output.clone();
        assert_ne!(
            minified_output, verbatim_output,
            "different output bytes must not share an output file name"
        );
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
        assert!(
            assets.contains_key("css/main.css"),
            "CSS without @module should NOT be pruned from manifest"
        );
    }
}
