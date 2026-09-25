use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

fn default_true() -> bool {
    true
}

fn default_inline_threshold() -> usize {
    1024
}

fn default_output_layout() -> String {
    "flat".to_string()
}

fn default_build_dir_name() -> String {
    "_paste_build".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourcePaths {
    /// Asset source roots, each walked recursively; every file is classified
    /// by extension (`.js`, `.scss`, `.css`) regardless of the folder it sits
    /// in. Resolved against base_dir.
    #[serde(default)]
    pub dirs: Vec<PathBuf>,
    /// Legacy JS root — appended after `dirs` when set. Empty means unset.
    #[serde(default)]
    pub js: PathBuf,
    /// Legacy SCSS root — appended after `js` when set. Empty means unset.
    #[serde(default)]
    pub scss: PathBuf,
    #[serde(default)]
    pub images: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputPaths {
    pub base: PathBuf,
    pub js: PathBuf,
    pub css: PathBuf,
    pub images: PathBuf,
    /// "flat" (default): every compiled file lands under the type's own
    /// output directory. "beside-source": a file discovered under a source
    /// dir inside base_dir compiles into `<file dir>/<build_dir_name>/`
    /// beside its source; include-path files keep flat placement under
    /// `base`. In beside-source mode every manifest output is relative to
    /// base_dir, which is also the runtime's output root.
    #[serde(default = "default_output_layout")]
    pub layout: String,
    /// Directory name beside-source output is written into — a single plain
    /// path segment. Directories with this name are never scanned as
    /// sources, in either layout.
    #[serde(default = "default_build_dir_name")]
    pub build_dir_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JamConfig {
    pub enabled: bool,
    pub base_path: String,
    /// Hash-qualified versioned combo URLs (`+vN.0-h<32hex>`) are emitted by
    /// default; `false` opts a build back to the legacy `+vN.0` token shape.
    /// Correctly qualified URLs still resolve while the policy is off.
    #[serde(default = "default_true")]
    pub hash_urls: bool,
    #[serde(default = "default_inline_threshold")]
    pub inline_threshold: usize,
}

impl Default for JamConfig {
    fn default() -> Self {
        JamConfig {
            enabled: false,
            base_path: "/paste".to_string(),
            hash_urls: true,
            inline_threshold: default_inline_threshold(),
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
pub struct TemplatesConfig {
    pub source: PathBuf,
    /// Rendered HTML output; falls back to `output.base` when unset.
    #[serde(default)]
    pub output: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Backend type: "local" or "dist"
    pub backend: String,
    /// Number of versions to keep (for the dist backend)
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
    /// Build-time template rendering — Some only when `templates.source`
    /// is configured.
    #[serde(default)]
    pub templates: Option<TemplatesConfig>,
}

impl Config {
    /// Load config with an explicit base directory for path resolution.
    /// HOCON configs expect relative paths to resolve against the config
    /// file's directory, not the process CWD — callers that only have a path
    /// resolve it against their own working directory first.
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
        if resolved_path.extension().is_some_and(|e| e == "json") {
            let mut cfg: Config = serde_json::from_str(&content)?;
            // Resolve all relative paths against base_dir, matching HOCON behavior.
            // JSON configs deserialized by serde contain paths as-is from the file.
            // An unset legacy js/scss root stays empty — `source_dirs` reads
            // emptiness as "not configured" and falls back to the defaults
            // only when nothing at all was configured.
            cfg.base_dir = base_dir.to_path_buf();
            cfg.source.dirs = cfg
                .source
                .dirs
                .iter()
                .map(|p| Self::resolve_path(base_dir, p))
                .collect();
            if !cfg.source.js.as_os_str().is_empty() {
                cfg.source.js = Self::resolve_path(base_dir, &cfg.source.js);
            }
            if !cfg.source.scss.as_os_str().is_empty() {
                cfg.source.scss = Self::resolve_path(base_dir, &cfg.source.scss);
            }
            cfg.source.images = if cfg.source.images.as_os_str().is_empty() {
                base_dir.join("src/main/assets/images")
            } else {
                Self::resolve_path(base_dir, &cfg.source.images)
            };
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
            if let Some(templates) = cfg.templates.as_mut() {
                templates.source = Self::resolve_path(base_dir, &templates.source);
                templates.output = if templates.output.as_os_str().is_empty() {
                    cfg.output.base.clone()
                } else {
                    Self::resolve_path(base_dir, &templates.output)
                };
            }
            cfg.normalize_paths()?;
            cfg.validate()?;
            return Ok(cfg);
        }

        Self::parse_hocon(&content, base_dir)
    }

    fn parse_hocon(content: &str, base_dir: &Path) -> Result<Self> {
        // Simplified HOCON parsing - extract key values
        // HOCON last-value-wins: keep the final raw match
        let get_raw = |key: &str| -> Option<String> {
            let mut last_value = None;
            for line in content.lines() {
                let line = line.trim();
                // Check for exact key match: key must be followed by '=' or whitespace
                if line.starts_with(key) {
                    let after_key = line.chars().nth(key.len());
                    if after_key == Some('=') || after_key == Some(' ') || after_key == Some('\t') {
                        if let Some(value) = line.split_once('=').map(|(_, v)| v) {
                            let value = Self::strip_inline_comment(value.trim());
                            last_value = Some(value.to_string());
                        }
                    }
                }
            }
            last_value
        };

        let get_value = |key: &str| -> Result<Option<String>> {
            get_raw(key).map(|v| Self::config_string(&v)).transpose()
        };

        // Booleans and numerics parse strictly: a typo is a configuration
        // error, never a silent default — a malformed setting fails the
        // load instead of quietly applying the fallback.
        let get_bool = |key: &str, fallback: bool| -> Result<bool> {
            Ok(get_value(key)?
                .map(|s| s.parse::<bool>())
                .transpose()?
                .unwrap_or(fallback))
        };

        // HOCON last-value-wins applies to arrays too: only the final match
        // is read, and a malformed final value is an error, not a fallback.
        let get_array = |key: &str| -> Result<Vec<String>> {
            match get_raw(key) {
                Some(value) => Self::config_array(&value),
                None => Ok(Vec::new()),
            }
        };

        let include_paths: Vec<PathBuf> = get_array("include-paths")?
            .iter()
            .map(|p| base_dir.join(p))
            .collect();

        let output_base =
            base_dir.join(get_value("output.base")?.unwrap_or_else(|| "src/main/public".into()));

        let config = Config {
            base_dir: base_dir.to_path_buf(),
            debug_mode: get_bool("debug-mode", false)?,
            minify: get_bool("minify", true)?,
            source_maps: get_bool("source-maps", true)?,
            source: SourcePaths {
                dirs: get_array("source.dirs")?
                    .iter()
                    .map(|p| base_dir.join(p))
                    .collect(),
                // Unset legacy roots stay empty — `source_dirs` treats empty
                // as "not configured" and only falls back to the defaults
                // when no source root was configured at all.
                js: get_value("source.js")?
                    .map(|v| base_dir.join(v))
                    .unwrap_or_default(),
                scss: get_value("source.scss")?
                    .map(|v| base_dir.join(v))
                    .unwrap_or_default(),
                images: base_dir.join(
                    get_value("source.images")?.unwrap_or_else(|| "src/main/assets/images".into()),
                ),
            },
            output: OutputPaths {
                base: output_base.clone(),
                // Type directories derive from output.base when unset, so a
                // relocated output root cannot strand defaults under the old
                // src/main/public prefix.
                js: get_value("output.js")?
                    .map(|p| base_dir.join(p))
                    .unwrap_or_else(|| output_base.join("js")),
                css: get_value("output.css")?
                    .map(|p| base_dir.join(p))
                    .unwrap_or_else(|| output_base.join("css")),
                images: get_value("output.images")?
                    .map(|p| base_dir.join(p))
                    .unwrap_or_else(|| output_base.join("images")),
                layout: get_value("output.layout")?.unwrap_or_else(default_output_layout),
                build_dir_name: get_value("output.build-dir-name")?
                    .unwrap_or_else(default_build_dir_name),
            },
            manifest_path: base_dir.join(
                get_value("manifest")?.unwrap_or_else(|| "src/main/public/manifest.json".into()),
            ),
            include_paths,
            storage: StorageConfig {
                backend: get_value("storage.backend")?.unwrap_or_else(|| "local".into()),
                keep_versions: get_value("storage.keep-versions")?
                    .map(|v| v.parse::<usize>())
                    .transpose()?
                    .unwrap_or(10),
            },
            jam: JamConfig {
                enabled: get_bool("jam.enabled", false)?,
                base_path: get_value("jam.base-path")?.unwrap_or_else(|| "/paste".into()),
                hash_urls: get_bool("jam.hash-urls", true)?,
                inline_threshold: get_value("jam.inline-threshold")?
                    .map(|v| v.parse::<usize>())
                    .transpose()?
                    .unwrap_or_else(default_inline_threshold),
            },
            cdn: CdnConfig {
                enabled: get_bool("cdn.enabled", false)?,
                url: get_value("cdn.url")?.unwrap_or_default(),
                fallback_to_local: get_bool("cdn.fallback-to-local", true)?,
            },
            templates: match get_value("templates.source")? {
                Some(source) => Some(TemplatesConfig {
                    source: base_dir.join(source),
                    output: get_value("templates.output")?
                        .map(|o| base_dir.join(o))
                        .unwrap_or(output_base),
                }),
                None => None,
            },
        };
        let mut config = config;
        config.normalize_paths()?;
        config.validate()?;
        Ok(config)
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

    /// Decode one raw HOCON scalar: a `"`-quoted value is a JSON-style string
    /// whose escapes are honored (Scala generators emit `\"` and `\\`);
    /// anything else is kept verbatim.
    fn config_string(value: &str) -> Result<String> {
        if value.starts_with('"') {
            Ok(serde_json::from_str::<String>(value)?)
        } else {
            Ok(value.to_owned())
        }
    }

    /// Decode a `[a, "b,c", ...]` literal. Commas split elements only outside
    /// quotes; each element goes through `config_string`. Unterminated quotes
    /// or escapes are errors, never a silent default.
    fn config_array(value: &str) -> Result<Vec<String>> {
        let value = value.trim();
        anyhow::ensure!(
            value.starts_with('[') && value.ends_with(']'),
            "expected an array literal [..], got {:?}",
            value
        );
        let inner = &value[1..value.len() - 1];
        let mut items = Vec::new();
        let mut in_quotes = false;
        let mut escaped = false;
        let mut start = 0;
        for (index, character) in inner.char_indices() {
            if escaped {
                escaped = false;
            } else if in_quotes && character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_quotes = !in_quotes;
            } else if character == ',' && !in_quotes {
                let token = inner[start..index].trim();
                if !token.is_empty() {
                    items.push(Self::config_string(token)?);
                }
                start = index + 1;
            }
        }
        anyhow::ensure!(!in_quotes, "unterminated quote in array {:?}", value);
        anyhow::ensure!(!escaped, "unterminated escape in array {:?}", value);
        let token = inner[start..].trim();
        if !token.is_empty() {
            items.push(Self::config_string(token)?);
        }
        Ok(items)
    }

    /// Resolve a path against base_dir if it is relative; return as-is if absolute.
    fn resolve_path(base_dir: &Path, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_dir.join(p)
        }
    }

    /// Absolutize then lexically normalize a path — a relative path resolves
    /// against the process working directory, then `.`/`..` collapse
    /// component-wise. Never relies on string prefixes or on the filesystem
    /// (`Path::canonicalize` would fail on directories that do not exist
    /// yet, which is legal here). The result is always absolute, so a `..`
    /// can only shorten real ancestors, never drop silently.
    fn normalize_path(path: &Path) -> Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut out = PathBuf::new();
        for component in absolute.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        Ok(out)
    }

    /// Normalize every filesystem path in the config — once, after relative
    /// paths have been resolved and before validation, so the paths the
    /// build walks and the paths it compares carry the same form and a `..`
    /// alias cannot split them. Unset legacy `source.js`/`source.scss`
    /// fields stay empty — `source_dirs` reads emptiness as "not
    /// configured".
    fn normalize_paths(&mut self) -> Result<()> {
        self.base_dir = Self::normalize_path(&self.base_dir)?;
        self.source.dirs = self
            .source
            .dirs
            .iter()
            .map(|p| Self::normalize_path(p))
            .collect::<Result<_>>()?;
        for slot in [
            &mut self.source.js,
            &mut self.source.scss,
            &mut self.source.images,
        ] {
            if !slot.as_os_str().is_empty() {
                *slot = Self::normalize_path(slot)?;
            }
        }
        self.output.base = Self::normalize_path(&self.output.base)?;
        self.output.js = Self::normalize_path(&self.output.js)?;
        self.output.css = Self::normalize_path(&self.output.css)?;
        self.output.images = Self::normalize_path(&self.output.images)?;
        self.manifest_path = Self::normalize_path(&self.manifest_path)?;
        self.include_paths = self
            .include_paths
            .iter()
            .map(|p| Self::normalize_path(p))
            .collect::<Result<_>>()?;
        if let Some(templates) = self.templates.as_mut() {
            if !templates.source.as_os_str().is_empty() {
                templates.source = Self::normalize_path(&templates.source)?;
            }
            if !templates.output.as_os_str().is_empty() {
                templates.output = Self::normalize_path(&templates.output)?;
            }
        }
        Ok(())
    }

    /// Every asset root the build walks, in scan order: `source.dirs` first,
    /// then the legacy `source.js` and `source.scss` roots when set,
    /// deduplicated. When nothing is configured the defaults apply — the js
    /// root, then the scss root — so path-derived module names keep the
    /// walked root they have always had.
    pub fn source_dirs(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for candidate in self
            .source
            .dirs
            .iter()
            .cloned()
            .chain((!self.source.js.as_os_str().is_empty()).then(|| self.source.js.clone()))
            .chain((!self.source.scss.as_os_str().is_empty()).then(|| self.source.scss.clone()))
        {
            if !roots.contains(&candidate) {
                roots.push(candidate);
            }
        }
        if roots.is_empty() {
            roots.push(self.base_dir.join("src/main/assets/js"));
            roots.push(self.base_dir.join("src/main/assets/scss"));
        }
        roots
    }

    /// The compiled-file resolver for this layout. `flat` resolves each
    /// output through the configured type directories; `beside-source`
    /// records every output relative to base_dir, so the resolver's root is
    /// the project itself — which is also the output root the runtime is
    /// handed.
    pub fn output_dirs(&self) -> crate::jam::OutputDirs {
        if self.output.layout == "beside-source" {
            crate::jam::OutputDirs::under(&self.base_dir)
        } else {
            crate::jam::OutputDirs {
                base: self.output.base.clone(),
                js: self.output.js.clone(),
                css: self.output.css.clone(),
            }
        }
    }

    /// Config invariants a build depends on: a known output layout, a
    /// single-segment build-dir name (it becomes a directory component in
    /// both layouts), and — beside-source records every output relative to
    /// base_dir — an output base inside it.
    pub fn validate(&self) -> Result<()> {
        match self.output.layout.as_str() {
            "flat" | "beside-source" => {}
            other => anyhow::bail!(
                "output.layout must be \"flat\" or \"beside-source\", got {:?}",
                other
            ),
        }
        let name = self.output.build_dir_name.as_str();
        let is_segment = matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [std::path::Component::Normal(_)]
        ) && !name.contains(['\\', ':']);
        if !is_segment {
            anyhow::bail!(
                "output.build-dir-name must be a single path segment, got {:?}",
                name
            );
        }
        // Containment is decided on normalized absolute paths — a raw
        // `starts_with` would take `output.base = <base>/../escape` for an
        // in-tree path while the build writes outside it.
        let base_dir = Self::normalize_path(&self.base_dir)?;
        let output_base = Self::normalize_path(&self.output.base)?;
        if self.output.layout == "beside-source" && !output_base.starts_with(&base_dir) {
            anyhow::bail!(
                "output.base {:?} must be inside base_dir {:?} when output.layout is \"beside-source\"",
                self.output.base,
                self.base_dir
            );
        }
        match self.storage.backend.as_str() {
            "local" | "dist" => {}
            other => anyhow::bail!(
                "storage.backend must be \"local\" or \"dist\", got {:?}",
                other
            ),
        }
        // The manifest cache lives at manifest_path.with_extension("bin") —
        // a manifest already named *.bin would collide with its own cache.
        if self.manifest_path.extension().is_some_and(|e| e == "bin") {
            anyhow::bail!(
                "manifest path {:?} must not use the .bin extension; it collides with the manifest cache file",
                self.manifest_path
            );
        }
        // A source root inside generated output is never a valid build:
        // discovery would walk its own compiled files as inputs (or exclude
        // the root outright and write an empty baseline). Output nested
        // inside a source tree stays legal — the generated directories are
        // filtered on the walk.
        let normalized_output: Vec<PathBuf> = [
            &self.output.base,
            &self.output.js,
            &self.output.css,
            &self.output.images,
        ]
        .iter()
        .map(|dir| Self::normalize_path(dir))
        .collect::<Result<_>>()?;
        for source_root in self.source_dirs() {
            let source_root = Self::normalize_path(&source_root)?;
            for out_dir in &normalized_output {
                if source_root.starts_with(out_dir) {
                    anyhow::bail!(
                        "source dir {:?} is inside or equal to output directory {:?} — a source root must not overlap generated output",
                        source_root,
                        out_dir
                    );
                }
            }
        }
        Ok(())
    }

    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug_mode = debug;
        self
    }

    #[cfg(test)]
    pub fn default_for(base_dir: &Path) -> Self {
        Config {
            base_dir: base_dir.to_path_buf(),
            debug_mode: false,
            minify: true,
            source_maps: true,
            source: SourcePaths {
                dirs: Vec::new(),
                js: base_dir.join("src/main/assets/js"),
                scss: base_dir.join("src/main/assets/scss"),
                images: base_dir.join("src/main/assets/images"),
            },
            output: OutputPaths {
                base: base_dir.join("src/main/public"),
                js: base_dir.join("src/main/public/js"),
                css: base_dir.join("src/main/public/css"),
                images: base_dir.join("src/main/public/images"),
                layout: default_output_layout(),
                build_dir_name: default_build_dir_name(),
            },
            manifest_path: base_dir.join("src/main/public/manifest.json"),
            include_paths: Vec::new(),
            storage: StorageConfig::default(),
            jam: JamConfig {
                enabled: true,
                base_path: "/paste".into(),
                hash_urls: true,
                inline_threshold: default_inline_threshold(),
            },
            cdn: CdnConfig {
                enabled: false,
                url: String::new(),
                fallback_to_local: true,
            },
            templates: None,
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
jam.hash-urls = true
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
jam.hash-urls = true
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

templates.source = "templates"
templates.output = "public"

storage.backend = "dist"
storage.keep-versions = 10
manifest = "public/manifest.json"

minify = true
source-maps = true

jam.enabled = true
jam.base-path = "/paste"
jam.hash-urls = true
"#
        }
    };

    let output_path = base_dir.join("paste-surface.conf");
    fs::write(&output_path, config)?;
    println!("Created {:?} for {} project", output_path, project_type);
    Ok(())
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
jam.hash-urls = true
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
            config.cdn.url, r#"path with "quoted" part"#,
            "Escaped quotes should decode to real quotes, comment stripped"
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
        // `../lib` escapes base_dir by its own text — the resolved path is
        // normalized to the real parent-relative directory.
        assert_eq!(
            config.include_paths[0],
            dir.path().parent().unwrap().join("lib"),
            "JSON include_paths resolve against base_dir, honoring '..'"
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

    // ============================================================
    // templates.* and jam.inline-threshold
    // ============================================================

    #[test]
    fn test_config_templates_and_inline_threshold_hocon() {
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(
            r#"
output.base = "dist"
templates.source = "pages"
templates.output = "site"
jam.inline-threshold = 512
"#,
            dir.path(),
        )
        .unwrap();
        let templates = config.templates.as_ref().expect("templates configured");
        assert!(
            templates.source.ends_with("pages"),
            "{:?}",
            templates.source
        );
        assert!(templates.output.ends_with("site"), "{:?}", templates.output);
        assert!(templates.source.starts_with(dir.path()));
        assert!(templates.output.starts_with(dir.path()));
        assert_eq!(config.jam.inline_threshold, 512);

        // templates.source alone renders into output.base.
        let config = Config::parse_hocon(
            "output.base = \"dist\"\ntemplates.source = \"pages\"\n",
            dir.path(),
        )
        .unwrap();
        let templates = config.templates.as_ref().expect("templates configured");
        assert_eq!(templates.output, config.output.base);

        // Absent keys: no templates, the default threshold.
        let config = Config::parse_hocon("output.base = \"dist\"\n", dir.path()).unwrap();
        assert!(config.templates.is_none());
        assert_eq!(config.jam.inline_threshold, 1024);
    }

    #[test]
    fn test_config_templates_and_inline_threshold_json() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("config.json");
        fs::write(
            &json_path,
            r#"{
                "source": { "js": "src/js", "scss": "src/scss", "images": "src/images" },
                "output": { "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" },
                "manifest_path": "dist/manifest.json",
                "templates": { "source": "pages", "output": "site" },
                "jam": { "enabled": true, "base_path": "/paste", "inline_threshold": 256 }
            }"#,
        )
        .unwrap();
        let config =
            Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path()).unwrap();
        let templates = config.templates.as_ref().expect("templates configured");
        assert!(templates.source.starts_with(dir.path()));
        assert!(
            templates.source.ends_with("pages"),
            "{:?}",
            templates.source
        );
        assert!(templates.output.ends_with("site"), "{:?}", templates.output);
        assert_eq!(config.jam.inline_threshold, 256);

        // JSON without the keys: templates None, threshold defaulting.
        fs::write(
            &json_path,
            r#"{
                "source": { "js": "src/js", "scss": "src/scss", "images": "src/images" },
                "output": { "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" },
                "manifest_path": "dist/manifest.json",
                "jam": { "enabled": true, "base_path": "/paste" }
            }"#,
        )
        .unwrap();
        let config =
            Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path()).unwrap();
        assert!(config.templates.is_none());
        assert_eq!(config.jam.inline_threshold, 1024);
    }

    #[test]
    fn test_init_basic_writes_templates_keys() {
        let dir = tempdir().unwrap();
        init_config("basic", dir.path()).unwrap();
        let written = fs::read_to_string(dir.path().join("paste-surface.conf")).unwrap();
        assert!(written.contains("templates.source"), "{written}");
        assert!(written.contains("templates.output"), "{written}");
        let config = Config::parse_hocon(&written, dir.path()).unwrap();
        assert!(config.templates.is_some());

        for project_type in ["react", "scala"] {
            init_config(project_type, dir.path()).unwrap();
            let written = fs::read_to_string(dir.path().join("paste-surface.conf")).unwrap();
            assert!(
                !written.contains("templates.source"),
                "{project_type} must not write templates keys"
            );
        }
    }

    // ============================================================
    // Quoted scalars and arrays decode escapes like the generators emit
    // ============================================================

    #[test]
    fn test_config_source_dirs_quoted_elements_round_trip() {
        // sbt-style generators quote every element and escape quotes and
        // backslashes; an element may itself contain a comma.
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(
            r#"source.dirs = ["assets, js", "say \"hi\"", "C:\\assets"]"#,
            dir.path(),
        )
        .unwrap();
        let dirs: Vec<String> = config
            .source
            .dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(dirs.len(), 3, "{dirs:?}");
        assert!(dirs[0].ends_with("assets, js"), "{}", dirs[0]);
        assert!(dirs[1].ends_with("say \"hi\""), "{}", dirs[1]);
        assert!(dirs[2].ends_with("C:\\assets"), "{}", dirs[2]);
    }

    #[test]
    fn test_config_rejects_malformed_quoted_values() {
        let dir = tempdir().unwrap();
        // Unterminated scalar quote
        assert!(Config::parse_hocon("output.base = \"unterminated\n", dir.path()).is_err());
        // Trailing garbage after a quoted scalar
        assert!(Config::parse_hocon("output.base = \"dist\" junk\n", dir.path()).is_err());
        // Array key given a non-array value
        assert!(Config::parse_hocon("include-paths = not-an-array\n", dir.path()).is_err());
        // Unterminated quote inside an array element
        assert!(Config::parse_hocon("include-paths = [\"a, \"b\"]\n", dir.path()).is_err());
        // Unterminated escape inside an array element
        assert!(Config::parse_hocon("include-paths = [\"a\\]\n", dir.path()).is_err());
    }

    #[test]
    fn test_config_output_dirs_default_under_output_base() {
        // With only output.base configured, the per-type directories live
        // under it — not under a hardcoded src/main/public.
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon("output.base = \"dist\"\n", dir.path()).unwrap();
        let base = dir.path().join("dist");
        assert_eq!(config.output.base, base);
        assert_eq!(config.output.js, base.join("js"));
        assert_eq!(config.output.css, base.join("css"));
        assert_eq!(config.output.images, base.join("images"));
    }

    #[test]
    fn test_config_rejects_unknown_storage_backend() {
        let dir = tempdir().unwrap();
        // s3 is a known name but not a supported backend — falling back to
        // local would silently drop retained versions.
        assert!(Config::parse_hocon(
            "output.base = \"dist\"\nstorage.backend = \"s3\"\n",
            dir.path()
        )
        .is_err());
        assert!(Config::parse_hocon(
            "output.base = \"dist\"\nstorage.backend = \"bogus\"\n",
            dir.path()
        )
        .is_err());
        assert!(Config::parse_hocon(
            "output.base = \"dist\"\nstorage.backend = \"local\"\n",
            dir.path()
        )
        .is_ok());
        assert!(Config::parse_hocon(
            "output.base = \"dist\"\nstorage.backend = \"dist\"\n",
            dir.path()
        )
        .is_ok());
    }

    #[test]
    fn test_config_rejects_bin_manifest_extension() {
        let dir = tempdir().unwrap();
        // manifest = "x.bin" would collide with the cache written at
        // manifest_path.with_extension("bin") — the same file.
        assert!(Config::parse_hocon(
            "output.base = \"dist\"\nmanifest = \"target/history.bin\"\n",
            dir.path()
        )
        .is_err());
        let config = Config::parse_hocon(
            "output.base = \"dist\"\nmanifest = \"target/history.json\"\n",
            dir.path(),
        )
        .unwrap();
        assert!(config.manifest_path.ends_with("history.json"));
    }

    #[test]
    fn test_rejects_typo_bool_values() {
        let dir = tempdir().unwrap();
        for content in [
            "minify = flase",
            r#"debug-mode = "yep""#,
            "jam.enabled = TRUEISH",
            "cdn.enabled = 1",
            "cdn.fallback-to-local = no",
            "source-maps = off",
        ] {
            let result = Config::parse_hocon(content, dir.path());
            assert!(
                result.is_err(),
                "config {content:?} must not silently default"
            );
        }
        let config = Config::parse_hocon("", dir.path()).unwrap();
        assert!(!config.debug_mode && !config.jam.enabled && !config.cdn.enabled);
        assert!(config.minify && config.source_maps && config.cdn.fallback_to_local);
    }

    #[test]
    fn test_rejects_invalid_numeric_values() {
        let dir = tempdir().unwrap();
        for content in [
            "storage.keep-versions = -1",
            r#"storage.keep-versions = "abc""#,
            "jam.inline-threshold = -5",
            r#"jam.inline-threshold = "ten""#,
        ] {
            let result = Config::parse_hocon(content, dir.path());
            assert!(
                result.is_err(),
                "config {content:?} must not silently default"
            );
        }
    }

    #[test]
    fn test_accepts_zero_numeric_values() {
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(
            "storage.keep-versions = 0\njam.inline-threshold = 0\n",
            dir.path(),
        )
        .unwrap();
        assert_eq!(config.storage.keep_versions, 0);
        assert_eq!(config.jam.inline_threshold, 0);
    }

    #[test]
    fn test_rejects_source_root_under_output() {
        let dir = tempdir().unwrap();
        // Equality and nesting are both invalid — discovery would either
        // walk its own output or silently write an empty baseline.
        for source in ["public", "public/js", "public/deep/nested"] {
            let content = format!(
                r#"source.dirs = ["{source}"]
output.base = "public"
"#
            );
            let result = Config::parse_hocon(&content, dir.path());
            assert!(
                result.is_err(),
                "source {source:?} under output must be rejected"
            );
        }
        // Same rejection through the JSON parser — with a fully populated
        // fixture, so the failure can only come from validation.
        fs::write(
            dir.path().join("config.json"),
            r#"{
                "source": {"dirs": ["public/js"]},
                "output": {
                    "base": "public",
                    "js": "out/js",
                    "css": "out/css",
                    "images": "out/images"
                },
                "manifest_path": "out/manifest.json"
            }"#,
        )
        .unwrap();
        let result = Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path());
        let message = format!("{}", result.unwrap_err());
        assert!(
            message.contains("overlap"),
            "expected an overlap validation error, got: {message}"
        );
        // A `..` alias naming the same directory is the same overlap.
        let result = Config::parse_hocon(
            r#"source.dirs = ["public"]
output.base = "assets/../public"
"#,
            dir.path(),
        );
        assert!(
            result.is_err(),
            "output.base escaping through '..' into a source root must be rejected"
        );
    }

    #[test]
    fn test_allows_output_under_source() {
        let dir = tempdir().unwrap();
        // Output nested inside a source tree is filtered on the walk and
        // must not be rejected. Optional source directories need not exist.
        let config = Config::parse_hocon(
            r#"source.dirs = ["assets"]
output.base = "assets/paste-build"
"#,
            dir.path(),
        )
        .unwrap();
        assert_eq!(config.source_dirs(), vec![dir.path().join("assets")]);
        // A `..` in the source path still normalizes into the output tree.
        let result = Config::parse_hocon(
            r#"source.dirs = ["assets/../public"]
output.base = "public"
"#,
            dir.path(),
        );
        assert!(result.is_err());
    }

    // ============================================================
    // jam.hash-urls: default on, both spellings, malformed rejected
    // ============================================================

    #[test]
    fn test_hash_urls_defaults_true() {
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(r#"source.dirs = ["assets"]"#, dir.path()).unwrap();
        assert!(config.jam.hash_urls);
        assert!(JamConfig::default().hash_urls);
    }

    #[test]
    fn test_hash_urls_hocon_false() {
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(
            r#"source.dirs = ["assets"]
jam.hash-urls = false
"#,
            dir.path(),
        )
        .unwrap();
        assert!(!config.jam.hash_urls);
    }

    #[test]
    fn test_hash_urls_json_false() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("config.json"),
            r#"{
                "source": {"dirs": ["assets"]},
                "output": {
                    "base": "public",
                    "js": "public/js",
                    "css": "public/css",
                    "images": "public/images"
                },
                "manifest_path": "public/manifest.json",
                "jam": {"enabled": true, "base_path": "/paste", "hash_urls": false}
            }"#,
        )
        .unwrap();
        let config =
            Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path()).unwrap();
        assert!(!config.jam.hash_urls);

        // An old JSON config without the field keeps the default.
        fs::write(
            dir.path().join("config.json"),
            r#"{
                "source": {"dirs": ["assets"]},
                "output": {
                    "base": "public",
                    "js": "public/js",
                    "css": "public/css",
                    "images": "public/images"
                },
                "manifest_path": "public/manifest.json",
                "jam": {"enabled": true, "base_path": "/paste"}
            }"#,
        )
        .unwrap();
        let config =
            Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path()).unwrap();
        assert!(config.jam.hash_urls);
    }

    #[test]
    fn test_hash_urls_malformed_rejected() {
        let dir = tempdir().unwrap();
        let result = Config::parse_hocon(
            r#"source.dirs = ["assets"]
jam.hash-urls = maybe
"#,
            dir.path(),
        );
        assert!(
            result.is_err(),
            "a non-boolean jam.hash-urls must fail the load, not default"
        );
    }
}
