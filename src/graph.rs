use anyhow::Result;
use chrono::{DateTime, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;
use walkdir::WalkDir;

use crate::config::Config;
use crate::jam::{AssetType, OutputDirs};
use crate::manifest::{AssetUpdate, Manifest};
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

/// Whether `name` is a module name as the build writes it: MODULE_NAME_RE's
/// alphabet after `/`→`.` normalization — nonempty dot-separated segments of
/// Unicode word characters and `-`. Empty segments ("a..b", ".a", "a.") are
/// rejected: they would let a declared name reach outside the output tree.
pub fn is_module_name(name: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[\w-]+(?:\.[\w-]+)*$").expect("valid module-name pattern"))
        .is_match(name)
}

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
const BUILD_OUTPUT_DIRS: [&str; 5] = [
    "target",
    "dist",
    "node_modules",
    ".git",
    // The conventional beside-source build directory is never a source, even
    // when `build-dir-name` has since been configured to something else.
    "_paste_build",
];

/// True for a directory inside the walk that holds build output — a vendored
/// or build directory, or a `build-dir-name` directory that beside-source
/// output lands in (skipped in either layout, like the original's
/// `excluded_dirs + [build_prefix]`). The walk root itself is never skipped:
/// a caller may deliberately point an include path at a built distribution.
fn is_build_output(entry: &walkdir::DirEntry, build_dir_name: &str) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| BUILD_OUTPUT_DIRS.contains(&name) || name == build_dir_name)
}

/// Which asset pipeline a discovered source file feeds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Js,
    Scss,
    Css,
}

/// One asset file found by [`AssetGraph::discover_sources`].
struct DiscoveredSource {
    path: PathBuf,
    /// The walk root the file was found under — path-derived names and
    /// style asset keys are relative to it.
    root: PathBuf,
    /// Whether `root` is a configured source dir rather than an include
    /// path — include-path files never write into a source tree.
    from_source_dir: bool,
    kind: SourceKind,
}

/// The ordering of one asset type's module/dependency map, as written to
/// `sorted_deps`.
pub struct LayeredOrder {
    /// Modules released in dependency layers: each layer holds every node
    /// whose remaining dependencies were all released earlier, name-sorted
    /// within the layer. Dependencies come before their dependents.
    pub sorted: Vec<String>,
    /// Cycle members and anything depending on them, name-sorted. They are
    /// left out of `sorted` — no layer can release them — but still build.
    pub cyclic: Vec<String>,
}

/// Layer a module→declared-dependencies map for `sorted_deps`.
///
/// Self-dependencies are dropped. A dependency naming no known module joins
/// the layering as a leaf so the dependent still sorts, but the unknown name
/// never appears in the output. Each pass releases every node with no
/// remaining dependencies (name-sorted) and removes it from the other nodes'
/// remaining sets; when a pass releases nothing, whatever is left is cyclic.
fn layered_order(deps: &BTreeMap<String, Vec<String>>) -> LayeredOrder {
    let mut remaining: BTreeMap<String, Vec<String>> = deps
        .iter()
        .map(|(name, declared)| {
            (
                name.clone(),
                declared.iter().filter(|d| *d != name).cloned().collect(),
            )
        })
        .collect();
    let unknown: Vec<String> = remaining
        .values()
        .flatten()
        .filter(|d| !remaining.contains_key(*d))
        .cloned()
        .collect();
    for name in unknown {
        remaining.insert(name, Vec::new());
    }

    let mut sorted = Vec::new();
    loop {
        // BTreeMap iterates keys in name order, so the layer is sorted.
        let layer: Vec<String> = remaining
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(name, _)| name.clone())
            .collect();
        if layer.is_empty() {
            break;
        }
        for name in &layer {
            remaining.remove(name);
        }
        for deps in remaining.values_mut() {
            deps.retain(|d| !layer.contains(d));
        }
        sorted.extend(layer.into_iter().filter(|name| deps.contains_key(name)));
    }
    // Leftover unknown names are impossible — they carry no dependencies and
    // release in the first layer — but the filter keeps the guarantee local.
    let cyclic: Vec<String> = remaining
        .into_keys()
        .filter(|name| deps.contains_key(name))
        .collect();
    LayeredOrder { sorted, cyclic }
}

/// One module's full dependency list for its manifest entry: the declared
/// deps in declaration order — deduplicated, self removed — followed by the
/// transitive deps appended in pre-order, first occurrence wins. The visited
/// guard ends recursion on the first revisit, so a cyclic graph still
/// terminates.
fn backfilled_deps(deps: &BTreeMap<String, Vec<String>>, module: &str) -> Vec<String> {
    expand_declared_deps(
        deps,
        module,
        deps.get(module).map(Vec::as_slice).unwrap_or(&[]),
    )
}

/// The dependency list a file's own `declared` set expands to — the same
/// traversal `backfilled_deps` performs, but seeded by an explicit declared
/// list rather than a map lookup, for a file whose name is not a module in
/// the map (a plain path-keyed asset). `self_name` only seeds the
/// self-dependency guard.
fn expand_declared_deps(
    deps: &BTreeMap<String, Vec<String>>,
    self_name: &str,
    declared: &[String],
) -> Vec<String> {
    fn walk<'a>(
        node: &'a str,
        deps: &'a BTreeMap<String, Vec<String>>,
        seen: &mut HashSet<&'a str>,
        walked: &mut HashSet<&'a str>,
        result: &mut Vec<String>,
    ) {
        if !walked.insert(node) {
            return;
        }
        if let Some(declared) = deps.get(node) {
            for dep in declared {
                if seen.insert(dep) {
                    result.push(dep.clone());
                }
                walk(dep, deps, seen, walked, result);
            }
        }
    }

    let mut seen: HashSet<&str> = HashSet::from([self_name]);
    let mut result: Vec<String> = declared
        .iter()
        .filter(|dep| seen.insert(dep.as_str()))
        .cloned()
        .collect();
    let mut walked = HashSet::new();
    for dep in declared {
        walk(dep, deps, &mut seen, &mut walked, &mut result);
    }
    result
}

/// One declared CSS module collected during the style pass — everything
/// sorted_deps emission needs afterwards.
struct CssModule {
    name: String,
    output: String,
    version: u32,
}

/// One style file whose `@module`/`@requires` were registered before the
/// compile pass — the record each expanded-dependency update is built from.
struct StyleScan<'a> {
    source: &'a DiscoveredSource,
    /// The asset key the file compiles under (`css/{path}.css`)
    key: String,
    /// Its declared module name, when the file carries an @module comment
    module_name: Option<String>,
    /// Its declared @requires list
    declared: Vec<String>,
}

/// Warn once about the modules that sorted as cyclic for one asset type.
fn warn_cyclic(asset_type: &str, cyclic: &[String]) {
    if !cyclic.is_empty() {
        eprintln!(
            "Warning: circular dependency among {} modules (built, but excluded from sorted_deps): {}",
            asset_type,
            cyclic.join(", ")
        );
    }
}

/// The last-modified stamp for one source file: its mtime, clamped to
/// SOURCE_DATE_EPOCH when the build read one. A reproducible build sets the
/// variable so the same source checkout stamps identically on every host;
/// an unreadable mtime falls back to now rather than failing the build.
fn source_stamp(source_path: &Path, epoch_clamp: Option<i64>) -> DateTime<Utc> {
    let mtime: DateTime<Utc> = fs::metadata(source_path)
        .and_then(|m| m.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now());
    match epoch_clamp.and_then(|secs| DateTime::from_timestamp(secs, 0)) {
        Some(clamp) => mtime.min(clamp),
        None => mtime,
    }
}

/// SOURCE_DATE_EPOCH, when the build environment sets it — the
/// reproducible-build clamp read once per build rather than inside the
/// manifest's per-asset update path.
fn source_date_epoch() -> Option<i64> {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
}

pub struct AssetGraph {
    config: Config,
    mode: BuildMode,
    js_deps: BTreeMap<String, Vec<String>>,
    /// Module name → (source file, whether it was found under a source dir).
    module_map: HashMap<String, (PathBuf, bool)>,
    storage: StorageBackend,
    /// The resolver every read and write of a compiled file goes through —
    /// flat layout or beside-source, per `config.output.layout`.
    output_dirs: OutputDirs,
    /// The build's stamp clamp — SOURCE_DATE_EPOCH, read once per build.
    stamp_clamp: Option<i64>,
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
        let output_dirs = config.output_dirs();
        AssetGraph {
            config,
            mode: BuildMode::Standard,
            js_deps: BTreeMap::new(),
            module_map: HashMap::new(),
            storage,
            output_dirs,
            stamp_clamp: None,
            // Compiled once for reuse — pattern shared via MODULE_NAME_RE
            requires_re: Regex::new(&format!(r"@requires\s+({})", MODULE_NAME_RE)).unwrap(),
            module_re: Regex::new(&format!(r#"@module\s+"?({})"?"#, MODULE_NAME_RE)).unwrap(),
        }
    }

    pub fn set_mode(&mut self, mode: BuildMode) {
        self.mode = mode;
    }

    /// How many history entries a rebuilt asset retains: the configured
    /// bound on dist — a retained version's pinned URL must still serve —
    /// and none on local, where only the current file is kept on disk and
    /// recorded history could never resolve to a file.
    fn history_bound(&self) -> usize {
        if self.storage.is_dist() {
            self.config.storage.keep_versions
        } else {
            0
        }
    }

    pub fn build(&mut self) -> Result<Manifest> {
        self.config.validate()?;
        // Every version stamp this build writes shares the one clamp.
        self.stamp_clamp = source_date_epoch();

        // Load existing manifest for version history. A load failure —
        // corrupt JSON, an unreadable file — fails the build rather than
        // silently resetting the history the deployable URLs depend on.
        let mut manifest = Manifest::load(&self.config.manifest_path)?;
        manifest.generated = Utc::now().to_rfc3339();

        // Snapshot the previous manifest's world before rebuilding the
        // active map: a deleted source's retired entry may still need its
        // module alias, recorded generated directories keep their discovery
        // exclusion across config changes, and the old outputs anchor the
        // history migration below.
        let previous_modules = manifest.modules.clone();
        let previous_generated_dirs = manifest.generated_dirs.clone();
        let old_assets = manifest.assets.clone();

        // Modules and sorted_deps are rebuilt fresh each build
        manifest.modules.clear();
        manifest.sorted_deps.clear();

        if self.config.source_maps && self.config.debug_mode {
            // TODO: source map generation not yet implemented
            eprintln!("Note: source_maps=true but source map generation is not yet implemented");
        }

        // Directories this build must never treat as sources: the
        // configured output roots, every recorded generated directory, and
        // the parent of every existing file the old manifest's versioned
        // outputs resolve to — an old flat or beside-source location
        // otherwise re-enters discovery the moment the layout or
        // build-dir name changes.
        let known_dirs: Vec<PathBuf> = previous_generated_dirs
            .iter()
            .map(|label| self.generated_dir_path(label))
            .collect::<Result<_>>()?;
        let mut excluded_dirs: Vec<PathBuf> = vec![
            self.config.output.base.clone(),
            self.config.output.js.clone(),
            self.config.output.css.clone(),
            self.config.output.images.clone(),
        ];
        excluded_dirs.extend(known_dirs.iter().cloned());
        for (key, entry) in &old_assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            for output in std::iter::once(&entry.output)
                .chain(entry.history.iter().map(|history| &history.output))
            {
                for candidate in self.existing_output_candidates(output, kind, &known_dirs) {
                    if candidate.is_file() {
                        if let Some(parent) = candidate.parent() {
                            excluded_dirs.push(parent.to_path_buf());
                        }
                    }
                }
            }
        }
        excluded_dirs.sort();
        excluded_dirs.dedup();

        // Track seen asset keys to prune or retire stale assets after build
        let mut seen_asset_keys: HashSet<String> = HashSet::new();

        // Discover every asset file: each source dir, then each include
        // path, walked recursively and classified by extension. One dir may
        // hold JS and styles together — the folder layout means nothing.
        let discovered = self.discover_sources(&excluded_dirs)?;

        // Register JS modules
        self.js_deps.clear();
        self.module_map.clear();
        for source in discovered.iter().filter(|s| s.kind == SourceKind::Js) {
            self.register_js_module(&source.path, &source.root, source.from_source_dir)?;
        }
        let js_order = layered_order(&self.js_deps);
        warn_cyclic("js", &js_order.cyclic);

        // Backfill each module's transitive dependencies
        let expanded_deps: HashMap<String, Vec<String>> = self
            .js_deps
            .keys()
            .map(|module| (module.clone(), backfilled_deps(&self.js_deps, module)))
            .collect();

        // Build sorted_deps for JS
        let mut js_sorted_deps: Vec<(String, String, u32)> = Vec::new();
        let js_sorted: HashSet<&String> = js_order.sorted.iter().collect();

        // Cyclic modules build like the rest, after the layered ones; only
        // sorted names are written to sorted_deps.
        for module in js_order.sorted.iter().chain(&js_order.cyclic) {
            if let Some((source_path, from_source_dir)) = self.module_map.get(module) {
                // Add to modules map: declared module name -> manifest asset key.
                // Consumers (the combo resolver, the language runtimes, this
                // rollup) look the value up in `assets`, so it must be a key of
                // that map and never a source path.
                let key = format!("js/{}.js", module.replace('.', "/"));
                record_module(&mut manifest, module, &key, "js");

                // Use expanded dependencies
                let deps = expanded_deps.get(module).cloned().unwrap_or_default();
                self.process_js_with_deps(
                    source_path.clone(),
                    *from_source_dir,
                    module,
                    deps,
                    &mut manifest,
                )?;

                // Add to sorted_deps and track seen keys
                seen_asset_keys.insert(key.clone());
                if js_sorted.contains(module) {
                    if let Some(entry) = manifest.assets.get(&key) {
                        js_sorted_deps.push((module.clone(), entry.output.clone(), entry.version));
                    }
                }
            }
        }

        // Store JS sorted_deps in manifest
        manifest
            .sorted_deps
            .insert("js".to_string(), js_sorted_deps);

        // Register every style file's @module/@requires before compiling
        // anything: the expanded dependency vector has to exist before a
        // version decision, exactly like the JS registration above. The
        // first file to declare a module name in scan order keeps it; a
        // later duplicate warns and is skipped entirely — compiled under its
        // own key it would still join the rollup as a path-keyed asset, so
        // static CSS would not keep the first declaration.
        let mut css_deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut seen_style_keys: HashSet<String> = HashSet::new();
        let mut styles: Vec<StyleScan<'_>> = Vec::new();

        for source in discovered
            .iter()
            .filter(|s| matches!(s.kind, SourceKind::Scss | SourceKind::Css))
        {
            let path = &source.path;
            // Skip partials — only `_`-prefixed SCSS is a partial; a plain
            // `.css` file is always an asset in its own right.
            if source.kind == SourceKind::Scss
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('_'))
            {
                continue;
            }
            // Track every style asset key to prevent pruning, even without @module
            let rel_path = path.strip_prefix(&source.root)?;
            let base_name = rel_path
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            let css_key = format!("css/{}.css", base_name);
            if !seen_style_keys.insert(css_key.clone()) {
                eprintln!(
                    "Warning: {} duplicates style asset {}; keeping the first",
                    path.display(),
                    css_key
                );
                continue;
            }
            let content = fs::read_to_string(path)?;
            let (module_name, declared) = self.style_declarations(&content, path)?;
            match &module_name {
                // A duplicate declaration keeps the first file only: the
                // later one is not compiled, recorded, or counted as seen —
                // a dist build retires a previously recorded entry at its key.
                Some(name) if css_deps.contains_key(name) => {
                    eprintln!(
                        "Warning: {} redeclares css module {}; keeping the first",
                        path.display(),
                        name
                    );
                    continue;
                }
                Some(name) => {
                    css_deps.insert(name.clone(), declared.clone());
                }
                None => {}
            }
            seen_asset_keys.insert(css_key.clone());
            styles.push(StyleScan {
                source,
                key: css_key,
                module_name,
                declared,
            });
        }

        // A style without @module still joins the module world: it takes a
        // path-derived name — the relative path under its root, extension
        // stripped, separators as dots, exactly like JS — as long as the
        // name is valid and unclaimed. A name that collides with a declared
        // module or an earlier derived name, or that is not a module name
        // at all, warns and leaves the file unnamed: it still compiles
        // under its path key, just outside the named-module index.
        for style in &mut styles {
            if style.module_name.is_some() {
                continue;
            }
            let derived = style
                .source
                .path
                .strip_prefix(&style.source.root)?
                .with_extension("")
                .to_string_lossy()
                .replace(['/', '\\'], ".");
            if !is_module_name(&derived) {
                eprintln!(
                    "Warning: {} has no @module and derives invalid module name {}; leaving it unnamed",
                    style.source.path.display(),
                    derived
                );
                continue;
            }
            if css_deps.contains_key(&derived) {
                eprintln!(
                    "Warning: {} derives css module name {} that is already claimed; leaving it unnamed",
                    style.source.path.display(),
                    derived
                );
                continue;
            }
            if !style.source.from_source_dir {
                eprintln!(
                    "Warning: {} in an include path has no @module; using path-derived name {}",
                    style.source.path.display(),
                    derived
                );
            }
            css_deps.insert(derived.clone(), style.declared.clone());
            style.module_name = Some(derived);
        }

        // Layer the declared CSS dependencies — the layering consumes the
        // declarations; the stored dependencies are the backfilled list.
        let css_order = layered_order(&css_deps);
        warn_cyclic("css", &css_order.cyclic);

        let mut css_modules: Vec<CssModule> = Vec::new();
        for style in &styles {
            let expanded = expand_declared_deps(
                &css_deps,
                style.module_name.as_deref().unwrap_or(&style.key),
                &style.declared,
            );

            if let Some(name) = &style.module_name {
                // The surviving file owns its module name — a later duplicate
                // declaration never reaches this pass.
                // A declared module whose source moved to another
                // source-relative key carries its entry — version sequence
                // and history included — into the new key, as long as the
                // destination is not already occupied. A plain style keeps
                // its path-based identity.
                if let Some(old_key) = previous_modules.get(name) {
                    if old_key != &style.key
                        && old_key.starts_with("css/")
                        && !manifest.assets.contains_key(&style.key)
                    {
                        if let Some(entry) = manifest.assets.remove(old_key) {
                            manifest.assets.insert(style.key.clone(), entry);
                        }
                    }
                }
                record_module(&mut manifest, name, &style.key, "css");
            }

            if let Some(record) = self.process_scss(
                &style.source.path,
                &style.source.root,
                style.source.from_source_dir,
                style.module_name.as_deref(),
                expanded,
                &mut manifest,
            )? {
                css_modules.push(record);
            }
        }

        let mut css_sorted_deps: Vec<(String, String, u32)> = Vec::new();
        for name in &css_order.sorted {
            if let Some(m) = css_modules.iter().find(|m| &m.name == name) {
                css_sorted_deps.push((name.clone(), m.output.clone(), m.version));
            }
        }

        // Store CSS sorted_deps in manifest
        manifest
            .sorted_deps
            .insert("css".to_string(), css_sorted_deps);

        // Unseen assets leave the active set: a local build drops them —
        // nothing local serves an old pin — while a dist build retires
        // them: their output and history stay recorded and materialized so
        // exact pinned URLs keep resolving to the original bytes.
        if self.storage.is_dist() {
            for (key, entry) in manifest.assets.iter_mut() {
                if !seen_asset_keys.contains(key) {
                    entry.retired = true;
                }
            }
            // A retired asset keeps its module alias only when no active
            // asset claimed the name for this type — the alias is how an
            // exact pinned URL still finds it. record_module keeps the
            // cross-type conventional-key fallback intact.
            for (module, key) in &previous_modules {
                let Some(asset_type) = key.split_once('/').map(|(t, _)| t) else {
                    continue;
                };
                let is_retired = manifest.assets.get(key).is_some_and(|entry| entry.retired);
                if is_retired && manifest.asset_key_for_module(module, asset_type).is_none() {
                    record_module(&mut manifest, module, key, asset_type);
                }
            }
        } else {
            manifest
                .assets
                .retain(|key, _| seen_asset_keys.contains(key));
        }

        // Move every recorded version to the file this build's layout
        // resolves it to: the history migration is a copy verified against
        // the recorded hash, never a rebuild of old bytes, and a retired
        // entry's current version must exist with its source gone.
        for (key, entry) in manifest.assets.iter_mut() {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            let ext = kind.ext();

            if entry.retired {
                // A retired entry's source is gone — where its current
                // version belongs comes from the recorded source label and
                // the key's own path shape.
                let source_path = self.config.base_dir.join(&entry.source);
                let from_source_dir = self
                    .config
                    .source_dirs()
                    .iter()
                    .any(|root| source_path.starts_with(root));
                let flat_rel = key
                    .strip_prefix(&format!("{ext}/"))
                    .and_then(|rest| rest.strip_suffix(&format!(".{ext}")))
                    .unwrap_or(key);
                let output_base =
                    self.output_base_for(&source_path, from_source_dir, flat_rel, ext);
                let new_output = format!(
                    "{}.{}.v{}.min.{}",
                    output_base, entry.hash, entry.version, ext
                );
                self.materialize_version(
                    key,
                    RelocatedVersion {
                        version: entry.version,
                        hash: &entry.hash,
                        recorded_output: &entry.output,
                        new_output: &new_output,
                    },
                    kind,
                    &known_dirs,
                )?;
                entry.output = new_output;
            }

            let tail = format!(".{}.v{}.min.{}", entry.hash, entry.version, ext);
            let Some(output_base) = entry.output.strip_suffix(&tail).map(str::to_string) else {
                // A manifest written before versioned outputs recorded a
                // plain path — its history keeps its recorded locations.
                continue;
            };
            for history in &mut entry.history {
                let new_output = format!(
                    "{}.{}.v{}.min.{}",
                    output_base, history.hash, history.version, ext
                );
                self.materialize_version(
                    key,
                    RelocatedVersion {
                        version: history.version,
                        hash: &history.hash,
                        recorded_output: &history.output,
                        new_output: &new_output,
                    },
                    kind,
                    &known_dirs,
                )?;
                history.output = new_output;
            }
        }

        // Record where generated files live for the next build's scanner
        // and for future output-root moves: the configured output roots and
        // the parent of every recorded file, project-relative labels only,
        // carried forward so a label outlives the config that created it.
        let mut generated: BTreeSet<String> = previous_generated_dirs.iter().cloned().collect();
        for dir in [
            &self.config.output.base,
            &self.config.output.js,
            &self.config.output.css,
            &self.config.output.images,
        ] {
            if let Some(label) = generated_label(&self.config.base_dir, dir) {
                generated.insert(label);
            }
        }
        for (key, entry) in &manifest.assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            for output in std::iter::once(&entry.output)
                .chain(entry.history.iter().map(|history| &history.output))
            {
                if let Some(parent) = self
                    .output_dirs
                    .compiled_file(output, kind)
                    .and_then(|path| path.parent().map(Path::to_path_buf))
                {
                    if let Some(label) = generated_label(&self.config.base_dir, &parent) {
                        generated.insert(label);
                    }
                }
            }
        }
        manifest.generated_dirs = generated.into_iter().collect();
        // The manifest carries the URL policy so adapters need no setting of
        // their own; a toggle changes rendered tokens only — never hashes,
        // versions, timestamps, or history.
        manifest.hash_urls = self.config.jam.hash_urls;

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

        // Prune old file versions from storage. Each entry's resolved output
        // names the exact file and directory its current version occupies,
        // so pruning works the same in either output layout — and a stem
        // prefix can never swallow a different asset's files.
        for (key, entry) in &manifest.assets {
            let kind = match key.split_once('/').map(|(t, _)| t) {
                Some("js") => AssetType::Js,
                Some("css") => AssetType::Css,
                _ => continue,
            };
            if let Some(file) = self.output_dirs.compiled_file(&entry.output, kind) {
                // Retention comes from the manifest itself: the resolved
                // files of every history entry still referenced. A count of
                // what happens to be on disk cannot substitute — a stray
                // file sharing a version number would displace a version a
                // pinned URL still serves.
                let retained: Vec<PathBuf> = entry
                    .history
                    .iter()
                    .filter_map(|h| self.output_dirs.compiled_file(&h.output, kind))
                    .collect();
                self.storage.prune_versions(&file, &retained)?;
            }
        }

        // Prune the locations the migration just moved away from: each old
        // output's directory is pruned by its exact stem and extension, and
        // the old file itself is removed only when the new manifest no
        // longer references it. Missing files are harmless; unrelated
        // siblings and unversioned rollups are never touched.
        let mut new_referenced: Vec<PathBuf> = Vec::new();
        for (key, entry) in &manifest.assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            for output in std::iter::once(&entry.output)
                .chain(entry.history.iter().map(|history| &history.output))
            {
                if let Some(path) = self.output_dirs.compiled_file(output, kind) {
                    new_referenced.push(path);
                }
            }
        }
        for (key, old_entry) in &old_assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            for output in std::iter::once(&old_entry.output)
                .chain(old_entry.history.iter().map(|history| &history.output))
            {
                for anchor in self
                    .existing_output_candidates(output, kind, &known_dirs)
                    .into_iter()
                    .filter(|candidate| candidate.is_file())
                {
                    self.storage.prune_versions(&anchor, &new_referenced)?;
                    if !new_referenced.contains(&anchor) {
                        match fs::remove_file(&anchor) {
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            result => result?,
                        }
                    }
                }
            }
        }

        // Render paste: templates and the static files their tags
        // reference, so an HTML site serves without a request-time server.
        // No-op unless config.templates is set; runs after the manifest is
        // saved on every successful build, watch rebuilds included.
        crate::templates::render_templates(&self.config, &manifest)?;

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
        // follow, in key order. A retired asset never joins — it exists only
        // to answer an exact pinned URL, not a fresh rollup.
        let mut remaining: Vec<String> = manifest
            .assets
            .iter()
            .filter(|(key, entry)| {
                key.starts_with(&prefix) && !seen.contains(*key) && !entry.retired
            })
            .map(|(key, _)| key.clone())
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
        let kind = match asset_type {
            "js" => AssetType::Js,
            _ => AssetType::Css,
        };
        let parts = self
            .rollup_asset_keys(manifest, asset_type)
            .into_iter()
            .filter_map(|key| manifest.assets.get(&key).map(|entry| entry.output.clone()))
            .map(|output| {
                let path = self
                    .output_dirs
                    .compiled_file(&output, kind)
                    .ok_or_else(|| {
                        anyhow::anyhow!("invalid {} rollup path: {}", asset_type, output)
                    })?;
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

    /// Walk every source root — the configured `source.dirs` plus the legacy
    /// js/scss roots, then every include path — and classify each file by
    /// extension. One dir may hold JS and styles together: the folder layout
    /// means nothing to discovery, only the extension does. `sort_by_file_name`
    /// keeps walk order deterministic, and the first root to claim a module
    /// name or style asset key wins. `generated_dirs` — the configured output
    /// roots, the manifest's recorded generated directories, and the parents
    /// of the old manifest's existing outputs — are never sources, and a
    /// missing optional root is skipped. Walk errors fail the build rather
    /// than silently publishing a reduced manifest.
    fn discover_sources(&self, generated_dirs: &[PathBuf]) -> Result<Vec<DiscoveredSource>> {
        let roots = self
            .config
            .source_dirs()
            .into_iter()
            .map(|root| (root, true))
            .chain(
                self.config
                    .include_paths
                    .iter()
                    .cloned()
                    .map(|root| (root, false)),
            );
        let mut found = Vec::new();
        for (root, from_source_dir) in roots {
            // An optional root that is absent is skipped; every other stat
            // error propagates — a root that exists but cannot be read must
            // not silently produce a reduced manifest.
            match fs::metadata(&root) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
            for entry in WalkDir::new(&root)
                .sort_by_file_name()
                .into_iter()
                // Generated trees are pruned, not just filtered after the
                // fact: descending into them would walk every retained
                // version on every build.
                .filter_entry(|e| {
                    !is_build_output(e, &self.config.output.build_dir_name)
                        && !generated_dirs
                            .iter()
                            .any(|directory| e.path().starts_with(directory))
                })
            {
                let entry = entry?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                // A generated versioned filename is never a source under a
                // configured root — a stale build directory whose configured
                // name has since changed cannot feed its output back in. An
                // include path may deliberately point at a built
                // distribution, so the check applies to source roots only.
                if from_source_dir && crate::storage::is_versioned_file(path) {
                    continue;
                }
                let kind = match path.extension().and_then(|ext| ext.to_str()) {
                    Some("js") => SourceKind::Js,
                    Some("scss") => SourceKind::Scss,
                    Some("css") => SourceKind::Css,
                    _ => continue,
                };
                found.push(DiscoveredSource {
                    path: path.to_path_buf(),
                    root: root.clone(),
                    from_source_dir,
                    kind,
                });
            }
        }
        Ok(found)
    }

    /// Register one JS source file's module name and dependencies.
    /// Files with @module comments use that name; others derive from the
    /// path relative to their walk root. The first root to claim a name
    /// wins; later files with the same name are skipped.
    fn register_js_module(
        &mut self,
        path: &Path,
        root: &Path,
        from_source_dir: bool,
    ) -> Result<()> {
        // A source that cannot be read fails the build — silently dropping
        // it would publish a manifest missing a module it promises.
        let content = fs::read_to_string(path)?;

        // Find module name from @module comment
        let (module_name, derived) = if let Some(caps) = self.module_re.captures(&content) {
            // Convert slash to dot: paste/ui/heroscroll -> paste.ui.heroscroll
            (caps.get(1).unwrap().as_str().replace('/', "."), false)
        } else {
            // Derive from path (fallback)
            (
                path.strip_prefix(root)
                    .unwrap_or(path)
                    .with_extension("")
                    .to_string_lossy()
                    .replace(['/', '\\'], "."),
                true,
            )
        };

        // A name that is not dot-separated word segments could join a
        // path outside the output tree (`@module /tmp` normalizes to
        // `.tmp`, an absolute tail) — refuse the file outright.
        if !is_module_name(&module_name) {
            anyhow::bail!(
                "invalid module name {:?} in {}",
                module_name,
                path.display()
            );
        }

        // Don't overwrite a module already found in an earlier root
        if self.module_map.contains_key(&module_name) {
            return Ok(());
        }

        // An include-path file's module name is invisible at a glance —
        // say when the fallback supplies it.
        if derived && !from_source_dir {
            eprintln!(
                "Warning: {} in an include path has no @module; using path-derived name {}",
                path.display(),
                module_name
            );
        }

        // Find dependencies (convert slash to dot)
        let deps: Vec<String> = self
            .requires_re
            .captures_iter(&content)
            .map(|c| c.get(1).unwrap().as_str().replace('/', "."))
            .collect();
        for dep in &deps {
            if !is_module_name(dep) {
                anyhow::bail!("invalid @requires name {:?} in {}", dep, path.display());
            }
        }

        self.js_deps.insert(module_name.clone(), deps);
        self.module_map
            .insert(module_name, (path.to_path_buf(), from_source_dir));
        Ok(())
    }

    /// The prefix an asset entry's versioned `output` is built on — the
    /// manifest records it and `OutputDirs::compiled_file` resolves it back
    /// to a file.
    ///
    /// In `flat` layout every output is `/{type}/{rel}` and lands under the
    /// type's own output directory. In `beside-source` layout a source-dir
    /// file inside base_dir compiles into `{file dir}/{build-dir-name}/{file
    /// stem}` beside its source; a file from an include path — never written
    /// into someone else's tree — keeps flat placement under output.base.
    /// Either way a beside-source output is recorded relative to base_dir,
    /// which is also the output root the runtime is handed.
    fn output_base_for(
        &self,
        source_path: &Path,
        from_source_dir: bool,
        flat_rel: &str,
        asset_type: &str,
    ) -> String {
        if self.config.output.layout != "beside-source" {
            return format!("/{}/{}", asset_type, flat_rel);
        }
        if from_source_dir {
            if let (Ok(rel), Some(stem)) = (
                source_path.strip_prefix(&self.config.base_dir),
                source_path.file_stem(),
            ) {
                let dir = rel
                    .parent()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                let stem = stem.to_string_lossy();
                return if dir.is_empty() {
                    format!("/{}/{}", self.config.output.build_dir_name, stem)
                } else {
                    format!("/{}/{}/{}", dir, self.config.output.build_dir_name, stem)
                };
            }
        }
        let rel_base = self
            .config
            .output
            .base
            .strip_prefix(&self.config.base_dir)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|s| !s.is_empty());
        match rel_base {
            Some(rel) => format!("/{}/{}/{}", rel, asset_type, flat_rel),
            None => format!("/{}/{}", asset_type, flat_rel),
        }
    }

    fn process_js_with_deps(
        &self,
        source_path: PathBuf,
        from_source_dir: bool,
        module: &str,
        deps: Vec<String>,
        manifest: &mut Manifest,
    ) -> Result<()> {
        let content = fs::read_to_string(&source_path)?;

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

        // Hash the emitted bytes, not the source: only an output change may
        // roll the version — a comment edit that the printer drops produces
        // identical bytes and keeps the name, while a whitespace change that
        // reaches the output gets a new one.
        let hash = format!("{:x}", md5::compute(output_content.as_bytes()));

        let key = format!("js/{}.js", module.replace('.', "/"));
        let rel_path = module.replace('.', "/");
        let output_base = self.output_base_for(&source_path, from_source_dir, &rel_path, "js");

        let entry = manifest.update_asset_with_history(AssetUpdate {
            key: &key,
            source: &self.source_label(&source_path),
            hash: &hash,
            output_base: &output_base,
            ext: "js",
            dependencies: deps, // Use expanded transitive dependencies
            max_history: self.history_bound(),
            stamp: source_stamp(&source_path, self.stamp_clamp),
        });

        // Resolve the target through the same resolver every read uses, and
        // write it unconditionally: existence is not correctness — the
        // backend compares bytes, so an unchanged output keeps its mtime
        // while a tampered or truncated one is repaired.
        let target = self
            .output_dirs
            .compiled_file(&entry.output, AssetType::Js)
            .ok_or_else(|| anyhow::anyhow!("invalid output path: {}", entry.output))?;
        self.storage
            .write_file(&target, output_content.as_bytes())?;

        Ok(())
    }

    /// Parse one style file's `@module` name and `@requires` list from its
    /// content, validating both — shared by the registration scan and the
    /// compile pass so a declared name is checked exactly once per shape.
    /// Declared names land in the manifest and in output paths — refuse
    /// anything that is not dot-separated word segments.
    fn style_declarations(
        &self,
        content: &str,
        source_path: &Path,
    ) -> Result<(Option<String>, Vec<String>)> {
        let module_name = self
            .module_re
            .captures(content)
            .map(|caps| caps.get(1).unwrap().as_str().replace('/', "."));
        let deps: Vec<String> = self
            .requires_re
            .captures_iter(content)
            .map(|c| c.get(1).unwrap().as_str().replace('/', "."))
            .collect();
        if let Some(name) = &module_name {
            if !is_module_name(name) {
                anyhow::bail!(
                    "invalid module name {:?} in {}",
                    name,
                    source_path.display()
                );
            }
        }
        for dep in &deps {
            if !is_module_name(dep) {
                anyhow::bail!(
                    "invalid @requires name {:?} in {}",
                    dep,
                    source_path.display()
                );
            }
        }
        Ok((module_name, deps))
    }

    /// Process one style file — `.scss` or plain `.css`, which is valid
    /// SCSS — and return its module record for the layering and
    /// sorted_deps emission that follow the walk. `module_name` is the
    /// effective name the registration pass resolved — declared or
    /// path-derived, `None` when neither applies — and `dependencies` is
    /// the expanded list the caller computed from the registration map —
    /// like JS, the version decision sees the full dependency metadata, so
    /// a transitive-dependency edit still versions the file.
    fn process_scss(
        &self,
        source_path: &Path,
        base_dir: &Path,
        from_source_dir: bool,
        module_name: Option<&str>,
        dependencies: Vec<String>,
        manifest: &mut Manifest,
    ) -> Result<Option<CssModule>> {
        // Compile through grass with every source dir and include path as a
        // load path — an @import may resolve against any of them. Use
        // from_path so grass knows the file's location for relative @import
        // resolution; plain .css is valid SCSS and takes the same path.
        let mut options = grass::Options::default();
        for dir in self
            .config
            .source_dirs()
            .iter()
            .chain(&self.config.include_paths)
        {
            options = options.load_path(dir);
        }

        let css_content = grass::from_path(source_path, &options)?;

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

        // Hash the emitted bytes, not the source — same contract as the JS path.
        let hash = format!("{:x}", md5::compute(output_content.as_bytes()));

        let rel_path = source_path.strip_prefix(base_dir)?;
        let base_name = rel_path
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let key = format!("css/{}.css", base_name);
        let output_base = self.output_base_for(source_path, from_source_dir, &base_name, "css");

        let entry = manifest.update_asset_with_history(AssetUpdate {
            key: &key,
            source: &self.source_label(source_path),
            hash: &hash,
            output_base: &output_base,
            ext: "css",
            dependencies,
            max_history: self.history_bound(),
            stamp: source_stamp(source_path, self.stamp_clamp),
        });

        // Capture entry info before moving
        let output = entry.output.clone();
        let version = entry.version;

        // Resolve the target through the same resolver every read uses, and
        // write it unconditionally: existence is not correctness — the
        // backend compares bytes, so an unchanged output keeps its mtime
        // while a tampered or truncated one is repaired.
        let target = self
            .output_dirs
            .compiled_file(&entry.output, AssetType::Css)
            .ok_or_else(|| anyhow::anyhow!("invalid output path: {}", entry.output))?;
        self.storage
            .write_file(&target, output_content.as_bytes())?;

        // Return the module record when the file is named; sorted_deps
        // emission consumes it.
        Ok(module_name.map(|name| CssModule {
            name: name.to_string(),
            output,
            version,
        }))
    }

    /// The asset type a manifest key belongs to (`js/…`, `css/…`), or None
    /// for a key outside the compiled set.
    fn key_asset_type(key: &str) -> Option<AssetType> {
        match key.split_once('/').map(|(t, _)| t) {
            Some("js") => Some(AssetType::Js),
            Some("css") => Some(AssetType::Css),
            _ => None,
        }
    }

    /// Resolve one recorded `generated_dirs` label to a path under
    /// base_dir. A label must be a relative path of ordinary components —
    /// anything else cannot name a generated directory and fails the build
    /// rather than silently dropping the exclusion.
    fn generated_dir_path(&self, label: &str) -> Result<PathBuf> {
        let rel = Path::new(label);
        anyhow::ensure!(
            !label.is_empty()
                && !label.contains(['\\', ':'])
                && rel.components().count() > 0
                && rel
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "manifest generated_dirs entry {:?} is not a project-relative directory",
            label
        );
        Ok(self.config.base_dir.join(rel))
    }

    /// The file locations a recorded `output` may already exist at, in
    /// resolver order: the current layout first, then the explicit flat
    /// layout, then the project-relative beside-source layout — after which
    /// each recorded generated directory is tried by basename. These are
    /// the locations this build and its predecessors can have written, not
    /// a filesystem search; every resolver candidate passed
    /// `compiled_file`'s path validation, and the basename fallback applies
    /// only to an output at least one resolver accepted.
    fn existing_output_candidates(
        &self,
        output: &str,
        kind: AssetType,
        known_dirs: &[PathBuf],
    ) -> Vec<PathBuf> {
        let resolvers = [
            self.output_dirs.clone(),
            OutputDirs {
                base: self.config.output.base.clone(),
                js: self.config.output.js.clone(),
                css: self.config.output.css.clone(),
            },
            OutputDirs::under(&self.config.base_dir),
        ];
        let mut candidates = Vec::new();
        let mut validated = false;
        for resolver in &resolvers {
            if let Some(path) = resolver.compiled_file(output, kind) {
                validated = true;
                if !candidates.contains(&path) {
                    candidates.push(path);
                }
            }
        }
        if validated {
            if let Some(basename) = Path::new(output.trim_start_matches('/')).file_name() {
                for dir in known_dirs {
                    let candidate = dir.join(basename);
                    if !candidates.contains(&candidate) {
                        candidates.push(candidate);
                    }
                }
            }
        }
        candidates
    }

    /// Make the file a recorded version resolves to exist at `new_output`,
    /// copying the bytes of whichever existing candidate still holds them —
    /// the recorded hash decides which candidate is real, so a tampered or
    /// stale copy is never mistaken for the retained version. A version
    /// with no existing candidate fails the build naming the version and
    /// path; candidates that exist but never match fail as corrupted
    /// historical output. Current bytes are never substituted for a
    /// historical version.
    fn materialize_version(
        &self,
        key: &str,
        file: RelocatedVersion<'_>,
        kind: AssetType,
        known_dirs: &[PathBuf],
    ) -> Result<()> {
        let target = self
            .output_dirs
            .compiled_file(file.new_output, kind)
            .ok_or_else(|| anyhow::anyhow!("invalid output path: {}", file.new_output))?;
        let mut found = false;
        for candidate in self.existing_output_candidates(file.recorded_output, kind, known_dirs) {
            if !candidate.is_file() {
                continue;
            }
            found = true;
            let bytes = fs::read(&candidate)?;
            if format!("{:x}", md5::compute(&bytes)) == file.hash {
                self.storage.write_file(&target, &bytes)?;
                return Ok(());
            }
        }
        if found {
            anyhow::bail!(
                "corrupted retained output for {} v{}: no candidate matches recorded hash {}",
                key,
                file.version,
                file.hash
            );
        }
        anyhow::bail!(
            "retained output for {} v{} is missing: {} resolves to no existing file",
            key,
            file.version,
            file.recorded_output
        );
    }
}

/// One versioned output to materialize at its new path: the version number
/// and recorded content hash that identify it, where the manifest last
/// recorded it, and where the current layout writes it.
struct RelocatedVersion<'a> {
    version: u32,
    hash: &'a str,
    recorded_output: &'a str,
    new_output: &'a str,
}

/// The project-relative label a generated directory is recorded under —
/// forward-slashed and ordinary components only — or None when the
/// directory is outside base_dir or is the project root itself, since
/// neither can be recorded safely.
fn generated_label(base_dir: &Path, dir: &Path) -> Option<String> {
    let rel = dir.strip_prefix(base_dir).ok()?;
    if rel.as_os_str().is_empty()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(rel.to_string_lossy().replace('\\', "/"))
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
pub fn watch(config: Config) -> Result<()> {
    let (tx, rx) = channel();

    let mut watcher: RecommendedWatcher = Watcher::new(
        tx,
        notify::Config::default().with_poll_interval(Duration::from_secs(1)),
    )?;

    println!("Watching for changes in:");
    for dir in config.source_dirs() {
        println!("  {:?}", dir);
    }
    println!("Press Ctrl+C to stop");

    // Only watch directories that exist on disk — the same roots
    // discovery walks.
    for dir in config.source_dirs() {
        if dir.exists() {
            watcher.watch(&dir, RecursiveMode::Recursive)?;
        } else {
            eprintln!("Warning: source directory does not exist: {:?}", dir);
        }
    }

    // Also watch include paths for SCSS changes (e.g. paste-elements modules)
    for include_path in &config.include_paths {
        if include_path.exists() {
            watcher.watch(include_path, RecursiveMode::Recursive)?;
            println!("  {:?} (include path)", include_path);
        }
    }

    // And the template tree when build-time rendering is configured —
    // a template edit is as much a source change as a module edit.
    if let Some(templates) = &config.templates {
        if templates.source.exists() {
            watcher.watch(&templates.source, RecursiveMode::Recursive)?;
            println!("  {:?} (templates)", templates.source);
        } else {
            eprintln!(
                "Warning: templates source directory does not exist: {:?}",
                templates.source
            );
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
                if event.paths.iter().any(|p| is_source_change(&config, p)) {
                    // Debounce: drain any additional events within the window
                    // to avoid multiple rebuilds from editors that emit rapid events
                    while rx.recv_timeout(debounce_duration).is_ok() {
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

/// Whether a filesystem event path is a build input for `config`. JS, SCSS
/// and CSS anywhere under a watched tree count, and HTML counts only under
/// `templates.source` — generated HTML in the output tree is a result, not
/// a source. Nothing under a configured output directory counts, so an
/// output tree nested inside a source tree cannot loop the watcher. A
/// `build-dir-name` directory is generated output for the same reason and
/// is excluded in either layout.
fn is_source_change(config: &Config, path: &Path) -> bool {
    if path.starts_with(&config.output.base)
        || path.starts_with(&config.output.js)
        || path.starts_with(&config.output.css)
        || path
            .components()
            .any(|c| c.as_os_str() == config.output.build_dir_name.as_str())
    {
        return false;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("js" | "scss" | "css") => true,
        Some("html") => config
            .templates
            .as_ref()
            .is_some_and(|t| path.starts_with(&t.source)),
        _ => false,
    }
}

/// Minify JavaScript using oxc's full compressor plus local-name mangling.
///
/// The compressor runs with an ES5 target so it never emits syntax newer than
/// the input: `function () {}` stays a function and is NEVER rewritten into an
/// arrow function, which avoids the minify-js 0.6 bug where constructor
/// functions became arrows and `new` threw "e is not a constructor", breaking
/// paste.js's OOP system.
///
/// Top-level names are not mangled (`top_level: false`): separately built
/// files share the global scope, so a `var` or `function` declared at the top
/// of one file may be referenced by another. Property names and string
/// literals are never renamed either, so `module['name']`-style exports and
/// `paste.define('name', ...)` module names are preserved.
///
/// The printer must never raise the syntax level of the input either: in
/// minify mode oxc's codegen prints plain string literals as template
/// literals (`` `x` `` is one byte shorter to escape than `"x"`), which is
/// ES2015 syntax and a hard parse error on the ES5 engines paste.js ships
/// polyfills for. `restore_string_literals` rewrites every substitution-free,
/// untagged template literal in the printed output back to a quoted string.
/// For the same reason `restore_longhand_object_properties` rewrites the
/// `{key}` shorthand the codegen emits for a `{key: key}` property back to
/// longhand.
///
/// On any parse error (or panic) we return Err so the caller falls back to the
/// original, unminified source instead of emitting broken output.
fn minify_js(content: &str) -> Result<String> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<String> {
        let printed = print_js(content)?;
        let printed = restore_string_literals(&printed)?;
        restore_longhand_object_properties(&printed)
    }));

    match result {
        Ok(inner) => inner,
        Err(_) => Err(anyhow::anyhow!("JS minifier panicked (unsupported syntax)")),
    }
}

/// Parse `content` as a classic script, compress and mangle it, and print it
/// through oxc's codegen in minify mode.
fn print_js(content: &str) -> Result<String> {
    use oxc::allocator::Allocator;
    use oxc::codegen::{Codegen, CodegenOptions};
    use oxc::minifier::{CompressOptions, MangleOptions, Minifier, MinifierOptions};
    use oxc::parser::Parser;
    use oxc::span::SourceType;
    use oxc::syntax::es_target::ESTarget;

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
    // oxc keeps the shorthand flag when it mangles a `__proto__` binding,
    // and its codegen then emits the renamed binding as the shorthand key —
    // `{__proto__}` would print as `{e:e}` and change the property name.
    // Reject the shorthand on the pre-mangled AST so the caller falls back
    // to unminified source; longhand `__proto__: x` and every other
    // shorthand key print correctly.
    use oxc::ast::ast::ObjectProperty;
    use oxc::ast::visit::walk;
    use oxc::ast::Visit;
    struct ProtoShorthand(bool);
    impl<'a> Visit<'a> for ProtoShorthand {
        fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
            self.0 |= property.shorthand && property.key.is_specific_static_name("__proto__");
            walk::walk_object_property(self, property);
        }
    }
    let mut proto_shorthand = ProtoShorthand(false);
    proto_shorthand.visit_program(&parsed.program);
    anyhow::ensure!(
        !proto_shorthand.0,
        "__proto__ shorthand requires unminified output to preserve its property name"
    );
    let mut program = parsed.program;
    let minified = Minifier::new(MinifierOptions {
        compress: Some(CompressOptions {
            target: ESTarget::ES5,
            drop_debugger: true,
            drop_console: false,
        }),
        mangle: Some(MangleOptions {
            top_level: false,
            debug: false,
        }),
    })
    .build(&allocator, &mut program);
    let printed = Codegen::new()
        .with_options(CodegenOptions {
            minify: true,
            ..CodegenOptions::default()
        })
        .with_symbol_table(minified.symbol_table)
        .build(&program);
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

/// Rewrite ES2015 shorthand object properties back to `key: value` longhand.
///
/// In minify mode oxc's codegen prints `{key: value}` as `{value}` whenever
/// the property key name equals the value's identifier — including for
/// properties the source wrote in longhand — which is ES2015 syntax and a
/// parse error on the ES5 engines paste.js ships polyfills for. The rewrite is
/// semantics-preserving: the shorthand and longhand forms denote the same
/// property — with one exception.
///
/// `__proto__` is not an ordinary property name: in an object literal the
/// longhand `__proto__: <expr>` is a prototype setter, while the shorthand
/// `{__proto__}` creates an own data property holding the identifier's value.
/// Rewriting the shorthand to `__proto__:__proto__` would therefore turn an
/// own property into a prototype link — a semantic change, not a spelling
/// change. There is no longhand spelling of the own-property form, so the
/// function returns Err and the caller falls back to the unminified source.
///
/// Binding and assignment patterns get no such rewrite: destructuring is
/// already ES2015 syntax in any form, so expanding their shorthand would buy
/// nothing.
fn restore_longhand_object_properties(code: &str) -> Result<String> {
    use oxc::allocator::Allocator;
    use oxc::ast::ast::ObjectProperty;
    use oxc::ast::visit::walk;
    use oxc::ast::Visit;
    use oxc::parser::Parser;
    use oxc::span::{GetSpan, SourceType};

    struct Collector<'c> {
        code: &'c str,
        replacements: Vec<(u32, u32, String)>,
        unwritable: bool,
    }

    impl<'a> Visit<'a> for Collector<'_> {
        fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
            if it.shorthand {
                // A shorthand property's key and value are the same identifier
                // spanning the property itself, e.g. `{value}`.
                let key_span = it.key.span();
                let value_span = it.value.span();
                let key = &self.code[key_span.start as usize..key_span.end as usize];
                let value = &self.code[value_span.start as usize..value_span.end as usize];
                if key == "__proto__" {
                    self.unwritable = true;
                    return;
                }
                self.replacements
                    .push((it.span.start, it.span.end, format!("{key}:{value}")));
                return;
            }
            walk::walk_object_property(self, it);
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
        code,
        replacements: Vec::new(),
        unwritable: false,
    };
    collector.visit_program(&parsed.program);
    if collector.unwritable {
        return Err(anyhow::anyhow!(
            "shorthand __proto__ cannot be written in ES5 longhand without turning an own property into a prototype link"
        ));
    }

    let mut out = code.to_string();
    collector
        .replacements
        .sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, replacement) in collector.replacements {
        out.replace_range(start as usize..end as usize, &replacement);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_module_name_accepts_the_build_alphabet() {
        assert!(is_module_name("paste.dom"));
        assert!(is_module_name("site.cafe\u{301}"));
        assert!(is_module_name("a\u{203F}b"));
        assert!(is_module_name("site.café"));
    }

    #[test]
    fn test_is_module_name_rejects_non_names() {
        assert!(!is_module_name(""));
        assert!(!is_module_name("../etc"));
        assert!(!is_module_name("a/b"));
        assert!(!is_module_name("a b"));
        assert!(!is_module_name("a+v1"));
        // Empty dot segments would let a name reach outside the output tree.
        assert!(!is_module_name("."));
        assert!(!is_module_name(".."));
        assert!(!is_module_name(".name"));
        assert!(!is_module_name("name."));
        assert!(!is_module_name("a..b"));
        assert!(!is_module_name("a\\b"));
    }

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
        // The compressor drops the no-op statement outright, which is equally
        // safe: a removed statement cannot become a directive either.
        assert!(
            !out.contains("\"use strict\""),
            "the dead statement should be removed, not printed: {out}"
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
    fn test_minify_js_mangles_locals_inside_define() {
        // YUI-style compression: local variables are shortened while every
        // property name and string literal is preserved, so module names,
        // named exports, and dot/bracket member access all survive.
        let input = "paste.define('paste.example', ['paste.util'], function (mymodule, util) { 'use strict'; var longLocalName = 1; function privateHelper(someArgument) { return someArgument + longLocalName; } mymodule.publicMethod = function (anotherArgument) { return privateHelper(anotherArgument) + util.value; }; mymodule['namedExport'] = mymodule.publicMethod; mymodule['delete'] = privateHelper; });";
        let out = minify_js(input).expect("minification should succeed");

        for local in [
            "longLocalName",
            "privateHelper",
            "someArgument",
            "anotherArgument",
            "mymodule",
        ] {
            assert!(
                !out.contains(local),
                "local name {local} must be mangled: {out}"
            );
        }
        for kept in [
            "\"paste.example\"",
            "\"paste.util\"",
            "publicMethod",
            "namedExport",
            "delete",
            ".value",
            "\"use strict\"",
        ] {
            assert!(out.contains(kept), "{kept} must be preserved: {out}");
        }
        assert!(!out.contains("=>"), "no arrow may be introduced: {out}");
        assert!(
            !out.contains('`'),
            "no template literal may be introduced: {out}"
        );
        minify_js(&out).expect("minified output must re-minify");
    }

    #[test]
    fn test_minify_js_keeps_top_level_names() {
        // Separately built files share the global scope, so top-level
        // declarations must keep their names; only locals are mangled.
        let input = "var pasteGlobal = 1; function topLevelFn(localValue) { var inner = pasteGlobal + localValue; return inner; }";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            out.contains("pasteGlobal"),
            "top-level var must not be mangled: {out}"
        );
        assert!(
            out.contains("topLevelFn"),
            "top-level function must not be mangled: {out}"
        );
        assert!(
            !out.contains("localValue"),
            "parameter must be mangled: {out}"
        );
        assert!(!out.contains("inner"), "local var must be mangled: {out}");
    }

    #[test]
    fn test_minify_js_keeps_string_keyed_globals() {
        // Bracket-string member access is how bootstrap code installs globals;
        // the names inside the brackets are strings and must be preserved.
        let input = "(function (w) { w['paste'] = w['paste'] || {}; w['paste']['define'] = function () {}; window['PasteIOFormData'] = function () {}; })(window);";
        let out = minify_js(input).expect("minification should succeed");
        for kept in ["paste", "define", "PasteIOFormData"] {
            assert!(out.contains(kept), "{kept} must be preserved: {out}");
        }
    }

    #[test]
    fn test_minify_js_does_not_emit_es2015_object_shorthand() {
        // The ES5 target must keep `key: value` longhand: `{value}` shorthand
        // is a parse error on ES5 engines.
        let input = "var value = 1; var o = { value: value }; function f(value) { return { value: value }; }";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            !out.contains("{value}"),
            "object shorthand is ES2015: {out}"
        );
        assert!(
            !out.contains("{value,") && !out.contains(",value}"),
            "object shorthand is ES2015: {out}"
        );
        assert!(out.contains("value:"), "the key must remain: {out}");
    }

    #[test]
    fn test_minify_js_proto_shorthand_falls_back_to_source() {
        use tempfile::tempdir;

        // `{__proto__}` shorthand is an own data property; there is no ES5
        // longhand spelling for it (`__proto__: value` is a prototype
        // setter). The pass refuses to rewrite it, minify_js fails, and the
        // emitted file is the unminified source — shorthand intact.
        let source = "// @module app\nvar __proto__ = {marker: 1};\nvar object = {__proto__};\n";
        let err = minify_js(source).unwrap_err();
        assert!(err.to_string().contains("__proto__"), "{err}");

        // The escaped spelling `\u005f_proto__` decodes to `__proto__` and
        // must be rejected just the same — key.name sees the cooked name.
        let escaped = r#"var \u005f_proto__ = {marker: 1}; var object = {\u005f_proto__};"#;
        let err = minify_js(escaped).unwrap_err();
        assert!(err.to_string().contains("__proto__"), "{err}");

        // Longhand `__proto__: value` is the prototype setter, not an own
        // property shorthand — it must not be misclassified.
        minify_js("var object = {__proto__: {marker: 1}};")
            .expect("longhand __proto__ minifies normally");

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("app.js"), source).unwrap();
        let manifest = AssetGraph::new(dist_config(dir.path(), true))
            .build()
            .unwrap();
        let emitted = fs::read_to_string(
            dir.path()
                .join("dist")
                .join(manifest.assets["js/app.js"].output.trim_start_matches('/')),
        )
        .unwrap();
        assert_eq!(emitted, source, "the fallback must emit the source bytes");
        assert!(emitted.contains("{__proto__}"), "{emitted}");
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
    // Dependency ordering and backfill
    // ============================================================

    fn deps_map(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(name, deps)| {
                (
                    name.to_string(),
                    deps.iter().map(|d| d.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn test_layered_order_releases_leaves_first() {
        // b and c have no deps, so both release in layer 1 (name order); a
        // follows once c is gone.
        let order = layered_order(&deps_map(&[("a", &["c"]), ("b", &[]), ("c", &[])]));
        assert_eq!(order.sorted, vec!["b", "c", "a"]);
        assert!(order.cyclic.is_empty());
    }

    #[test]
    fn test_layered_order_diamond() {
        let order = layered_order(&deps_map(&[
            ("a", &["b", "c"]),
            ("b", &["d"]),
            ("c", &["d"]),
            ("d", &[]),
        ]));
        assert_eq!(order.sorted, vec!["d", "b", "c", "a"]);
        assert!(order.cyclic.is_empty());
    }

    #[test]
    fn test_layered_order_ignores_self_dependencies() {
        let order = layered_order(&deps_map(&[("a", &["a"]), ("b", &["a"])]));
        assert_eq!(order.sorted, vec!["a", "b"]);
        assert!(order.cyclic.is_empty());
    }

    #[test]
    fn test_layered_order_unknown_deps_layer_but_never_sort() {
        // `x` names no module: it joins the layering as a leaf so `a` still
        // sorts, but it must never appear in the output.
        let order = layered_order(&deps_map(&[("a", &["x"])]));
        assert_eq!(order.sorted, vec!["a"]);
        assert!(order.cyclic.is_empty());
    }

    #[test]
    fn test_layered_order_cycle_traps_its_dependents() {
        // a↔b is a cycle; c depends on a and can never release either. d is
        // the only node that sorts.
        let order = layered_order(&deps_map(&[
            ("a", &["b"]),
            ("b", &["a"]),
            ("c", &["a"]),
            ("d", &[]),
        ]));
        assert_eq!(order.sorted, vec!["d"]);
        assert_eq!(order.cyclic, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_backfilled_deps_declared_first_then_preorder() {
        // Declared order first (b, c), then the pre-order walk appends d.
        let deps = deps_map(&[("a", &["b", "c"]), ("b", &["d"]), ("c", &[]), ("d", &[])]);
        assert_eq!(backfilled_deps(&deps, "a"), vec!["b", "c", "d"]);
    }

    #[test]
    fn test_backfilled_deps_dedupes_and_drops_self() {
        let deps = deps_map(&[("a", &["a", "b", "b", "c"]), ("b", &["c"]), ("c", &[])]);
        assert_eq!(backfilled_deps(&deps, "a"), vec!["b", "c"]);
    }

    #[test]
    fn test_backfilled_deps_keeps_unknown_and_ends_on_cycles() {
        // Unknown dep stays in the list; the a↔b cycle terminates instead of
        // recursing forever.
        let deps = deps_map(&[("a", &["b"]), ("b", &["a", "x"])]);
        assert_eq!(backfilled_deps(&deps, "a"), vec!["b", "x"]);
        assert_eq!(backfilled_deps(&deps, "b"), vec!["a", "x"]);
    }

    #[test]
    fn test_build_js_cycle_builds_every_module_but_sorts_none() {
        use tempfile::tempdir;

        // a↔b is circular: the build must succeed, record and emit both
        // modules, and leave them out of sorted_deps — only c sorts.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("a.js"),
            "// @module a\n// @requires b\nvar a = 1;",
        )
        .unwrap();
        fs::write(
            js_dir.join("b.js"),
            "// @module b\n// @requires a\nvar b = 1;",
        )
        .unwrap();
        fs::write(js_dir.join("c.js"), "// @module c\nvar c = 1;").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        for module in ["a", "b", "c"] {
            let key = format!("js/{module}.js");
            assert!(
                manifest.assets.contains_key(&key),
                "missing asset for cyclic module {module}"
            );
            assert!(
                manifest.modules.contains_key(module),
                "missing modules entry for {module}"
            );
            let output = &manifest.assets[&key].output;
            assert!(
                dir.path()
                    .join("dist")
                    .join(output.trim_start_matches('/'))
                    .exists(),
                "missing output file {output}"
            );
        }
        let sorted: Vec<&String> = manifest.sorted_deps["js"]
            .iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(sorted, vec!["c"], "cyclic modules must not sort");
    }

    #[test]
    fn test_css_sorted_deps_follow_layered_order_not_file_order() {
        use tempfile::tempdir;

        // The file walk reads atheme.scss before zbase.scss; @requires still
        // puts base before theme.
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("atheme.scss"),
            "// @module theme\n// @requires base\n.t { color: red; }",
        )
        .unwrap();
        fs::write(
            scss_dir.join("zbase.scss"),
            "// @module base\n.b { color: blue; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        let sorted: Vec<&String> = manifest.sorted_deps["css"]
            .iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(sorted, vec!["base", "theme"]);

        let theme_key = &manifest.modules["theme"];
        assert_eq!(
            manifest.assets[theme_key].dependencies,
            vec!["base".to_string()],
            "css asset deps should be backfilled too"
        );
    }

    // ============================================================
    // Styles without @module take a path-derived module name
    // ============================================================

    #[test]
    fn test_unnamed_style_takes_path_derived_module_name() {
        use tempfile::tempdir;

        // src/scss/site/theme.scss declares nothing: it is named
        // "site.theme" like a JS file at that path would be, records the
        // module, and sorts into css sorted_deps.
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss/site");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("theme.scss"),
            "// @requires base\n.t { color: red; }",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/scss/base.scss"),
            "// @module base\n.b { color: blue; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        assert_eq!(
            manifest.modules.get("site.theme"),
            Some(&"css/site/theme.css".to_string())
        );
        assert!(manifest.assets.contains_key("css/site/theme.css"));
        let sorted: Vec<&String> = manifest.sorted_deps["css"]
            .iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(sorted, vec!["base", "site.theme"]);
        assert_eq!(
            manifest.assets["css/site/theme.css"].dependencies,
            vec!["base".to_string()]
        );
    }

    #[test]
    fn test_derived_style_name_loses_to_a_declared_name() {
        use tempfile::tempdir;

        // declared.scss declares @module theme; theme.scss derives the same
        // name — the declaration keeps it and the deriving file stays
        // unnamed but still compiles under its path key.
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("declared.scss"),
            "// @module theme\n.d { color: red; }",
        )
        .unwrap();
        fs::write(scss_dir.join("theme.scss"), ".t { color: blue; }").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        assert_eq!(
            manifest.modules.get("theme"),
            Some(&"css/declared.css".to_string()),
            "the declared owner keeps the name"
        );
        assert!(manifest.assets.contains_key("css/declared.css"));
        assert!(
            manifest.assets.contains_key("css/theme.css"),
            "the unnamed file still builds under its path key"
        );
        let names: Vec<&String> = manifest.modules.keys().collect();
        assert_eq!(
            names.iter().filter(|n| ***n == "theme").count(),
            1,
            "one owner for the name"
        );
    }

    #[test]
    fn test_invalid_derived_style_name_stays_unnamed_and_builds() {
        use tempfile::tempdir;

        // A filename that cannot be a module name (a space) warns, leaves
        // the file unnamed, and does not fail the build.
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(scss_dir.join("in valid.scss"), ".t { color: red; }").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        assert!(
            manifest.assets.contains_key("css/in valid.css"),
            "the file compiles under its path key"
        );
        assert!(
            manifest
                .modules
                .values()
                .all(|key| key != "css/in valid.css"),
            "no module name claims the invalid file"
        );
    }

    #[test]
    fn test_include_path_style_derives_a_module_name() {
        use tempfile::tempdir;

        // A style an include path provides is named from its path like a
        // source-dir style (the fallback warning fires on stderr).
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        let include_dir = dir.path().join("vendor");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::create_dir_all(&include_dir).unwrap();
        fs::write(
            scss_dir.join("base.scss"),
            "// @module base\n.b { color: blue; }",
        )
        .unwrap();
        fs::write(include_dir.join("widget.scss"), ".w { color: red; }").unwrap();

        let mut config = dist_config(dir.path(), false);
        config.include_paths = vec![include_dir];
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        assert_eq!(
            manifest.modules.get("widget"),
            Some(&"css/widget.css".to_string())
        );
        assert!(manifest.assets.contains_key("css/widget.css"));
    }

    #[test]
    fn test_include_path_js_derives_a_module_name() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let include_dir = dir.path().join("vendor");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&include_dir).unwrap();
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();
        fs::write(include_dir.join("widget.js"), "var w = 1;").unwrap();

        let mut config = dist_config(dir.path(), false);
        config.include_paths = vec![include_dir];
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        assert_eq!(
            manifest.modules.get("widget"),
            Some(&"js/widget.js".to_string())
        );
        assert!(manifest.assets.contains_key("js/widget.js"));
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
                dirs: vec![],
                js: js_dir.clone(),
                scss: dir.path().join("src/scss"),
            },
            output: crate::config::OutputPaths {
                base: dir.path().join("output"),
                js: output_js,
                css: output_css,
                images: output_images,
                layout: "flat".into(),
                build_dir_name: "_paste_build".into(),
            },
            manifest_path: dir.path().join("manifest.json"),
            include_paths: vec![],
            storage: crate::config::StorageConfig::default(),
            jam: crate::config::JamConfig {
                enabled: false,
                base_path: "/paste".into(),
                // Legacy pins — these fixtures request `+vN.0` URLs; the
                // qualified form is covered by the hash_url tests below.
                hash_urls: false,
                inline_threshold: 1024,
            },
            cdn: crate::config::CdnConfig {
                enabled: false,
                url: String::new(),
                fallback_to_local: true,
            },
            templates: None,
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
                dirs: vec![],
                js: dir.join("src/js"),
                scss: dir.join("src/scss"),
            },
            output: crate::config::OutputPaths {
                base: output.clone(),
                js: output.join("js"),
                css: output.join("css"),
                images: output.join("images"),
                layout: "flat".into(),
                build_dir_name: "_paste_build".into(),
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
                // Legacy pins — these fixtures request `+vN.0` URLs; the
                // qualified form is covered by the hash_url tests below.
                hash_urls: false,
                inline_threshold: 1024,
            },
            cdn: crate::config::CdnConfig {
                enabled: false,
                url: String::new(),
                fallback_to_local: true,
            },
            templates: None,
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
        // build must be removed — load() trusts it only while it is no older
        // than the manifest.
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

    // ============================================================
    // Output-byte hashing: a rebuild changes the fingerprint only
    // when the emitted bytes change
    // ============================================================

    /// Build twice in `dir` with `minify`, mutating `js/app.js` between the
    /// builds; returns the two manifests' app entries.
    fn rebuild_with_js(
        dir: &Path,
        minify: bool,
        first: &str,
        second: &str,
    ) -> (Manifest, Manifest) {
        let js_dir = dir.join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("app.js"), first).unwrap();
        let manifest1 = AssetGraph::new(dist_config(dir, minify)).build().unwrap();
        fs::write(js_dir.join("app.js"), second).unwrap();
        let manifest2 = AssetGraph::new(dist_config(dir, minify)).build().unwrap();
        (manifest1, manifest2)
    }

    #[test]
    fn test_js_literal_whitespace_change_rolls_the_version() {
        use tempfile::tempdir;
        for minify in [true, false] {
            let dir = tempdir().unwrap();
            let (m1, m2) = rebuild_with_js(
                dir.path(),
                minify,
                "// @module app\nwindow.label = 'a b';\n",
                "// @module app\nwindow.label = 'a  b';\n",
            );
            let e1 = &m1.assets["js/app.js"];
            let e2 = &m2.assets["js/app.js"];
            assert_ne!(e1.hash, e2.hash, "minify={minify}: hash must change");
            assert_eq!(e1.version + 1, e2.version, "minify={minify}");
            assert_ne!(e1.output, e2.output, "minify={minify}");
            let out2 = fs::read_to_string(
                dir.path()
                    .join("dist")
                    .join(e2.output.trim_start_matches('/')),
            )
            .unwrap();
            assert!(
                out2.contains("a  b"),
                "minify={minify}: output must carry the change: {out2}"
            );
        }
    }

    #[test]
    fn test_js_asi_change_rolls_the_version() {
        use tempfile::tempdir;
        for minify in [true, false] {
            let dir = tempdir().unwrap();
            let (m1, m2) = rebuild_with_js(
                dir.path(),
                minify,
                "// @module app\nfunction f() { return\n1; } window.f = f;\n",
                "// @module app\nfunction f() { return 1; } window.f = f;\n",
            );
            assert_ne!(
                m1.assets["js/app.js"].hash, m2.assets["js/app.js"].hash,
                "minify={minify}: ASI changes the emitted bytes"
            );
        }
    }

    #[test]
    fn test_js_regex_and_nested_template_changes_roll_the_version() {
        use tempfile::tempdir;
        for minify in [true, false] {
            let dir = tempdir().unwrap();
            let (m1, m2) = rebuild_with_js(
                dir.path(),
                minify,
                "// @module app\nvar re = /a b/; var t = `x${1}`;\n",
                "// @module app\nvar re = /a  b/; var t = `x${2}`;\n",
            );
            assert_ne!(
                m1.assets["js/app.js"].hash, m2.assets["js/app.js"].hash,
                "minify={minify}: regex/template contents are output bytes"
            );
        }
    }

    #[test]
    fn test_comment_only_change_hashes_by_emitted_bytes() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let (minified1, minified2) = rebuild_with_js(
            dir.path(),
            true,
            "// @module app\n// first comment\nvar app = 1;\n",
            "// @module app\n// second comment\nvar app = 1;\n",
        );
        assert_eq!(
            minified1.assets["js/app.js"].hash, minified2.assets["js/app.js"].hash,
            "comments do not reach the minified output, so the hash must not move"
        );
        assert_eq!(
            minified1.assets["js/app.js"].version,
            minified2.assets["js/app.js"].version
        );

        // In verbatim mode comments ARE the output: the same edit is a change.
        let dir2 = tempdir().unwrap();
        let (verbatim1, verbatim2) = rebuild_with_js(
            dir2.path(),
            false,
            "// @module app\n// first comment\nvar app = 1;\n",
            "// @module app\n// second comment\nvar app = 1;\n",
        );
        assert_ne!(
            verbatim1.assets["js/app.js"].hash, verbatim2.assets["js/app.js"].hash,
            "verbatim output carries comments, so the hash must move"
        );
    }

    #[test]
    fn test_css_literal_whitespace_change_rolls_the_version() {
        use tempfile::tempdir;
        for minify in [true, false] {
            let dir = tempdir().unwrap();
            let scss_dir = dir.path().join("src/scss");
            fs::create_dir_all(&scss_dir).unwrap();
            fs::write(
                scss_dir.join("app.scss"),
                "// @module app\n.x::before { content: 'a b'; }\n",
            )
            .unwrap();
            let m1 = AssetGraph::new(dist_config(dir.path(), minify))
                .build()
                .unwrap();
            fs::write(
                scss_dir.join("app.scss"),
                "// @module app\n.x::before { content: 'a  b'; }\n",
            )
            .unwrap();
            let m2 = AssetGraph::new(dist_config(dir.path(), minify))
                .build()
                .unwrap();
            let e1 = &m1.assets["css/app.css"];
            let e2 = &m2.assets["css/app.css"];
            assert_ne!(e1.hash, e2.hash, "minify={minify}: hash must change");
            assert_eq!(e1.version + 1, e2.version, "minify={minify}");
            let out2 = fs::read_to_string(
                dir.path()
                    .join("dist")
                    .join(e2.output.trim_start_matches('/')),
            )
            .unwrap();
            assert!(
                out2.contains("a  b"),
                "minify={minify}: output must carry the change: {out2}"
            );
        }
    }

    // ============================================================
    // Module name validation
    // ============================================================

    #[test]
    fn test_build_rejects_an_invalid_js_module_name() {
        use tempfile::tempdir;
        for declared in ["/tmp", "a..b", ".name", ".."] {
            let dir = tempdir().unwrap();
            let js_dir = dir.path().join("src/js");
            fs::create_dir_all(&js_dir).unwrap();
            fs::write(
                js_dir.join("evil.js"),
                format!("// @module {declared}\nvar x = 1;\n"),
            )
            .unwrap();

            let mut graph = AssetGraph::new(dist_config(dir.path(), false));
            assert!(
                graph.build().is_err(),
                "@module {declared:?} must fail the build"
            );
            // Nothing may be written outside the output tree.
            assert!(
                !dir.path().join("tmp").exists(),
                "@module {declared:?} must not write outside dist"
            );
        }
    }

    #[test]
    fn test_build_rejects_an_invalid_js_requires_name() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("app.js"),
            "// @module app\n// @requires ../escape\nvar app = 1;\n",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        assert!(graph.build().is_err());
    }

    #[test]
    fn test_build_rejects_an_invalid_scss_module_name() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("evil.scss"),
            "// @module ../escape\n.e { color: red; }\n",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        assert!(graph.build().is_err());
        assert!(!dir.path().join("escape").exists());
    }

    #[test]
    fn test_nested_source_paths_use_forward_slashes_in_keys() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js/nested");
        let scss_dir = dir.path().join("src/scss/nested");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(js_dir.join("deep.js"), "var deep = 1;").unwrap();
        fs::write(scss_dir.join("deep.scss"), ".deep { color: red; }").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        assert!(manifest.assets.contains_key("js/nested/deep.js"));
        assert!(manifest.assets.contains_key("css/nested/deep.css"));
        for key in manifest.assets.keys() {
            assert!(!key.contains('\\'), "asset key has a backslash: {key}");
        }
        for name in manifest.modules.keys() {
            assert!(!name.contains('\\'), "module name has a backslash: {name}");
        }
    }

    #[test]
    fn test_is_source_change_filters_watch_events() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let mut config = dist_config(dir.path(), false);
        config.templates = Some(crate::config::TemplatesConfig {
            source: dir.path().join("templates"),
            output: dir.path().join("dist"),
        });

        // Source files count.
        assert!(is_source_change(&config, &dir.path().join("src/js/app.js")));
        assert!(is_source_change(
            &config,
            &dir.path().join("src/scss/site.scss")
        ));
        assert!(is_source_change(
            &config,
            &dir.path().join("templates/index.html")
        ));
        // Nested template sources count too — creation and deletion events
        // report the same path shape.
        assert!(is_source_change(
            &config,
            &dir.path().join("templates/nested/page.html")
        ));

        // Generated output never counts — including a tree nested under a
        // watched source directory.
        assert!(!is_source_change(
            &config,
            &dir.path().join("dist/index.html")
        ));
        assert!(!is_source_change(
            &config,
            &dir.path().join("dist/js/app.v1.js")
        ));
        assert!(!is_source_change(
            &config,
            &dir.path().join("dist/css/site.css")
        ));

        let mut nested = config.clone();
        nested.output.base = dir.path().join("src/dist");
        assert!(!is_source_change(
            &nested,
            &dir.path().join("src/dist/out.js")
        ));

        // HTML outside the template source is not a source either.
        assert!(!is_source_change(
            &config,
            &dir.path().join("src/js/readme.html")
        ));
        // And without templates configured, no HTML counts.
        config.templates = None;
        assert!(!is_source_change(
            &config,
            &dir.path().join("templates/index.html")
        ));
    }

    /// A config whose only source roots are `source.dirs` — the legacy
    /// js/scss roots are unset so `dirs` alone drives discovery.
    fn dirs_config(dir: &Path, roots: &[&str], minify: bool) -> Config {
        let mut config = dist_config(dir, minify);
        config.source.dirs = roots.iter().map(|root| dir.join(root)).collect();
        config.source.js = PathBuf::new();
        config.source.scss = PathBuf::new();
        config
    }

    /// `dirs_config` in beside-source layout; output.base stays inside
    /// base_dir as the layout requires.
    fn beside_source_config(dir: &Path, roots: &[&str], minify: bool) -> Config {
        let mut config = dirs_config(dir, roots, minify);
        config.output.layout = "beside-source".into();
        config
    }

    /// The directory a source-dir file compiles into under beside-source:
    /// `<file dir>/<build-dir-name>` beneath base_dir.
    fn build_dir_of(dir: &Path, rel: &str) -> PathBuf {
        let parent = Path::new(rel).parent().unwrap();
        dir.join(parent).join("_paste_build")
    }

    #[test]
    fn test_source_dirs_classifies_by_extension_not_layout() {
        // One root holds JS and styles together at arbitrary depth — the
        // folder layout means nothing to discovery, only the extension does.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let web = dir.path().join("web");
        fs::create_dir_all(web.join("deep/nested")).unwrap();
        fs::write(web.join("app.js"), "var app = 1;").unwrap();
        fs::write(web.join("deep/nested/extra.js"), "var extra = 1;").unwrap();
        fs::write(web.join("deep/site.scss"), ".site { color: red; }").unwrap();
        fs::write(web.join("plain.css"), "a  {  color :  red ;  }").unwrap();
        fs::write(web.join("deep/_reset.scss"), "* { margin: 0; }").unwrap();

        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["web"], true));
        let manifest = graph.build().unwrap();

        assert!(manifest.assets.contains_key("js/app.js"));
        assert!(manifest.assets.contains_key("js/deep/nested/extra.js"));
        assert!(manifest.assets.contains_key("css/deep/site.css"));
        assert!(manifest.assets.contains_key("css/plain.css"));
        // An _-prefixed scss partial is never an asset; a plain .css is.
        assert!(!manifest.assets.contains_key("css/deep/_reset.css"));

        // The plain .css ran the grass → lightningcss pipeline: the output
        // is minified, not a byte copy.
        let css_entry = &manifest.assets["css/plain.css"];
        let compiled = dir
            .path()
            .join("dist")
            .join(css_entry.output.trim_start_matches('/'));
        let body = fs::read_to_string(&compiled).unwrap();
        assert!(
            body.contains("color:red") && !body.contains("color :"),
            "plain css should be minified: {body}"
        );
    }

    #[test]
    fn test_source_dirs_first_root_wins_a_duplicate_module() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("one")).unwrap();
        fs::create_dir_all(dir.path().join("two")).unwrap();
        fs::write(
            dir.path().join("one/shared.js"),
            "// @module shared\nvar shared = 'one';",
        )
        .unwrap();
        fs::write(
            dir.path().join("two/shared.js"),
            "// @module shared\nvar shared = 'two';",
        )
        .unwrap();
        fs::write(
            dir.path().join("two/only.js"),
            "// @module only\nvar only = 1;",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["one", "two"], false));
        let manifest = graph.build().unwrap();

        // Both roots were walked.
        assert!(manifest.assets.contains_key("js/shared.js"));
        assert!(manifest.assets.contains_key("js/only.js"));

        // The first root claimed the shared name: the second file is not
        // its source and never reaches the manifest.
        let entry = &manifest.assets["js/shared.js"];
        assert!(
            entry.source.contains("one"),
            "duplicate should keep the first root's file: {}",
            entry.source
        );
        let compiled = dir
            .path()
            .join("dist")
            .join(entry.output.trim_start_matches('/'));
        assert!(fs::read_to_string(&compiled).unwrap().contains("'one'"));
    }

    #[test]
    fn test_legacy_source_roots_hocon_and_json_match() {
        // A config with only the legacy source.js/source.scss keys — in
        // either syntax — produces the same module names and asset keys.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("assets/js")).unwrap();
        fs::create_dir_all(dir.path().join("assets/scss")).unwrap();
        fs::write(dir.path().join("assets/js/app.js"), "var app = 1;").unwrap();
        fs::write(
            dir.path().join("assets/scss/site.scss"),
            "// @module site\n.site { color: red; }",
        )
        .unwrap();

        fs::write(
            dir.path().join("paste.conf"),
            r#"source.js = "assets/js"
source.scss = "assets/scss"
output.base = "public"
output.js = "public/js"
output.css = "public/css"
output.images = "public/images"
manifest = "public/manifest.json"
storage.backend = "local"
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("paste.json"),
            r#"{
                "source": {"js": "assets/js", "scss": "assets/scss"},
                "output": {
                    "base": "public",
                    "js": "public/js",
                    "css": "public/css",
                    "images": "public/images"
                },
                "manifest_path": "public/manifest.json",
                "storage": {"backend": "local", "keep_versions": 10}
            }"#,
        )
        .unwrap();

        let hocon = Config::load_with_base_dir(Path::new("paste.conf"), dir.path()).unwrap();
        let json = Config::load_with_base_dir(Path::new("paste.json"), dir.path()).unwrap();

        // The legacy roots feed discovery in their old order: js, then scss.
        assert_eq!(
            hocon.source_dirs(),
            vec![dir.path().join("assets/js"), dir.path().join("assets/scss")]
        );

        let manifest_h = AssetGraph::new(hocon).build().unwrap();
        let manifest_j = AssetGraph::new(json).build().unwrap();
        let hocon_keys: BTreeMap<_, _> = manifest_h.assets.keys().map(|k| (k, ())).collect();
        let json_keys: BTreeMap<_, _> = manifest_j.assets.keys().map(|k| (k, ())).collect();
        assert_eq!(
            hocon_keys, json_keys,
            "HOCON and JSON disagree on asset keys"
        );
        let hocon_modules: BTreeMap<_, _> = manifest_h.modules.keys().map(|k| (k, ())).collect();
        let json_modules: BTreeMap<_, _> = manifest_j.modules.keys().map(|k| (k, ())).collect();
        assert_eq!(hocon_modules, json_modules);
        assert!(manifest_h.assets.contains_key("js/app.js"));
        assert!(manifest_h.assets.contains_key("css/site.css"));
        assert!(manifest_h.modules.contains_key("app"));
        assert!(manifest_h.modules.contains_key("site"));
    }

    #[test]
    fn test_beside_source_dist_writes_next_to_the_source() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.storage.backend = "dist".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        // The file lands beside its source, in the build directory, and the
        // manifest records it relative to base_dir with forward slashes.
        let entry = &manifest.assets["js/app.js"];
        assert!(
            entry.output.starts_with("/src/_paste_build/app.")
                && entry.output.ends_with(".v1.min.js"),
            "unexpected beside-source output: {}",
            entry.output
        );
        let compiled = dir.path().join(entry.output.trim_start_matches('/'));
        assert!(compiled.exists(), "missing {}", compiled.display());
        let body = fs::read_to_string(&compiled).unwrap();

        // The runtime's output root in beside-source mode is the project
        // root: the same resolver serves those exact bytes.
        let dirs = OutputDirs::under(dir.path());
        let r = crate::jam::combo(&manifest, &dirs, "/paste", "/paste/app.js", None);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, body);

        // The static rollup still builds, under the type output dir.
        assert!(dir.path().join("dist/js/assetgraph.js").exists());

        // An unchanged rebuild leaves the committed manifest byte-identical.
        let manifest_path = dir.path().join("dist/manifest.json");
        let before = fs::read(&manifest_path).unwrap();
        graph.build().unwrap();
        assert_eq!(before, fs::read(&manifest_path).unwrap());

        // A content change bumps the version; the dist backend keeps v1 in
        // the same build directory.
        fs::write(dir.path().join("src/app.js"), "var app = 2;").unwrap();
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert!(
            entry.output.ends_with(".v2.min.js"),
            "expected v2: {}",
            entry.output
        );
        assert!(compiled.exists(), "dist must retain the v1 file");
        assert!(dir
            .path()
            .join(entry.output.trim_start_matches('/'))
            .exists());
    }

    #[test]
    fn test_beside_source_local_keeps_only_the_current_file() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.storage.backend = "local".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let first = dir
            .path()
            .join(manifest.assets["js/app.js"].output.trim_start_matches('/'));
        assert!(first.exists());

        fs::write(dir.path().join("src/app.js"), "var app = 2;").unwrap();
        let manifest = graph.build().unwrap();
        let current = dir
            .path()
            .join(manifest.assets["js/app.js"].output.trim_start_matches('/'));
        assert!(current.exists());
        assert!(!first.exists(), "local must remove the superseded file");

        let build_dir = build_dir_of(dir.path(), "src/app.js");
        let count = fs::read_dir(&build_dir).unwrap().count();
        assert_eq!(count, 1, "only the current file may remain");
    }

    #[test]
    fn test_beside_source_include_path_stays_under_output_base() {
        // An include-path file is another tree's source — it is never
        // written there. It keeps flat placement under output.base, and its
        // manifest output is still relative to base_dir.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let external = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();
        fs::write(
            external.path().join("vendor.js"),
            "// @module vendor\nvar vendor = 1;",
        )
        .unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.include_paths = vec![external.path().to_path_buf()];
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        let entry = &manifest.assets["js/vendor.js"];
        assert!(
            entry.output.starts_with("/dist/js/vendor.") && entry.output.ends_with(".min.js"),
            "include-path output should stay flat under output.base: {}",
            entry.output
        );
        let compiled = dir.path().join(entry.output.trim_start_matches('/'));
        assert!(compiled.exists(), "missing {}", compiled.display());
        assert!(
            !external.path().join("_paste_build").exists(),
            "nothing may be written into the include path's tree"
        );

        let dirs = OutputDirs::under(dir.path());
        let r = crate::jam::combo(&manifest, &dirs, "/paste", "/paste/vendor.js", None);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, fs::read_to_string(&compiled).unwrap());
    }

    #[test]
    fn test_custom_build_dir_name_is_honored_and_never_scanned() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/compiled")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();
        // A source inside a build-dir-name directory is generated output's
        // place — it is never a module even before the first build.
        fs::write(
            dir.path().join("src/compiled/ignored.js"),
            "// @module ignored\nvar ignored = 1;",
        )
        .unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.output.build_dir_name = "compiled".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        let entry = &manifest.assets["js/app.js"];
        assert!(
            entry.output.starts_with("/src/compiled/app."),
            "custom build dir not used: {}",
            entry.output
        );
        assert!(dir
            .path()
            .join(entry.output.trim_start_matches('/'))
            .exists());
        assert!(!manifest.modules.contains_key("ignored"));
        assert!(!manifest.assets.contains_key("js/compiled/ignored.js"));
    }

    #[test]
    fn test_config_validation_rejects_bad_layout_and_build_dir() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();

        let mut config = dist_config(dir.path(), true);
        config.output.layout = "sideways".into();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("output.layout"));

        for bad in ["a/b", "a\\b", "..", ""] {
            let mut config = dist_config(dir.path(), true);
            config.output.build_dir_name = bad.into();
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("build-dir-name"),
                "{bad:?} should be rejected"
            );
        }

        let mut config = beside_source_config(dir.path(), &["src"], true);
        config.output.base = outside.path().join("out");
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("base_dir"));
    }

    #[test]
    fn test_pruning_a_stem_never_touches_its_longer_sibling() {
        // `site` and `site.theme` compile into the same output directory;
        // rebuilding `site` must not count or delete `site.theme`'s file.
        for backend in ["local", "dist"] {
            use tempfile::tempdir;
            let dir = tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src")).unwrap();
            fs::write(dir.path().join("src/site.js"), "var site = 1;").unwrap();
            fs::write(dir.path().join("src/site.theme.js"), "var theme = 1;").unwrap();

            let mut config = dirs_config(dir.path(), &["src"], false);
            config.storage.backend = backend.into();
            let mut graph = AssetGraph::new(config);
            let manifest = graph.build().unwrap();
            // Flat layout: the manifest output joins onto output.base.
            let theme = dir.path().join(
                manifest.assets["js/site/theme.js"]
                    .output
                    .trim_start_matches('/')
                    .replacen("js/", "dist/js/", 1),
            );
            assert!(theme.exists(), "{backend}: theme output missing");

            fs::write(dir.path().join("src/site.js"), "var site = 2;").unwrap();
            let manifest = graph.build().unwrap();
            let site = dir.path().join(
                manifest.assets["js/site.js"]
                    .output
                    .trim_start_matches('/')
                    .replacen("js/", "dist/js/", 1),
            );
            assert!(site.exists(), "{backend}: new site output missing");
            assert!(
                theme.exists(),
                "{backend}: rebuilding site deleted site.theme's file"
            );
        }
    }

    #[test]
    fn test_dist_prune_keeps_every_version_the_manifest_references() {
        // The manifest retains keep_versions history entries plus the
        // current file — every one of their pinned URLs must still serve.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 0;").unwrap();

        let mut config = dirs_config(dir.path(), &["src"], false);
        config.storage.backend = "dist".into();
        config.storage.keep_versions = 2;
        let mut graph = AssetGraph::new(config.clone());
        graph.build().unwrap();
        for i in 1..=4 {
            fs::write(dir.path().join("src/app.js"), format!("var app = {i};")).unwrap();
            graph.build().unwrap();
        }

        // Five versions built; the manifest references current plus the
        // two retained history entries — all must exist on disk.
        let manifest = Manifest::load(&config.manifest_path).unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 5);
        assert_eq!(entry.history.len(), 2);
        let referenced: Vec<&str> = std::iter::once(entry.output.as_str())
            .chain(entry.history.iter().map(|h| h.output.as_str()))
            .collect();
        for output in &referenced {
            assert!(
                dir.path()
                    .join("dist")
                    .join(output.trim_start_matches('/'))
                    .exists(),
                "referenced output missing: {output}"
            );
        }

        // And exactly those files remain — the pruned older versions are gone.
        let on_disk: HashSet<String> = fs::read_dir(dir.path().join("dist/js"))
            .unwrap()
            .filter_map(|e| {
                let name = e.unwrap().file_name().to_string_lossy().to_string();
                name.starts_with("app.").then(|| format!("/js/{name}"))
            })
            .collect();
        let expected: HashSet<String> = referenced.iter().map(|o| o.to_string()).collect();
        assert_eq!(on_disk, expected);

        // The oldest retained version still serves its pinned URL.
        let oldest = entry.history.iter().min_by_key(|h| h.version).unwrap();
        let stamp = crate::manifest::stamp_epoch(&oldest.timestamp).unwrap();
        let dirs = config.output_dirs();
        let r = crate::jam::combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{stamp}/app+v{}.0.js", oldest.version),
            None,
        );
        assert_eq!(r.status, 200, "pinned v{} must serve", oldest.version);
        let oldest_file = dir
            .path()
            .join("dist")
            .join(oldest.output.trim_start_matches('/'));
        assert_eq!(r.body, fs::read_to_string(&oldest_file).unwrap());
    }

    // ============================================================
    // Output repair: an existing file is not proof of correct bytes
    // ============================================================

    #[test]
    fn test_rebuild_repairs_a_corrupted_output_without_bumping_the_version() {
        // A tampered or truncated output used to survive every rebuild: the
        // write was skipped whenever the file existed. The build must now
        // restore the bytes an unchanged source produces, without rolling
        // the version its URLs are pinned to.
        for backend in ["local", "dist"] {
            use tempfile::tempdir;
            let dir = tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src")).unwrap();
            fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();
            fs::write(dir.path().join("src/site.scss"), ".site { color: red; }").unwrap();

            let mut config = dirs_config(dir.path(), &["src"], false);
            config.storage.backend = backend.into();
            let mut graph = AssetGraph::new(config);
            let manifest = graph.build().unwrap();

            for (key, prefix) in [("js/app.js", "dist"), ("css/site.css", "dist")] {
                let entry = &manifest.assets[key];
                let file = dir
                    .path()
                    .join(prefix)
                    .join(entry.output.trim_start_matches('/'));
                let expected = fs::read(&file).unwrap();
                fs::write(&file, b"corrupted").unwrap();

                let rebuilt = graph.build().unwrap();
                let entry = &rebuilt.assets[key];
                assert_eq!(
                    entry.version, 1,
                    "{backend}: repairing {key} must not roll the version"
                );
                assert_eq!(
                    fs::read(&file).unwrap(),
                    expected,
                    "{backend}: corrupted {key} output must be restored"
                );
            }
        }
    }

    #[test]
    fn test_build_fails_on_a_corrupt_manifest_and_preserves_it() {
        // A manifest that cannot be loaded used to be silently replaced by a
        // fresh one, discarding every version the deployable URLs name. The
        // build must fail and leave the file untouched.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let config = dirs_config(dir.path(), &["src"], false);
        let mut graph = AssetGraph::new(config.clone());
        graph.build().unwrap();

        let corrupt = b"{ not json";
        fs::write(&config.manifest_path, corrupt).unwrap();
        let before = fs::read(&config.manifest_path).unwrap();

        assert!(
            graph.build().is_err(),
            "a corrupt manifest must fail the build"
        );
        assert_eq!(
            fs::read(&config.manifest_path).unwrap(),
            before,
            "the corrupt manifest must be preserved, not reset"
        );
    }

    #[test]
    fn test_layout_change_relocates_output_without_bumping_the_version() {
        // Switching layouts or renaming the build dir moves where the
        // current version resolves: the manifest must record the new prefix
        // and the build must emit the file there, while the version — and
        // every URL already issued for it — stays put.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let config = dirs_config(dir.path(), &["src"], false);
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let output = &manifest.assets["js/app.js"].output;
        assert!(
            output.starts_with("/js/app.") && output.ends_with(".v1.min.js"),
            "flat output: {output}"
        );

        // flat -> beside-source: the file lands in the build dir beside its
        // source, at the same version.
        let config = beside_source_config(dir.path(), &["src"], false);
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 1, "a layout change keeps the version");
        assert!(
            entry.output.starts_with("/src/_paste_build/app.")
                && entry.output.ends_with(".v1.min.js"),
            "beside-source output: {}",
            entry.output
        );
        assert!(
            dir.path()
                .join(entry.output.trim_start_matches('/'))
                .exists(),
            "the current output must exist at the new path"
        );

        // Renaming the build dir relocates the same version again.
        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.output.build_dir_name = "compiled".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 1, "a build-dir rename keeps the version");
        assert!(
            entry.output.starts_with("/src/compiled/app."),
            "custom build-dir output: {}",
            entry.output
        );
        assert!(
            dir.path()
                .join(entry.output.trim_start_matches('/'))
                .exists(),
            "the current output must exist under the renamed build dir"
        );
    }

    #[test]
    fn test_reducing_keep_versions_trims_history_on_an_unchanged_rebuild() {
        // Retention lives in the manifest: lowering keep_versions must drain
        // recorded history on the next build — even one whose content did
        // not change — and pruning must remove exactly the versions no
        // longer referenced while every retained file survives.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();

        let mut config = dirs_config(dir.path(), &["src"], false);
        config.storage.keep_versions = 3;
        let mut graph = AssetGraph::new(config.clone());
        for i in 0..4 {
            fs::write(dir.path().join("src/app.js"), format!("var app = {i};")).unwrap();
            graph.build().unwrap();
        }
        let manifest = Manifest::load(&config.manifest_path).unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 4);
        assert_eq!(entry.history.len(), 3);
        let evicted: Vec<String> = entry
            .history
            .iter()
            .take(2)
            .map(|h| h.output.clone())
            .collect();

        // An unchanged rebuild under a smaller bound trims the recorded
        // history and the files it no longer names.
        config.storage.keep_versions = 1;
        let mut graph = AssetGraph::new(config.clone());
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 4, "unchanged content keeps the version");
        assert_eq!(entry.history.len(), 1, "history honors the new bound");
        for output in std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
        {
            let file = dir.path().join("dist").join(output.trim_start_matches('/'));
            assert!(
                file.exists(),
                "a referenced version must exist: {}",
                file.display()
            );
        }
        for output in &evicted {
            assert!(
                !dir.path()
                    .join("dist")
                    .join(output.trim_start_matches('/'))
                    .exists(),
                "an evicted version must be pruned: {output}"
            );
        }
    }

    #[test]
    fn test_local_manifest_records_no_version_history() {
        // Local keeps only the current file on disk, so its manifest keeps
        // no retained history either — an entry it could never resolve to a
        // file would be a dangling pin.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();

        let mut config = dirs_config(dir.path(), &["src"], false);
        config.storage.backend = "local".into();
        let mut graph = AssetGraph::new(config);
        for i in 0..3 {
            fs::write(dir.path().join("src/app.js"), format!("var app = {i};")).unwrap();
            let manifest = graph.build().unwrap();
            let entry = &manifest.assets["js/app.js"];
            assert_eq!(entry.version, i + 1);
            assert!(
                entry.history.is_empty(),
                "local records no prunable history"
            );
        }
    }

    /// Answer a combo request against a built graph — tests read `status`.
    fn serve(manifest: &Manifest, graph: &AssetGraph, path: &str) -> u16 {
        crate::jam::combo(manifest, &graph.output_dirs, "/paste", path, None).status
    }

    /// The stamp segment for a pinned URL naming `entry`'s own stamp.
    fn pin(manifest: &Manifest, key: &str, module: &str, version: u32, ext: &str) -> String {
        let entry = &manifest.assets[key];
        let stamp = crate::manifest::stamp_epoch(&entry.timestamp).unwrap();
        format!("/paste/{stamp}/{module}+v{version}.0.min.{ext}")
    }

    /// The stamp segment for a history pin — the retained version's own stamp.
    fn history_pin(
        manifest: &Manifest,
        key: &str,
        module: &str,
        history_index: usize,
        ext: &str,
    ) -> String {
        let history = &manifest.assets[key].history[history_index];
        let stamp = crate::manifest::stamp_epoch(&history.timestamp).unwrap();
        format!("/paste/{stamp}/{module}+v{}.0.min.{ext}", history.version)
    }

    #[test]
    fn test_dist_deleted_source_retires_but_serves_the_exact_pin() {
        // A distribution build keeps history on source deletion — the old
        // pinned URL still serves bytes while fresh lookups see only the
        // active set.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("app.js"), "// @module app\nvar app = 'A';").unwrap();
        fs::write(js_dir.join("keep.js"), "// @module keep\nvar keep = 'K';").unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = true;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest.asset_key_for_module("app", "js").as_deref(),
            Some("js/app.js")
        );
        let pinned = pin(&manifest, "js/app.js", "app", 1, "js");
        assert_eq!(serve(&manifest, &graph, &pinned), 200);
        assert_eq!(serve(&manifest, &graph, "/paste/app.min.js"), 200);

        fs::remove_file(js_dir.join("app.js")).unwrap();
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert!(
            entry.retired,
            "a deleted source retires, it does not vanish"
        );
        assert!(
            manifest.asset_key_for_module("app", "js").is_none(),
            "the active-only lookup filters a retired alias"
        );
        assert_eq!(
            manifest.modules.get("app").map(String::as_str),
            Some("js/app.js"),
            "the module map may keep a retired alias"
        );

        // Fresh requests fail; the exact pin still serves the same bytes.
        assert_eq!(serve(&manifest, &graph, "/paste/app.min.js"), 404);
        let response = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &pinned, None);
        assert_eq!(response.status, 200);
        assert!(
            response.body.contains("A"),
            "the pinned URL serves the retired bytes: {}",
            response.body
        );

        // Render expansion refuses the retired module.
        let opts = crate::jam::RenderOptions {
            cdn_base: String::new(),
            base_path: "/paste".to_string(),
            inline_threshold: 1024,
            url_component_limit: None,
        };
        assert!(
            crate::jam::render_all(
                &manifest,
                &graph.output_dirs,
                &opts,
                &[
                    crate::jam::Event::Jam(crate::jam::Jam {
                        js: vec!["app".to_string()],
                        ..Default::default()
                    }),
                    crate::jam::Event::Dump(crate::jam::DumpScope::Js),
                ],
            )
            .is_err(),
            "a retired module cannot enter a new page"
        );

        // The packages fingerprint omits retired entries.
        let mut unretired = manifest.clone();
        unretired.assets.get_mut("js/app.js").unwrap().retired = false;
        assert_ne!(
            crate::jam::packages_segment(&manifest),
            crate::jam::packages_segment(&unretired),
            "a retired entry must not change the loader fingerprint"
        );

        // And the static rollup carries only the remaining active modules.
        let rollup = fs::read_to_string(dir.path().join("dist/js/assetgraph.js")).unwrap();
        assert!(rollup.contains("keep"), "rollup keeps active modules");
        assert!(!rollup.contains("'A'"), "rollup drops the retired module");
    }

    #[test]
    fn test_dist_readded_source_continues_the_version_sequence() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = js_dir.join("app.js");
        fs::write(&source, "// @module app\nvar app = 'A';").unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = true;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let v1_pin = pin(&manifest, "js/app.js", "app", 1, "js");
        let v1_body =
            crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v1_pin, None).body;

        fs::remove_file(&source).unwrap();
        let manifest = graph.build().unwrap();
        assert!(manifest.assets["js/app.js"].retired);

        // Re-added at the same key: the flag clears and the sequence
        // continues rather than restarting at v1.
        fs::write(&source, "// @module app\nvar app = 'B';").unwrap();
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert!(!entry.retired);
        assert_eq!(entry.version, 2);
        assert_eq!(entry.history.len(), 1);
        assert_eq!(entry.history[0].version, 1);
        assert_eq!(serve(&manifest, &graph, "/paste/app.min.js"), 200);
        // The original pin still resolves the original bytes.
        let response = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v1_pin, None);
        assert_eq!(response.status, 200);
        assert_eq!(response.body, v1_body);
    }

    #[test]
    fn test_local_deleted_source_no_longer_resolves() {
        // Local has no history to protect — a removed source is gone
        // entirely, pin or no pin.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        let source = dir.path().join("src/app.js");
        fs::write(&source, "// @module app\nvar app = 1;").unwrap();

        let mut config = dirs_config(dir.path(), &["src"], true);
        config.storage.backend = "local".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let pinned = pin(&manifest, "js/app.js", "app", 1, "js");
        assert_eq!(serve(&manifest, &graph, &pinned), 200);

        fs::remove_file(&source).unwrap();
        let manifest = graph.build().unwrap();
        assert!(!manifest.assets.contains_key("js/app.js"));
        assert!(!manifest.modules.contains_key("app"));
        assert_eq!(serve(&manifest, &graph, "/paste/app.min.js"), 404);
        assert_eq!(serve(&manifest, &graph, &pinned), 404);
    }

    #[test]
    fn test_js_dependency_only_edit_versions_identical_output() {
        // A requires-line compiles to the same bytes, but the dependency
        // metadata is semantic: the version rolls and both pins resolve.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("a.js"), "// @module lib.a\nvar libA = 1;").unwrap();
        fs::write(js_dir.join("c.js"), "// @module lib.c\nvar libC = 1;").unwrap();
        let b_src = js_dir.join("b.js");
        fs::write(
            &b_src,
            "// @module lib.b\n// @requires lib.a\nvar libB = libA;",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = true;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        // JS asset keys follow the declared module name, not the file path.
        let entry = &manifest.assets["js/lib/b.js"];
        assert_eq!(entry.version, 1);
        assert_eq!(entry.dependencies, vec!["lib.a".to_string()]);
        let same_hash = entry.hash.clone();
        let v1_pin = pin(&manifest, "js/lib/b.js", "lib.b", 1, "js");

        // Swap the declared dependency — the minified bytes are unchanged.
        fs::write(
            &b_src,
            "// @module lib.b\n// @requires lib.c\nvar libB = libA;",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/lib/b.js"];
        assert_eq!(entry.hash, same_hash, "the compiled bytes are identical");
        assert_eq!(entry.version, 2, "a dependency change is a new version");
        assert_eq!(entry.dependencies, vec!["lib.c".to_string()]);
        assert_eq!(entry.history.len(), 1);
        assert_eq!(serve(&manifest, &graph, &v1_pin), 200);
        let v2_pin = pin(&manifest, "js/lib/b.js", "lib.b", 2, "js");
        assert_eq!(serve(&manifest, &graph, &v2_pin), 200);

        // A comment that compiles away entirely bumps nothing.
        fs::write(
            js_dir.join("a.js"),
            "// @module lib.a\n// a discarded comment\nvar libA = 1;",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest.assets["js/lib/a.js"].version, 1,
            "an edit that compiles to the same bytes keeps the version"
        );
    }

    #[test]
    fn test_css_deps_expand_like_js_and_survive_renames() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("c.scss"),
            "// @module chain.c\n.c { color: green; }",
        )
        .unwrap();
        fs::write(
            scss_dir.join("b.scss"),
            "// @module chain.b\n// @requires chain.c\n.b { color: blue; }",
        )
        .unwrap();
        fs::write(
            scss_dir.join("a.scss"),
            "// @module chain.a\n// @requires chain.b\n.a { color: red; }",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = true;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        // Stored dependencies are the transitive backfilled set, as for JS.
        assert_eq!(
            manifest.assets["css/a.css"].dependencies,
            vec!["chain.b".to_string(), "chain.c".to_string()],
            "expanded transitive deps are stored"
        );
        assert_eq!(manifest.assets["css/a.css"].version, 1);

        // A dependency-metadata change on b rolls b — and a, whose expanded
        // list now includes chain.d — while c keeps its version.
        fs::write(
            scss_dir.join("d.scss"),
            "// @module chain.d\n.d { color: black; }",
        )
        .unwrap();
        fs::write(
            scss_dir.join("b.scss"),
            "// @module chain.b\n// @requires chain.c\n// @requires chain.d\n.b { color: blue; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(manifest.assets["css/b.css"].version, 2);
        assert_eq!(
            manifest.assets["css/a.css"].version, 2,
            "the dependent's expanded dep list changed too"
        );
        assert_eq!(manifest.assets["css/a.css"].history.len(), 1);
        assert_eq!(
            manifest.assets["css/a.css"].dependencies,
            vec![
                "chain.b".to_string(),
                "chain.c".to_string(),
                "chain.d".to_string()
            ]
        );
        assert_eq!(manifest.assets["css/c.css"].version, 1);
        assert_eq!(manifest.assets["css/d.css"].version, 1);
        let a_v1_pin = history_pin(&manifest, "css/a.css", "chain.a", 0, "css");
        let a_stamp_v2 = pin(&manifest, "css/a.css", "chain.a", 2, "css");

        // A declared module moved to another source path carries its entry —
        // version, history, and every retained pin included.
        fs::create_dir_all(scss_dir.join("nested")).unwrap();
        fs::write(
            scss_dir.join("nested/moved.scss"),
            "// @module chain.a\n// @requires chain.b\n.a { color: red; }",
        )
        .unwrap();
        fs::remove_file(scss_dir.join("a.scss")).unwrap();
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["css/nested/moved.css"];
        assert!(!manifest.assets.contains_key("css/a.css"));
        assert_eq!(entry.version, 2, "the rename keeps the version sequence");
        assert_eq!(entry.history.len(), 1);
        assert_eq!(
            manifest.modules.get("chain.a").map(String::as_str),
            Some("css/nested/moved.css")
        );
        assert_eq!(serve(&manifest, &graph, &a_v1_pin), 200);
        assert_eq!(serve(&manifest, &graph, &a_stamp_v2), 200);

        // Duplicate declarations keep the first file in scan order; the
        // later one is not compiled or recorded — a path-keyed duplicate
        // asset would still join the rollup, so static CSS would not keep
        // the first declaration.
        fs::write(
            scss_dir.join("dup_a.scss"),
            "// @module theme.dup\n.dupa { color: red; }",
        )
        .unwrap();
        fs::write(
            scss_dir.join("dup_b.scss"),
            "// @module theme.dup\n.dupb { color: blue; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest.asset_key_for_module("theme.dup", "css").as_deref(),
            Some("css/dup_a.css"),
            "the first declaration in scan order wins"
        );
        assert!(
            manifest
                .assets
                .get("css/dup_b.css")
                .is_none_or(|entry| entry.retired),
            "a duplicate declaration never becomes an active asset"
        );
        let rollup = fs::read_to_string(dir.path().join("dist/css/assetgraph.css")).unwrap();
        assert!(rollup.contains(".dupa"), "the first rule is in the rollup");
        assert!(
            !rollup.contains(".dupb"),
            "a duplicate declaration never joins the rollup: {rollup}"
        );
        let fresh = crate::jam::combo(
            &manifest,
            &graph.output_dirs,
            "/paste",
            "/paste/theme.dup.min.css",
            None,
        );
        assert_eq!(fresh.status, 200);
        assert!(
            fresh.body.contains(".dupa"),
            "the unpinned combo serves the first"
        );
        assert!(
            !fresh.body.contains(".dupb"),
            "the unpinned combo never serves the duplicate: {}",
            fresh.body
        );
    }

    #[test]
    fn test_aliased_output_dir_is_never_rescanned() {
        // `other/../assets/dist` is the same directory as `assets/dist`:
        // loading normalizes the alias onto the absolute form the walk
        // compares against, so generated output nested under a source root
        // is pruned rather than discovered as source on the next build.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let src = dir.path().join("assets");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("app.js"), "// @module app\nvar app = 1;").unwrap();

        fs::write(
            dir.path().join("paste.conf"),
            r#"source.dirs = ["assets"]
output.base = "other/../assets/dist"
manifest = "manifest.json"
storage.backend = "dist"
"#,
        )
        .unwrap();
        let config = Config::load_with_base_dir(Path::new("paste.conf"), dir.path()).unwrap();
        assert_eq!(
            config.output.base,
            dir.path().join("assets/dist"),
            "the alias must normalize to the real output directory"
        );

        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        assert!(manifest.assets.contains_key("js/app.js"));

        // The dist build wrote unversioned rollups into the output tree —
        // the second build must not pick them up as sources.
        let manifest = graph.build().unwrap();
        assert!(
            !manifest.assets.keys().any(|key| key.contains("assetgraph")),
            "generated rollup files must never re-enter discovery"
        );
        assert_eq!(
            manifest.assets["js/app.js"].version, 1,
            "the second build is a no-op"
        );
    }

    #[test]
    fn test_layout_migration_preserves_every_retained_pin() {
        // flat -> beside-source -> renamed build dir -> flat: every retained
        // version is located by manifest-owned metadata, verified against its
        // recorded hash, and rewritten at the new logical path. The old pins
        // keep serving throughout.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        let source = dir.path().join("src/app.js");
        fs::write(&source, "// @module app\nvar app = 'A';").unwrap();

        let mut config = dirs_config(dir.path(), &["src"], false);
        config.jam.enabled = true;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let v1_pin = pin(&manifest, "js/app.js", "app", 1, "js");
        let v1_body =
            crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v1_pin, None).body;
        assert_eq!(serve(&manifest, &graph, &v1_pin), 200);

        fs::write(&source, "// @module app\nvar app = 'B';").unwrap();
        let manifest = graph.build().unwrap();
        let v2_pin = pin(&manifest, "js/app.js", "app", 2, "js");
        let v2_body =
            crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v2_pin, None).body;
        assert_eq!(manifest.assets["js/app.js"].history.len(), 1);
        let retained_stamp = manifest.assets["js/app.js"].history[0].timestamp.clone();
        let retained_hash = manifest.assets["js/app.js"].history[0].hash.clone();

        // -> beside-source
        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.jam.enabled = true;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 2);
        assert_eq!(entry.history.len(), 1, "retained history survives");
        assert!(entry.output.starts_with("/src/_paste_build/app."));
        assert_eq!(
            entry.history[0].version, 1,
            "the retained version number survives"
        );
        assert_eq!(
            entry.history[0].timestamp, retained_stamp,
            "the retained stamp survives"
        );
        assert_eq!(
            entry.history[0].hash, retained_hash,
            "the retained hash survives"
        );
        for output in std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
        {
            assert!(
                output.starts_with("/src/_paste_build/"),
                "every recorded output moves to the new root: {output}"
            );
            assert!(
                dir.path().join(output.trim_start_matches('/')).exists(),
                "the rewritten file exists: {output}"
            );
        }
        // The old flat versioned files are pruned; the unversioned rollup
        // is regenerated at the type output root regardless of layout.
        let flat_js = dir.path().join("dist/js");
        let leftovers: Vec<_> = fs::read_dir(&flat_js)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftovers.iter().all(|name| !name.starts_with("app.")),
            "the old flat app outputs are pruned: {leftovers:?}"
        );
        assert_eq!(
            serve(&manifest, &graph, &v1_pin),
            200,
            "the v1 pin survives a layout move"
        );
        assert_eq!(serve(&manifest, &graph, &v2_pin), 200);
        assert_eq!(
            crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v1_pin, None).body,
            v1_body,
            "the v1 pin still serves the original bytes"
        );

        // -> renamed build dir
        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.jam.enabled = true;
        config.output.build_dir_name = "gen".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert!(entry.output.starts_with("/src/gen/app."));
        let old_build = dir.path().join("src/_paste_build");
        assert!(
            !old_build.exists() || fs::read_dir(&old_build).unwrap().next().is_none(),
            "the renamed build dir holds no generated files"
        );
        for output in std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
        {
            assert!(dir.path().join(output.trim_start_matches('/')).exists());
        }
        assert_eq!(serve(&manifest, &graph, &v1_pin), 200);
        assert_eq!(serve(&manifest, &graph, &v2_pin), 200);
        assert_eq!(
            crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v2_pin, None).body,
            v2_body
        );

        // -> flat again
        let mut config = dirs_config(dir.path(), &["src"], false);
        config.jam.enabled = true;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(
            manifest.assets.len(),
            1,
            "generated files never re-enter discovery"
        );
        assert!(entry.output.starts_with("/js/app."));
        for output in std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
        {
            assert!(output.starts_with("/js/"), "back at flat: {output}");
            assert!(
                dir.path()
                    .join("dist")
                    .join(output.trim_start_matches('/'))
                    .exists(),
                "flat file exists: {output}"
            );
        }
        assert_eq!(serve(&manifest, &graph, &v1_pin), 200);
        assert_eq!(serve(&manifest, &graph, &v2_pin), 200);
    }

    #[test]
    fn test_tampered_or_missing_retained_output_fails_the_build() {
        // A retained file that no longer matches its recorded hash is never
        // silently replaced by current bytes — the build stops and names it.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = js_dir.join("app.js");
        fs::write(&source, "var app = 1;").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        graph.build().unwrap();
        fs::write(&source, "var app = 2;").unwrap();
        let manifest = graph.build().unwrap();
        let retained = &manifest.assets["js/app.js"].history[0];
        let retained_file = dir
            .path()
            .join("dist")
            .join(retained.output.trim_start_matches('/'));
        assert!(retained_file.exists());

        fs::write(&retained_file, "tampered").unwrap();
        let err = graph.build().unwrap_err();
        assert!(
            err.to_string().contains("corrupted retained output"),
            "tampering must be detected: {err}"
        );

        fs::remove_file(&retained_file).unwrap();
        let err = graph.build().unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("missing") && message.contains("js/app.js"),
            "a missing retained file must be named: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_source_fails_the_build() {
        // A discovery or read error propagates — a partial manifest is never
        // published in place of a failed scan.
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = js_dir.join("app.js");
        fs::write(&source, "var app = 1;").unwrap();

        fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();
        let result = AssetGraph::new(dist_config(dir.path(), false)).build();
        // Restore before asserting so cleanup cannot fail on some platforms.
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(result.is_err(), "an unreadable source must fail the build");
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_source_subdir_fails_discovery() {
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let blocked = js_dir.join("private");
        fs::create_dir_all(&blocked).unwrap();
        fs::write(blocked.join("hidden.js"), "var h = 1;").unwrap();
        fs::write(js_dir.join("app.js"), "var app = 1;").unwrap();

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        let result = AssetGraph::new(dist_config(dir.path(), false)).build();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err(), "a walk error must fail the build");
    }

    // ============================================================
    // jam.hash-urls policy in real builds
    // ============================================================

    /// One single-module project built at a forced source mtime.
    fn build_at_stamp(
        body: &str,
        requires: &str,
        hash_urls: bool,
    ) -> (tempfile::TempDir, Manifest, AssetGraph) {
        let dir = tempfile::tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = js_dir.join("a.js");
        fs::write(&source, format!("// @module a\n{requires}{body}")).unwrap();
        // Both builds see the same source mtime — the stamp cannot be what
        // distinguishes their URLs.
        filetime::set_file_mtime(
            &source,
            filetime::FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        let mut config = dist_config(dir.path(), true);
        config.jam.hash_urls = hash_urls;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        (dir, manifest, graph)
    }

    #[test]
    fn test_hash_urls_distinguish_same_stamp_same_version_builds() {
        // Two independent builds — identical source mtimes, different
        // compiled bytes, no prior manifest — mint the same version and the
        // same stamp, but different qualified URLs: the hash, not the stamp,
        // names the content.
        let (_dir_a, manifest_a, _graph_a) = build_at_stamp("var a = 1;", "", true);
        let (_dir_b, manifest_b, graph_b) = build_at_stamp("var a = 2;", "", true);
        let entry_a = &manifest_a.assets["js/a.js"];
        let entry_b = &manifest_b.assets["js/a.js"];

        assert!(manifest_a.hash_urls);
        assert_eq!(entry_a.version, 1);
        assert_eq!(entry_b.version, 1);
        assert_eq!(entry_a.timestamp, entry_b.timestamp);
        assert_ne!(entry_a.hash, entry_b.hash);

        let url_a = manifest_a.jam_url(&["a"], "js", "/paste", false).unwrap();
        let url_b = manifest_b.jam_url(&["a"], "js", "/paste", false).unwrap();
        assert_ne!(url_a, url_b);
        assert!(url_a.contains(&format!("-h{}", entry_a.hash)), "{url_a}");
        assert!(url_b.contains(&format!("-h{}", entry_b.hash)), "{url_b}");

        // Build A's qualified URL asked of build B names bytes B never
        // wrote — a 404, never a redirect or different content.
        let r = crate::jam::combo(&manifest_b, &graph_b.output_dirs, "/paste", &url_a, None);
        assert_eq!(r.status, 404, "{url_a}");
        assert_eq!(r.cache_control, "no-cache");
    }

    #[test]
    fn test_rebuild_unchanged_content_keeps_the_qualified_url() {
        let (_dir, manifest, mut graph) = build_at_stamp("var a = 1;", "", true);
        let url = manifest.jam_url(&["a"], "js", "/paste", false).unwrap();

        let manifest2 = graph.build().unwrap();
        let url2 = manifest2.jam_url(&["a"], "js", "/paste", false).unwrap();
        assert_eq!(url, url2, "an unchanged rebuild must mint the same URL");
        assert_eq!(manifest2.assets["js/a.js"].version, 1);

        // Flipping the real build config — not just the in-memory field —
        // rewrites only the policy: every asset record (version, hash,
        // stamp, history) is carried forward unchanged while the persisted
        // manifest and its emitted URLs switch to the legacy shape.
        graph.config.jam.hash_urls = false;
        let legacy = graph.build().unwrap();
        assert!(!legacy.hash_urls);
        assert_eq!(legacy.assets, manifest2.assets);
        let legacy_url = legacy.jam_url(&["a"], "js", "/paste", false).unwrap();
        assert!(legacy_url.ends_with("/a+v1.0.min.js"), "{legacy_url}");
        assert!(!legacy_url.contains("-h"), "{legacy_url}");

        graph.config.jam.hash_urls = true;
        let reenabled = graph.build().unwrap();
        assert!(reenabled.hash_urls);
        assert_eq!(reenabled.assets, manifest2.assets);
        assert_eq!(
            reenabled.jam_url(&["a"], "js", "/paste", false).unwrap(),
            url,
            "re-enabling must restore the original qualified URL"
        );
    }

    #[test]
    fn test_hash_url_optout_build_emits_legacy_urls() {
        let (_dir, manifest, _graph) = build_at_stamp("var a = 1;", "", false);
        assert!(!manifest.hash_urls);
        let url = manifest.jam_url(&["a"], "js", "/paste", false).unwrap();
        assert!(url.ends_with("/a+v1.0.min.js"), "{url}");
        assert!(!url.contains("-h"), "{url}");
    }
}
