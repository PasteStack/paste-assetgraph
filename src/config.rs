use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

fn default_true() -> bool {
    true
}

fn default_output_layout() -> String {
    "flat".to_string()
}

fn default_build_dir_name() -> String {
    "_paste_build".to_string()
}

/// Keep-all is the default retention: a versioned URL minted by any earlier
/// build must keep resolving, so nothing is pruned unless the config bounds
/// it. `usize::MAX` flows through the history drain with no special case;
/// the directory-scan prune skips it, so a versioned file no manifest names
/// is left in place.
fn default_keep_versions() -> usize {
    usize::MAX
}

/// `storage.keep-versions`: `all` keeps every version a build has written —
/// the default — while a number bounds retained history per asset, `0`
/// keeping the current output only. Anything else is a config error.
fn parse_keep_versions(raw: &str) -> Result<usize> {
    match raw {
        "all" => Ok(usize::MAX),
        _ => raw.parse::<usize>().map_err(|e| {
            anyhow::anyhow!(
                "storage.keep-versions must be a non-negative integer or \"all\", got {raw:?}: {e}"
            )
        }),
    }
}

/// JSON spelling of the same rule: `"keep_versions": "all"` parses to
/// keep-all, a number bounds retention. Absent uses the same default.
mod keep_versions_json {
    use serde::de::{self, Unexpected, Visitor};
    use serde::Deserializer;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<usize, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct KeepVersions;
        impl Visitor<'_> for KeepVersions {
            type Value = usize;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a non-negative integer or \"all\"")
            }

            fn visit_u64<E>(self, value: u64) -> Result<usize, E>
            where
                E: de::Error,
            {
                usize::try_from(value)
                    .map_err(|_| E::invalid_value(Unexpected::Unsigned(value), &self))
            }

            fn visit_str<E>(self, value: &str) -> Result<usize, E>
            where
                E: de::Error,
            {
                // The same rule the flat format applies: `all` or a
                // non-negative integer, quoted or not.
                super::parse_keep_versions(value)
                    .map_err(|_| E::invalid_value(Unexpected::Str(value), &self))
            }
        }
        deserializer.deserialize_any(KeepVersions)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// `source.images` was read but never used — modelled so a JSON config
    /// still carrying the key parses and is deliberately ignored rather than
    /// denied. Flat `source.images` stays an unknown key.
    #[serde(default, skip_serializing)]
    pub images: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct JamConfig {
    /// Inert build input kept as `Option` so `validate` can warn on it:
    /// the render/combo API is always available to a runtime that loads
    /// the manifest — nothing is gated on this flag, and the build
    /// records nothing about it.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Inert build input kept as `Option` so `validate` can warn on it:
    /// the combo base path is a runtime decision — the settings'
    /// `basePath` (or `--base-path`) chooses it per request, and the build
    /// records nothing about it.
    #[serde(default)]
    pub base_path: Option<String>,
    /// Hash-qualified versioned combo URLs (`+vN.0-h<32hex>`) are emitted by
    /// default; `false` opts a build back to the legacy `+vN.0` token shape.
    /// Correctly qualified URLs still resolve while the policy is off.
    #[serde(default = "default_true")]
    pub hash_urls: bool,
    /// Inert build input kept as `Option` so `validate` can warn on it:
    /// build-time automatic inlining by size was never implemented —
    /// inlining is a render-time decision under `inlineThreshold`.
    #[serde(default)]
    pub inline_threshold: Option<usize>,
}

impl Default for JamConfig {
    fn default() -> Self {
        JamConfig {
            enabled: None,
            base_path: None,
            hash_urls: true,
            inline_threshold: None,
        }
    }
}

/// Retired `cdn.fallback-to-local`: any presence of the key in a JSON
/// config — either spelling, any value including null — is a named
/// migration error at deserialize time. No value is parsed first, so
/// the rejection is about the key, not its type.
fn reject_cdn_fallback<'de, D>(deserializer: D) -> std::result::Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let _ = serde::de::IgnoredAny::deserialize(deserializer)?;
    Err(serde::de::Error::custom(
        "cdn.fallback-to-local is no longer supported: the manifest carries the configured origin as the default for emitted URLs and a runtime cdnBase overrides it; remove the key",
    ))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CdnConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
    /// Retired key, still deserialized so `validate` can reject it by name:
    /// the manifest carries the origin default and a runtime `cdnBase`
    /// overrides it — there is nothing left for a fallback flag to switch.
    /// JSON presence fails in `reject_cdn_fallback`; HOCON reaches the same
    /// rejection through the manual parser plus `validate`.
    #[serde(
        default,
        skip_serializing,
        alias = "fallback-to-local",
        deserialize_with = "reject_cdn_fallback"
    )]
    pub fallback_to_local: Option<bool>,
}

/// Legacy `templates.*` publication settings, still deserialized so
/// `validate` can reject them by name instead of dropping the keys
/// silently. Build-time page publication was removed: a build emits
/// individual versioned assets and manifest metadata only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplatesConfig {
    /// Deserialized even when absent so a `templates` object carrying
    /// only `output` still reaches `validate`'s named rejection rather
    /// than failing on a missing field.
    #[serde(default)]
    pub source: PathBuf,
    /// Rendered HTML output; falls back to `output.base` when unset.
    #[serde(default)]
    pub output: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Backend type: "local" or "dist" (the default)
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Versions the dist backend retains per asset: `all` keeps every
    /// version ever built (the default — a pinned URL stays resolvable),
    /// `0` keeps the current output only, `N` keeps the current output
    /// plus the `N` most recent versions.
    #[serde(
        default = "default_keep_versions",
        deserialize_with = "keep_versions_json::deserialize"
    )]
    pub keep_versions: usize,
}

fn default_backend() -> String {
    "dist".to_string()
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            // Retention is the default: every version ever built stays
            // servable unless a deployment opts into `local`.
            backend: default_backend(),
            keep_versions: default_keep_versions(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Source-map generation is not implemented — the field parses for
    /// compatibility but produces nothing, so it defaults off and an
    /// explicit `true` earns the build's compatibility note.
    #[serde(default)]
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
    /// Retired build-time template publication — `Some` only so
    /// `validate` can report the configured keys as a migration error.
    #[serde(default)]
    pub templates: Option<TemplatesConfig>,
}

/// Windows `canonicalize` answers verbatim paths (`\\?\C:\…`), which never
/// `starts_with` the ordinary spelling every other configured path keeps.
/// Strip the verbatim prefix back to the plain form; elsewhere a no-op.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            if rest.as_bytes().get(1) == Some(&b':') {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

impl Config {
    /// Load config with an explicit base directory for path resolution.
    /// HOCON configs expect relative paths to resolve against the config
    /// file's directory, not the process CWD — callers that only have a path
    /// resolve it against their own working directory first.
    pub fn load_with_base_dir(path: &Path, base_dir: &Path) -> Result<Self> {
        // Collapse the base once, before any join: a `--cwd ..` spelling
        // must not leak `..` into every root and so into recorded labels.
        let base_dir = &Self::normalize_path(base_dir)?;
        // Resolve config path relative to base_dir if not absolute
        let resolved_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base_dir.join(path)
        };
        let content = fs::read_to_string(&resolved_path)
            .with_context(|| format!("cannot read config {}", resolved_path.display()))?;

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
            // An empty path segment resolves to base_dir, silently widening
            // discovery to every source in the project. Reject raw list
            // entries before any path resolution can erase that distinction.
            Self::validate_path_items("source.dirs", &cfg.source.dirs)?;
            Self::validate_path_items("include_paths", &cfg.include_paths)?;
            cfg.source.dirs = cfg
                .source
                .dirs
                .iter()
                .map(|p| Self::resolve_path(base_dir, p))
                .collect();
            for slot in [&mut cfg.source.js, &mut cfg.source.scss] {
                if slot.to_string_lossy().trim().is_empty() {
                    *slot = PathBuf::new();
                } else {
                    *slot = Self::resolve_path(base_dir, slot);
                }
            }
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
        Self::validate_flat_syntax(content)?;
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
                .map(|s| {
                    s.parse::<bool>()
                        .with_context(|| format!("{key} must be true or false, got {s:?}"))
                })
                .transpose()?
                .unwrap_or(fallback))
        };

        // HOCON last-value-wins applies to arrays too: only the final match
        // is read, and a malformed final value is an error, not a fallback.
        let get_array = |key: &str| -> Result<Vec<String>> {
            match get_raw(key) {
                Some(value) => Self::config_array(&value, key),
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
            // Source maps are not implemented — the key parses so a config
            // that sets it can get an explicit diagnostic from `validate`.
            source_maps: get_bool("source-maps", false)?,
            source: SourcePaths {
                dirs: get_array("source.dirs")?
                    .iter()
                    .map(|p| base_dir.join(p))
                    .collect(),
                // Unset legacy roots stay empty — `source_dirs` treats empty
                // as "not configured" and only falls back to the defaults
                // when no source root was configured at all.
                // An empty value is unset, as in JSON — joining "" would
                // silently widen discovery to the whole project.
                js: get_value("source.js")?
                    .filter(|v| !v.trim().is_empty())
                    .map(|v| base_dir.join(v))
                    .unwrap_or_default(),
                scss: get_value("source.scss")?
                    .filter(|v| !v.trim().is_empty())
                    .map(|v| base_dir.join(v))
                    .unwrap_or_default(),
                // The flat grammar accepts `source.images` (see KNOWN_KEYS)
                // but never reads it, matching the ignored JSON field.
                images: None,
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
                backend: get_value("storage.backend")?.unwrap_or_else(|| "dist".into()),
                keep_versions: get_value("storage.keep-versions")?
                    .map(|v| parse_keep_versions(&v))
                    .transpose()?
                    .unwrap_or_else(default_keep_versions),
            },
            jam: JamConfig {
                enabled: get_value("jam.enabled")?
                    .map(|v| {
                        v.parse::<bool>().with_context(|| {
                            format!("jam.enabled must be true or false, got {v:?}")
                        })
                    })
                    .transpose()?,
                base_path: get_value("jam.base-path")?,
                hash_urls: get_bool("jam.hash-urls", true)?,
                inline_threshold: get_value("jam.inline-threshold")?
                    .map(|v| {
                        v.parse::<usize>().with_context(|| {
                            format!(
                                "jam.inline-threshold must be a non-negative integer, got {v:?}"
                            )
                        })
                    })
                    .transpose()?,
            },
            cdn: CdnConfig {
                enabled: get_bool("cdn.enabled", false)?,
                url: get_value("cdn.url")?.unwrap_or_default(),
                // The retired key's presence is what `validate` reports —
                // its value no longer means anything.
                fallback_to_local: get_value("cdn.fallback-to-local")?.map(|_| true),
            },
            // Either retired key is still read into the config so
            // `validate` can name it in the migration error; silently
            // ignoring `templates.output` alone would hide a half-removed
            // publication setting.
            templates: {
                let source = get_value("templates.source")?;
                let output = get_value("templates.output")?;
                if source.is_none() && output.is_none() {
                    None
                } else {
                    Some(TemplatesConfig {
                        source: source.map(|s| base_dir.join(s)).unwrap_or_default(),
                        output: output.map(|o| base_dir.join(o)).unwrap_or(output_base),
                    })
                }
            },
        };
        let mut config = config;
        config.normalize_paths()?;
        config.validate()?;
        Ok(config)
    }

    /// The named migration errors for the retired keys, keyed by the
    /// config token that carried them — block and colon spellings get the
    /// same message as a flat assignment so a half-migrated file still
    /// names what to remove.
    fn retired_key_error(key: &str) -> Option<String> {
        if key == "templates" || key.starts_with("templates.") {
            Some(
                "templates.source/templates.output are no longer supported: build-time page publication was removed — the build emits individual versioned assets and manifest metadata only; remove the keys and serve pages through the render/combo API"
                    .to_string(),
            )
        } else if key == "cdn.fallback-to-local" || key == "fallback-to-local" {
            Some(
                "cdn.fallback-to-local is no longer supported: the manifest carries the configured origin as the default for emitted URLs and a runtime cdnBase overrides it; remove the key"
                    .to_string(),
            )
        } else {
            None
        }
    }

    /// The complete flat-key grammar: every spelling the parser reads.
    /// Anything else on an assignment line is a configuration error —
    /// a misspelling must never fall through to a silent default.
    /// Retired keys are absent from this list by design: they fail
    /// earlier with their own named migration errors.
    const KNOWN_KEYS: &[&str] = &[
        "debug-mode",
        "minify",
        "source-maps",
        "source.dirs",
        "source.js",
        "source.scss",
        // Read but never used, like the JSON field: a legacy config that
        // still carries `source.images` loads rather than failing.
        "source.images",
        "include-paths",
        "output.base",
        "output.js",
        "output.css",
        "output.images",
        "output.layout",
        "output.build-dir-name",
        "manifest",
        "storage.backend",
        "storage.keep-versions",
        "jam.enabled",
        "jam.base-path",
        "jam.hash-urls",
        "jam.inline-threshold",
        "cdn.enabled",
        "cdn.url",
    ];

    /// The config grammar is flat `key = value` lines plus `#` and `//`
    /// comments — the parser never learned HOCON blocks, `:` separators,
    /// objects, or substitution. Validate every line up front so a
    /// malformed file fails the load with the line number and the
    /// supported spelling instead of silently defaulting the keys the
    /// misspelling hid.
    fn validate_flat_syntax(content: &str) -> Result<()> {
        for (index, raw) in content.lines().enumerate() {
            let line_no = index + 1;
            let line = Self::strip_inline_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            // A key is `[A-Za-z0-9]` followed by `[A-Za-z0-9._-]*` — the
            // token every `key = value` line opens with.
            let key_len = line
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
                .unwrap_or(line.len());
            let key = &line[..key_len];
            // A retired key keeps its named migration error whatever shape
            // the line takes — a block or colon spelling is still the
            // retired setting, not a syntax error to decode.
            if let Some(message) = Self::retired_key_error(key) {
                anyhow::bail!("{message}");
            }
            let fail = |detail: &str, fix: &str| -> anyhow::Error {
                anyhow::anyhow!(
                    "unsupported config syntax at line {line_no}: {line:?} — {detail}; {fix}"
                )
            };
            if key_len == 0 {
                return Err(fail(
                    "expected a `key = value` assignment",
                    "write one `key = value` per line",
                ));
            }
            if Self::has_unquoted_brace(line) {
                return Err(fail(
                    "nested blocks and objects are not supported",
                    "flatten to dotted `key = value` lines",
                ));
            }
            match line[key_len..].trim_start().chars().next() {
                Some('=') => {}
                Some(':') => {
                    return Err(fail(
                        "this grammar takes `=` separators, not `:`",
                        &format!("write `{key} = …`"),
                    ))
                }
                _ => {
                    return Err(fail(
                        "missing `=` after the key",
                        &format!("write `{key} = …`"),
                    ))
                }
            }
            if !Self::KNOWN_KEYS.contains(&key) {
                anyhow::bail!(
                    "unknown config key {key:?} at line {line_no} — the supported keys are: {}",
                    Self::KNOWN_KEYS.join(", ")
                );
            }
        }
        Ok(())
    }

    /// Structural braces are unsupported, but braces inside a quoted
    /// path are ordinary data. Scan bytes so an escaped quote cannot
    /// accidentally terminate the quoted span.
    fn has_unquoted_brace(value: &str) -> bool {
        let bytes = value.as_bytes();
        let mut quoted = false;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if quoted && i + 1 < bytes.len() => i += 2,
                b'"' => {
                    quoted = !quoted;
                    i += 1;
                }
                b'{' | b'}' if !quoted => return true,
                _ => i += 1,
            }
        }
        false
    }

    /// Strip a trailing `#` or `//` comment. A marker counts only in
    /// comment position — at the value's start or following whitespace —
    /// so `//` inside an unquoted token (`https://cdn.example/…`) and `#`
    /// inside an unquoted scalar are data, not comment starts. Quoted
    /// strings and their escapes are always preserved intact. The same
    /// rule applies to whole lines and to the value half of an
    /// assignment, so a line never parses under a different reading than
    /// it validates under.
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
                let comment_position = i == 0 || bytes[i - 1].is_ascii_whitespace();
                if bytes[i] == b'#' && comment_position {
                    return value[..i].trim_end();
                }
                if bytes[i] == b'/'
                    && comment_position
                    && i + 1 < bytes.len()
                    && bytes[i + 1] == b'/'
                {
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
    fn config_array(value: &str, key: &str) -> Result<Vec<String>> {
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
                anyhow::ensure!(
                    !token.is_empty(),
                    "{key}[{}] is empty — array entries must name a path",
                    items.len()
                );
                items.push(Self::config_string(token)?);
                start = index + 1;
            }
        }
        anyhow::ensure!(!in_quotes, "unterminated quote in array {:?}", value);
        anyhow::ensure!(!escaped, "unterminated escape in array {:?}", value);
        let token = inner[start..].trim();
        if !token.is_empty() {
            items.push(Self::config_string(token)?);
        } else if !inner.trim().is_empty() {
            anyhow::bail!(
                "{key}[{}] is empty — trailing array separator is not permitted",
                items.len()
            );
        }
        // Empty arrays have existing fallback semantics; individual empty
        // strings do not. Reject quoted blanks even when the list token
        // itself is syntactically nonempty.
        for (index, item) in items.iter().enumerate() {
            anyhow::ensure!(
                !item.trim().is_empty(),
                "{key}[{index}] is empty — array entries must name a path"
            );
        }
        Ok(items)
    }

    fn validate_path_items(key: &str, paths: &[PathBuf]) -> Result<()> {
        for (index, path) in paths.iter().enumerate() {
            anyhow::ensure!(
                !path.as_os_str().is_empty() && !path.to_string_lossy().trim().is_empty(),
                "{key}[{index}] is empty — array entries must name a path"
            );
        }
        Ok(())
    }

    /// Resolve a path against base_dir if it is relative; return as-is if absolute.
    fn resolve_path(base_dir: &Path, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_dir.join(p)
        }
    }

    /// Absolutize then normalize a path — a relative path resolves
    /// against the process working directory, then `.`/`..` collapse
    /// component-wise. `..` is interpreted in filesystem order: the
    /// accumulated prefix resolves through `canonicalize` first, because
    /// a symlinked ancestor makes `link/../x` land somewhere a lexical
    /// pop would never reach. A prefix the filesystem cannot resolve
    /// yet falls back to the lexical step, so missing directories stay
    /// legal — and a symlinked root keeps its entry spelling for watch
    /// mode, which must observe the link itself being retargeted.
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
                    if let Ok(resolved) = std::fs::canonicalize(&out) {
                        out = without_verbatim_prefix(resolved);
                    }
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        Ok(out)
    }

    /// The watch/traversal spelling of a configured root: absolutized and
    /// `.`-collapsed, but `..` components stay literal. Collapsing `..`
    /// through `canonicalize` bakes today's symlink targets into the path
    /// — `alias/../sources` would forget `alias` ever existed, and a later
    /// retarget has no link left to observe. Kept literal, every access
    /// re-resolves the alias and its parent stays a watch candidate for
    /// the retarget itself; physical forms are resolved separately at
    /// registration and comparison time.
    fn normalize_path_spelled(path: &Path) -> Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut out = PathBuf::new();
        for component in absolute.components() {
            match component {
                std::path::Component::CurDir => {}
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
        Self::validate_path_items("source.dirs", &self.source.dirs)?;
        Self::validate_path_items("include_paths", &self.include_paths)?;
        self.base_dir = Self::normalize_path(&self.base_dir)?;
        self.source.dirs = self
            .source
            .dirs
            .iter()
            .map(|p| Self::normalize_path_spelled(p))
            .collect::<Result<_>>()?;
        for slot in [&mut self.source.js, &mut self.source.scss] {
            if !slot.as_os_str().is_empty() {
                *slot = Self::normalize_path_spelled(slot)?;
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
            .map(|p| Self::normalize_path_spelled(p))
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
        // Programmatic configs need the same guard as both file parsers;
        // normalization would turn an empty PathBuf into the CWD.
        Self::validate_path_items("source.dirs", &self.source.dirs)?;
        Self::validate_path_items("include_paths", &self.include_paths)?;
        if let Some(templates) = &self.templates {
            anyhow::bail!(
                "templates.source/templates.output ({:?} -> {:?}) are no longer supported: build-time page publication was removed — the build emits individual versioned assets and manifest metadata only; remove the keys and serve pages through the render/combo API",
                templates.source,
                templates.output
            );
        }
        if self.cdn.fallback_to_local.is_some() {
            anyhow::bail!(
                "cdn.fallback-to-local is no longer supported: the manifest carries the configured origin as the default for emitted URLs and a runtime cdnBase overrides it; remove the key"
            );
        }
        if self.cdn.enabled && self.cdn.url.is_empty() {
            anyhow::bail!("cdn.enabled requires cdn.url");
        }
        if self.cdn.enabled {
            // The same prefix contract the renderer enforces — a build
            // must reject the spellings that would put asset paths inside
            // a query or fragment, or break out of markup context, before
            // any output or manifest is written.
            crate::jam::validate_cdn_prefix(&self.cdn.url)
                .map_err(|e| anyhow::anyhow!("cdn.url is not a valid URL prefix: {e}"))?;
        }

        match self.output.layout.as_str() {
            "flat" | "beside-source" => {}
            other => anyhow::bail!(
                "output.layout must be \"flat\" or \"beside-source\", got {:?}",
                other
            ),
        }
        let name = self.output.build_dir_name.as_str();
        // Checked on the raw string: `Path::components` drops a trailing
        // `/`, `/.` or `//`, but discovery and output placement compare the
        // name as written.
        let is_segment = matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [std::path::Component::Normal(_)]
        ) && !name.contains(['/', '\\', ':'])
            && name.trim() == name;
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
        // The same rule binds include roots: an include path inside
        // generated output is pruned by the discovery filter at depth
        // zero, so the modules it was configured to contribute would
        // vanish without any error naming why. An external tree that
        // merely contains a generated directory beneath it stays legal —
        // it is the root itself overlapping output that fails.
        for include_root in &self.include_paths {
            // Include roots keep their spelled form for watch/traversal —
            // the overlap guard must compare the resolved physical path,
            // or a `tmp/../out/x` alias slips past the check.
            let resolved = Self::normalize_path(include_root)?;
            for out_dir in &normalized_output {
                if resolved.starts_with(out_dir) {
                    anyhow::bail!(
                        "include path {:?} is inside or equal to output directory {:?} — an input root must not overlap generated output",
                        include_root,
                        out_dir
                    );
                }
            }
        }
        Ok(())
    }

    /// The compatibility notices for settings that parse but do nothing.
    /// Emitted by `AssetGraph::build` exactly once per build — they are NOT
    /// part of `validate`, which also runs at config load and would repeat
    /// every one of them.
    pub fn emit_compat_notes(&self) {
        // Explicit `source-maps = true` must not read as an active
        // feature: generation is not implemented, so the build says so
        // rather than silently producing no maps.
        if self.source_maps {
            eprintln!(
                "note: source-maps is not implemented — no .map files are emitted; remove the key"
            );
        }
        if self.jam.enabled.is_some() {
            eprintln!(
                "note: jam.enabled has no effect — the render/combo API is always available to a runtime that loads the manifest; remove the key"
            );
        }
        // Inert keys are tolerated but noticed: a config that still sets
        // them must not read as if they did something.
        if self.jam.base_path.is_some() {
            eprintln!(
                "note: jam.base-path is ignored by the build — the combo base path is a runtime setting (the settings' basePath / --base-path); remove the key"
            );
        }
        if self.jam.inline_threshold.is_some() {
            eprintln!(
                "note: jam.inline-threshold is ignored — automatic inlining by size is a render-time setting (the settings' inlineThreshold / --inline-threshold); remove the key"
            );
        }
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
            source_maps: false,
            source: SourcePaths {
                dirs: Vec::new(),
                js: base_dir.join("src/main/assets/js"),
                scss: base_dir.join("src/main/assets/scss"),
                images: None,
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
                enabled: None,
                base_path: None,
                hash_urls: true,
                inline_threshold: None,
            },
            cdn: CdnConfig {
                enabled: false,
                url: String::new(),
                fallback_to_local: None,
            },
            templates: None,
        }
    }
}

/// The configuration filename `build`/`watch`/`init` use.
pub const CONFIG_FILE_NAME: &str = "paste-assetgraph.conf";

/// The former default filename — still loaded when it is the only config
/// present, but `init` refuses to create a competing configuration
/// beside it.
pub const FORMER_CONFIG_FILE_NAME: &str = "paste-surface.conf";

/// Write a starter `paste-assetgraph.conf` for `project_type` — one of
/// `basic`, `react`, or `scala`; anything else is an error rather than a
/// silent basic scaffold. An existing config is never truncated: the
/// current file is created with `create_new` so a race or a rerun fails
/// instead of overwriting it, and a lone former-name config refuses
/// outright — two configs in one project would be ambiguous, so the
/// operator renames the old file rather than getting a second one.
pub fn init_config(project_type: &str, base_dir: &Path) -> Result<()> {
    // Use flat key=value format compatible with parse_hocon. Only live
    // settings appear — source maps and jam.enabled are not real build
    // inputs, and the runtime keys (base path, inline threshold) are
    // settings the render/combo call carries.
    let config = match project_type {
        "react" => {
            r#"# Paste AssetGraph Configuration (React/Vite)

debug-mode = false

source.dirs = ["src"]

output.base = "dist"
output.js = "dist/assets"
output.css = "dist/assets"
output.images = "dist/images"

storage.backend = "dist"
storage.keep-versions = all
manifest = "dist/manifest.json"

minify = true

jam.hash-urls = true
"#
        }
        "scala" => {
            r#"# Paste AssetGraph Configuration (Scala/Twirl)

debug-mode = false

source.dirs = ["src/main/assets/js", "src/main/assets/scss"]

output.base = "src/main/public"
output.js = "src/main/public/js"
output.css = "src/main/public/css"
output.images = "src/main/public/images"

storage.backend = "dist"
storage.keep-versions = all
manifest = "src/main/public/manifest.json"

minify = true

jam.hash-urls = true
"#
        }
        "basic" => {
            r#"# Paste AssetGraph Configuration (Basic HTML)

debug-mode = false

source.dirs = ["assets/js", "assets/scss"]

output.base = "public"
output.js = "public/js"
output.css = "public/css"
output.images = "public/images"

storage.backend = "dist"
storage.keep-versions = all
manifest = "public/manifest.json"

minify = true

jam.hash-urls = true
"#
        }
        other => anyhow::bail!(
            "unknown project type {other:?} — expected \"basic\", \"react\", or \"scala\""
        ),
    };

    let output_path = base_dir.join(CONFIG_FILE_NAME);
    let former_path = base_dir.join(FORMER_CONFIG_FILE_NAME);
    if !output_path.exists() && former_path.exists() {
        anyhow::bail!(
            "{} already exists — rename it to {} rather than creating a second configuration beside it",
            former_path.display(),
            CONFIG_FILE_NAME,
        );
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .map_err(|e| {
            let message = if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "cannot create {} — it already exists; refusing to overwrite it",
                    output_path.display()
                )
            } else {
                format!("cannot create {}", output_path.display())
            };
            anyhow::Error::new(e).context(message)
        })?;
    use std::io::Write;
    file.write_all(config.as_bytes())?;
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
        // A lookalike key is a configuration error, not silently ignored
        // input — `source.json-extra` shares a prefix with `source.js`
        // but is no key at all.
        let config_content = r#"
source.json-extra = "src/json"
source.js = "src/js"
source.scss = "src/scss"
output.base = "dist"
"#;

        let dir = tempdir().unwrap();
        let error = Config::parse_hocon(config_content, dir.path())
            .expect_err("an unknown lookalike key must fail the load");
        let message = format!("{error:#}");
        assert!(
            message.contains("source.json-extra") && message.contains("line 2"),
            "the error names the key and its line, got: {message}"
        );

        // The positive case: the exact keys alone parse and bind.
        let config = Config::parse_hocon(
            r#"
source.js = "src/js"
source.scss = "src/scss"
output.base = "dist"
"#,
            dir.path(),
        )
        .unwrap();
        let js_path = config.source.js.to_string_lossy();
        assert!(
            js_path.ends_with("src/js"),
            "source.js should be 'src/js', got: {js_path}"
        );
    }

    #[test]
    fn test_config_key_with_similar_prefix() {
        // "output.json" is not "output.js" — the lookalike is rejected
        // rather than tolerated beside the real key.
        let config_content = r#"
output.base = "dist"
output.json = "dist/json"
output.js = "dist/js"
output.css = "dist/css"
"#;

        let dir = tempdir().unwrap();
        let error = Config::parse_hocon(config_content, dir.path())
            .expect_err("an unknown lookalike key must fail the load");
        assert!(
            format!("{error:#}").contains("output.json"),
            "the error names the lookalike, got: {error:#}"
        );

        let config = Config::parse_hocon(
            r#"
output.base = "dist"
output.js = "dist/js"
output.css = "dist/css"
"#,
            dir.path(),
        )
        .unwrap();
        assert!(config.output.js.to_string_lossy().ends_with("dist/js"));
    }

    #[test]
    fn test_config_key_order_independence() {
        // Exact keys bind identically in either order; a lookalike is
        // rejected in either order too — order never rescues a bad key.
        let dir = tempdir().unwrap();
        for content in [
            "source.js = \"src/js\"\nmanifest = \"a.json\"\n",
            "manifest = \"a.json\"\nsource.js = \"src/js\"\n",
        ] {
            let config = Config::parse_hocon(content, dir.path()).unwrap();
            assert!(config.source.js.to_string_lossy().ends_with("src/js"));
        }
        for content in [
            "source.json-extra = \"x\"\nsource.js = \"src/js\"\n",
            "source.js = \"src/js\"\nsource.json-extra = \"x\"\n",
        ] {
            let error = Config::parse_hocon(content, dir.path())
                .expect_err("the lookalike fails in either position");
            assert!(format!("{error:#}").contains("source.json-extra"));
        }
    }

    #[test]
    fn test_config_unknown_keys_fail_with_name_and_line() {
        // Every misspelling that used to silently fall back to a default
        // now fails, naming the key it rejected.
        let dir = tempdir().unwrap();
        for (key, line) in [
            ("storage.backends = \"dist\"\n", "storage.backends"),
            ("source.dir = [\"src/js\"]\n", "source.dir"),
            ("output.jss = \"dist/js\"\n", "output.jss"),
            ("cdn.urls = \"https://cdn.example\"\n", "cdn.urls"),
            ("jam.hash-url = false\n", "jam.hash-url"),
            ("minifyy = true\n", "minifyy"),
        ] {
            let error = Config::parse_hocon(key, dir.path())
                .expect_err("an unknown key must fail the load");
            let message = format!("{error:#}");
            assert!(
                message.contains(line) && message.contains("line 1"),
                "{line}: got {message}"
            );
        }
        // The misspelling must not partially apply: `storage.backends`
        // beside a valid `storage.backend` still fails the whole file.
        let error = Config::parse_hocon(
            "storage.backend = \"dist\"\nstorage.backends = \"local\"\n",
            dir.path(),
        )
        .expect_err("an unknown key fails even beside valid ones");
        assert!(format!("{error:#}").contains("storage.backends"));
    }

    #[test]
    fn test_config_json_unknown_fields_fail() {
        // serde's deny_unknown_fields: a misspelt or foreign key fails the
        // load at every level of the JSON shape, with the field named.
        let dir = tempdir().unwrap();
        let base = |extra: &str| {
            format!(
                r#"{{
                    "source": {{ "js": "src/js", "scss": "src/scss" }},
                    "output": {{ "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" }},
                    "manifest_path": "dist/manifest.json"{extra}
                }}"#
            )
        };
        for (content, expected) in [
            (base(r#", "minifyy": true"#), "minifyy"),
            (base(r#", "storage": { "backend": "dist", "backends": "x" }"#), "backends"),
            (base(r#", "cdn": { "enabled": true, "urls": "x" }"#), "urls"),
            (
                r#"{
                    "source": { "js": "src/js", "scss": "src/scss", "dirs2": ["x"] },
                    "output": { "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" },
                    "manifest_path": "dist/manifest.json"
                }"#
                .to_string(),
                "dirs2",
            ),
            (
                r#"{
                    "source": { "js": "src/js", "scss": "src/scss" },
                    "output": { "base": "dist", "jss": "d", "js": "dist/js", "css": "dist/css", "images": "dist/images" },
                    "manifest_path": "dist/manifest.json"
                }"#
                .to_string(),
                "jss",
            ),
        ] {
            let path = dir.path().join("config.json");
            fs::write(&path, content).unwrap();
            let error = Config::load_with_base_dir(&path, dir.path())
                .expect_err("an unknown JSON field must fail the load");
            assert!(
                format!("{error:#}").contains(expected),
                "{expected}: got {error:#}"
            );
        }
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
        // Array keys obey the same strict grammar: `include-paths-extra`
        // is a lookalike, not an ignored extra list.
        let config_content = r#"
include-paths-extra = ["should-not-match"]
include-paths = ["correct-path"]
"#;

        let dir = tempdir().unwrap();
        let error = Config::parse_hocon(config_content, dir.path())
            .expect_err("a lookalike array key must fail the load");
        assert!(format!("{error:#}").contains("include-paths-extra"));

        let config = Config::parse_hocon(
            r#"
include-paths = ["correct-path"]
"#,
            dir.path(),
        )
        .unwrap();
        assert_eq!(config.include_paths.len(), 1);
        assert!(config.include_paths[0]
            .to_string_lossy()
            .contains("correct-path"));
    }

    #[test]
    fn test_config_rejects_empty_path_array_entries_and_positions() {
        let dir = tempdir().unwrap();
        for (key, declaration, position) in [
            ("source.dirs", "[,,]", 0),
            ("source.dirs", "[\"src/js\",,\"src/scss\"]", 1),
            ("source.dirs", "[,\"src/js\"]", 0),
            ("source.dirs", "[\"src/js\",]", 1),
            ("source.dirs", "[\"\"]", 0),
            ("source.dirs", "[\"   \"]", 0),
            ("include-paths", "[\"\"]", 0),
            ("include-paths", "[\"src\",\"   \"]", 1),
        ] {
            let error = Config::parse_hocon(&format!("{key} = {declaration}"), dir.path())
                .expect_err("malformed path array must be rejected");
            let message = format!("{error:#}");
            assert!(
                message.contains(&format!("{key}[{position}]")),
                "{declaration}: {message}"
            );
        }
        let empty = Config::parse_hocon("source.dirs = []", dir.path()).unwrap();
        assert!(empty.source.dirs.is_empty());
        assert_eq!(empty.source_dirs().len(), 2);
        let intentional = Config::parse_hocon("source.dirs = [\".\"]", dir.path()).unwrap();
        assert_eq!(intentional.source_dirs(), vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn test_json_and_programmatic_empty_path_items_fail_before_resolution() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        for (source, includes, key) in [
            (r#"[""]"#, r#"[]"#, "source.dirs[0]"),
            (r#"[" "]"#, r#"[]"#, "source.dirs[0]"),
            (r#"["assets"]"#, r#"[""]"#, "include_paths[0]"),
        ] {
            fs::write(&path, format!(r#"{{
                "source": {{ "dirs": {source} }},
                "output": {{ "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" }},
                "manifest_path": "dist/manifest.json",
                "include_paths": {includes}
            }}"#)).unwrap();
            let error = Config::load_with_base_dir(&path, dir.path()).unwrap_err();
            assert!(format!("{error:#}").contains(key), "{error:#}");
        }
        let mut direct = Config::default_for(dir.path());
        direct.source.dirs = vec![PathBuf::new()];
        assert!(format!("{:#}", direct.validate().unwrap_err()).contains("source.dirs[0]"));
        direct.source.dirs.clear();
        direct.include_paths = vec![PathBuf::new()];
        assert!(format!("{:#}", direct.validate().unwrap_err()).contains("include_paths[0]"));
    }

    #[test]
    fn test_quoted_braces_are_path_data_not_nested_objects() {
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(
            r#"output.base = "dist/{preview}"
source.dirs = ["assets/{v1}", "src/quoted\\\"{v2}\\\""]
"#,
            dir.path(),
        )
        .unwrap();
        assert_eq!(config.output.base, dir.path().join("dist/{preview}"));
        assert_eq!(config.source.dirs[0], dir.path().join("assets/{v1}"));
        assert_eq!(
            config.source.dirs[1],
            dir.path().join("src/quoted\\\"{v2}\\\"")
        );
        for illegal in [
            "output { base = \"dist\" }",
            "output.base = { value: \"dist\" }",
        ] {
            let error = Config::parse_hocon(illegal, dir.path()).unwrap_err();
            assert!(format!("{error:#}").contains("nested blocks and objects"));
        }
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
        assert_eq!(
            config.jam.enabled,
            Some(true),
            "jam.enabled records the explicit setting"
        );
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
        assert_eq!(config.jam.enabled, Some(false));
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
            config.storage.keep_versions,
            usize::MAX,
            "keep_versions should default to keep-all"
        );
        assert_eq!(
            config.jam.enabled, None,
            "jam.enabled is unset unless configured"
        );
        assert!(
            !config.source_maps,
            "source maps are unimplemented — the default stays false"
        );
        assert!(!config.cdn.enabled, "cdn.enabled should default to false");
    }

    #[test]
    fn test_config_defaults_retain_every_version() {
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon("", dir.path()).unwrap();
        assert_eq!(config.storage.backend, "dist");
        assert_eq!(config.storage.keep_versions, usize::MAX);
        assert_eq!(StorageConfig::default().backend, "dist");
    }

    #[test]
    fn test_config_empty_legacy_source_roots_are_unset_in_both_formats() {
        let dir = tempdir().unwrap();
        let hocon =
            Config::parse_hocon("source.js = \"\"\nsource.scss = \" \"\n", dir.path()).unwrap();
        assert!(hocon.source.js.as_os_str().is_empty());
        assert!(hocon.source.scss.as_os_str().is_empty());
        std::fs::write(
            dir.path().join("c.json"),
            r#"{
                "source": {"js": "", "scss": " "},
                "output": {"base": "public", "js": "public/js", "css": "public/css", "images": "public/images"},
                "manifest_path": "public/manifest.json",
                "storage": {"backend": "dist", "keep_versions": "all"}
            }"#,
        )
        .unwrap();
        let json = Config::load_with_base_dir(Path::new("c.json"), dir.path()).unwrap();
        assert!(json.source.js.as_os_str().is_empty());
        assert!(json.source.scss.as_os_str().is_empty());
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
        // Minify defaults on; source maps stay off — generation is not
        // implemented, so an unset key must not read as an active feature.
        let config_content = r#"
output.base = "dist"
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(config.minify, "minify should default to true");
        assert!(
            !config.source_maps,
            "source_maps should default to false — maps are not emitted"
        );
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
output.base = "target/public"
output.js = "target/public/js"
output.css = "target/public/css"
output.images = "target/public/images"
manifest = "target/paste-assetgraph-manifest.json"
include-paths = ["../paste-elements", "../other-lib"]
debug-mode = false
minify = true
storage.backend = "local"
storage.keep-versions = all
jam.base-path = "/paste"
jam.hash-urls = true
"#;

        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        // Verify all fields are parsed correctly
        assert!(!config.debug_mode);
        assert!(config.minify);
        assert!(!config.source_maps);
        assert_eq!(config.jam.enabled, None);
        assert_eq!(config.jam.base_path.as_deref(), Some("/paste"));
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
    fn test_include_path_elements_may_contain_equals_signs() {
        let config_content = r#"include-paths = ["path=with=equals", "b"]"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();
        assert_eq!(
            config.include_paths,
            vec![dir.path().join("path=with=equals"), dir.path().join("b"),]
        );
    }

    #[test]
    fn test_config_still_loads_when_source_images_is_present() {
        // `source.images` is no longer a config field, but a config that
        // still carries the key loads in both parsers.
        let dir = tempdir().unwrap();
        let hocon = Config::parse_hocon(
            r#"
source.js = "src/js"
source.scss = "src/scss"
source.images = "src/images"
manifest = "public/manifest.json"
"#,
            dir.path(),
        )
        .unwrap();
        assert!(hocon.source.js.to_string_lossy().ends_with("src/js"));

        let json_path = dir.path().join("config.json");
        fs::write(
            &json_path,
            r#"{
                "source": { "js": "src/js", "scss": "src/scss", "images": "src/images" },
                "output": { "base": "public", "js": "public/js", "css": "public/css", "images": "public/images" },
                "manifest_path": "public/manifest.json"
            }"#,
        )
        .unwrap();
        let json = Config::load_with_base_dir(&json_path, dir.path()).unwrap();
        assert!(json.source.js.to_string_lossy().ends_with("src/js"));
    }

    #[test]
    fn test_config_value_with_equals_sign() {
        // Values containing '=' should not be truncated. cdn.url is no
        // longer a safe carrier — a query-string prefix is rejected — so
        // a free-form path field exercises the parser rule.
        let config_content = r#"
output.base = "dist/assets=v2&deep"
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();
        assert!(
            config
                .output
                .base
                .to_string_lossy()
                .ends_with("dist/assets=v2&deep"),
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
        // # inside quotes is NOT a comment — exercised through a free-form
        // path field since a cdn.url carrying a fragment is now rejected.
        let config_content = r#"
manifest = "dist/#snapshot.json"
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(
            config
                .manifest_path
                .to_string_lossy()
                .ends_with("dist/#snapshot.json"),
            "# inside quotes should NOT be treated as comment"
        );
    }

    // ============================================================
    // cdn.fallback-to-local is retired and rejected by name
    // ============================================================

    #[test]
    fn test_config_cdn_fallback_to_local_rejected_hocon() {
        let config_content = r#"
cdn.enabled = true
cdn.url = "https://cdn.example.com"
cdn.fallback-to-local = false
"#;
        let dir = tempdir().unwrap();
        let error = Config::parse_hocon(config_content, dir.path())
            .expect_err("cdn.fallback-to-local must fail validation");
        assert!(
            format!("{error:#}").contains("cdn.fallback-to-local"),
            "error should name the retired key, got: {error:#}"
        );
    }

    #[test]
    fn test_config_cdn_fallback_to_local_rejected_json() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("config.json");
        std::fs::write(
            &json_path,
            r#"{
                "source": { "js": "src/js", "scss": "src/scss", "images": "src/images" },
                "output": { "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" },
                "manifest_path": "dist/manifest.json",
                "cdn": { "enabled": true, "url": "https://cdn.example.com", "fallback_to_local": true }
            }"#,
        )
        .unwrap();

        let error = Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path())
            .expect_err("a cdn.fallback_to_local key must fail validation");
        assert!(
            format!("{error:#}").contains("cdn.fallback-to-local"),
            "error should name the retired key, got: {error:#}"
        );
    }

    #[test]
    fn test_config_cdn_fallback_to_local_rejected_json_all_values_and_spellings() {
        // Presence is the rejection — null, false, true, and a string all
        // fail under both the `fallback_to_local` field name and the
        // `fallback-to-local` alias, before any value typing matters.
        let dir = tempdir().unwrap();
        for key in ["fallback_to_local", "fallback-to-local"] {
            for value in ["null", "false", "true", "\"no\""] {
                let json_path = dir.path().join("config.json");
                std::fs::write(
                    &json_path,
                    format!(
                        r#"{{
                            "source": {{ "js": "src/js", "scss": "src/scss", "images": "src/images" }},
                            "output": {{ "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" }},
                            "manifest_path": "dist/manifest.json",
                            "cdn": {{ "enabled": true, "url": "https://cdn.example.com", "{key}": {value} }}
                        }}"#
                    ),
                )
                .unwrap();
                let error =
                    Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path())
                        .expect_err("{key}={value} must fail");
                assert!(
                    format!("{error:#}").contains("cdn.fallback-to-local"),
                    "{key}={value}: error should name the retired key, got: {error:#}"
                );
            }
        }
    }

    #[test]
    fn test_config_cdn_fallback_to_local_absent_survives_a_roundtrip() {
        // The field is skipped on serialize and defaults to absent on
        // read, so a config without the key writes nothing and loads
        // cleanly — the tombstone only guards presence.
        let config = Config::default_for(tempdir().unwrap().path());
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("fallback"),
            "the retired key must not serialize, got: {json}"
        );
        let loaded: Config = serde_json::from_str(&json).unwrap();
        assert!(loaded.cdn.fallback_to_local.is_none());
    }

    #[test]
    fn test_config_cdn_enabled_requires_url() {
        let config_content = r#"
cdn.enabled = true
"#;
        let dir = tempdir().unwrap();
        let error = Config::parse_hocon(config_content, dir.path())
            .expect_err("cdn.enabled without cdn.url must fail validation");
        assert!(
            format!("{error:#}").contains("cdn.url"),
            "error should name cdn.url, got: {error:#}"
        );
    }

    #[test]
    fn test_config_cdn_url_rejects_bare_relative_prefixes() {
        // `cdn/js/…` resolves against each requesting document's own
        // path — a relative prefix can never fetch from one origin. The
        // build validator shares the renderer's prefix contract, so the
        // bad spellings fail here before any output or manifest is
        // written.
        let dir = tempdir().unwrap();
        for bad in ["cdn", "assets/x", "static"] {
            let content = format!("cdn.enabled = true\ncdn.url = {bad}\n");
            let error = Config::parse_hocon(&content, dir.path())
                .expect_err("a relative cdn.url must fail validation");
            assert!(
                format!("{error:#}").contains("cdn.url"),
                "{bad} — got: {error:#}"
            );
        }
        // Root-relative and origin prefixes still load — a protocol-
        // relative value is quoted because `//` at value start is comment
        // position.
        for good in ["/assets", "https://cdn.example/assets"] {
            let content = format!("cdn.enabled = true\ncdn.url = {good}\n");
            Config::parse_hocon(&content, dir.path())
                .unwrap_or_else(|e| panic!("{good} must load: {e:#}"));
        }
        Config::parse_hocon(
            "cdn.enabled = true\ncdn.url = \"//cdn.example/x\"\n",
            dir.path(),
        )
        .expect("a quoted protocol-relative cdn.url must load");
    }

    #[test]
    fn test_config_cdn_url_rejects_malformed_authorities_at_load() {
        // The authority carries syntax, not just presence: a bad or
        // out-of-range port, userinfo, and a doubled-dot host each join
        // into asset URLs that fetch nothing — reject them at load,
        // before any build output or manifest write.
        let dir = tempdir().unwrap();
        for bad in [
            "https://cdn.example:bad/assets",
            "\"//cdn.example:bad/assets\"",
            "https://cdn.example:99999/assets",
            "\"https://user@cdn.example/x\"",
            "https://cdn..example/x",
            "https://cdn.%30/x",
            "\"//cdn.%2eexample/x\"",
        ] {
            let content = format!("cdn.enabled = true\ncdn.url = {bad}\n");
            let error = Config::parse_hocon(&content, dir.path())
                .expect_err("a malformed cdn.url authority must fail validation");
            assert!(
                format!("{error:#}").contains("cdn.url"),
                "{bad} — got: {error:#}"
            );
        }
        // Enabled is required — a malformed url under cdn.enabled=false
        // stays inert per the existing configuration contract.
        Config::parse_hocon("cdn.url = https://cdn.example:bad/assets\n", dir.path())
            .expect("a disabled cdn.url is not validated");
        // Valid authorities still load.
        for good in ["http://localhost:8080/cdn", "\"https://[::1]:8080/x\""] {
            let content = format!("cdn.enabled = true\ncdn.url = {good}\n");
            Config::parse_hocon(&content, dir.path())
                .unwrap_or_else(|e| panic!("{good} must load: {e:#}"));
        }
    }

    #[test]
    fn test_config_cdn_url_unquoted_is_not_truncated_by_comment_scanning() {
        // `//` inside an unquoted value is data — `https://cdn.example`
        // must arrive whole, not truncated to `https:`. The same scanner
        // reads the line for validation and the value for parsing, so the
        // two interpretations cannot diverge.
        let dir = tempdir().unwrap();
        for (content, expected) in [
            (
                "cdn.enabled = true\ncdn.url = https://cdn.example/assets\n",
                "https://cdn.example/assets",
            ),
            (
                "cdn.enabled = true\ncdn.url = \"https://cdn.example/assets\"\n",
                "https://cdn.example/assets",
            ),
            (
                "cdn.enabled = true\ncdn.url = http://cdn.example:8080/x\n",
                "http://cdn.example:8080/x",
            ),
            (
                "cdn.enabled = true\ncdn.url = https://cdn.example/x // the origin\n",
                "https://cdn.example/x",
            ),
            (
                "cdn.enabled = true\ncdn.url = https://cdn.example/x # the origin\n",
                "https://cdn.example/x",
            ),
            (
                // `//` at value start is comment position — a
                // protocol-relative origin must be quoted.
                "cdn.enabled = true\ncdn.url = \"//cdn.example/x\"\n",
                "//cdn.example/x",
            ),
        ] {
            let config = Config::parse_hocon(content, dir.path())
                .unwrap_or_else(|e| panic!("{content:?} must parse: {e:#}"));
            assert_eq!(
                config.cdn.url, expected,
                "{content:?} must produce the complete URL"
            );
        }

        // A truncated or authority-less origin fails before any build
        // output could be written — validation runs at load.
        for content in [
            "cdn.enabled = true\ncdn.url = https:\n",
            "cdn.enabled = true\ncdn.url = http:\n",
            "cdn.enabled = true\ncdn.url = \"https:\"\n",
            "cdn.enabled = true\ncdn.url = https://\n",
            "cdn.enabled = true\ncdn.url = https:///x\n",
        ] {
            let error = Config::parse_hocon(content, dir.path())
                .expect_err("an incomplete origin must fail at load");
            assert!(
                format!("{error:#}").contains("cdn.url"),
                "{content:?}: got {error:#}"
            );
        }
    }

    #[test]
    fn test_config_comment_position_rule_is_one_rule_for_line_and_value() {
        // `#` inside an unquoted token is data under the same rule that
        // keeps `https://` whole — comment position means line start or
        // after whitespace, in both the syntax scan and the value decode.
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(
            "manifest = dist/#snapshot.json\noutput.base = dist/x#y\n",
            dir.path(),
        )
        .unwrap();
        assert!(config
            .manifest_path
            .to_string_lossy()
            .ends_with("dist/#snapshot.json"));
        assert!(config.output.base.to_string_lossy().ends_with("dist/x#y"));

        // A full-line comment and a whitespace-separated comment still
        // count as comments.
        let config = Config::parse_hocon(
            "// leading comment\noutput.base = dist // note\nmanifest = m.json # note\n",
            dir.path(),
        )
        .unwrap();
        assert!(config.output.base.to_string_lossy().ends_with("dist"));
        assert!(config.manifest_path.to_string_lossy().ends_with("m.json"));
    }

    // ============================================================
    // strip_inline_comment handles escaped quotes
    // ============================================================

    #[test]
    fn test_config_escaped_quote_in_value() {
        // Escaped quotes inside a value should not break comment
        // stripping — on a free-form path field, since a quoted cdn.url
        // prefix is rejected by validation.
        let config_content = r#"
manifest = "dist/\"odd\" manifest.json" # comment
"#;
        let dir = tempdir().unwrap();
        let config = Config::parse_hocon(config_content, dir.path()).unwrap();

        assert!(
            config
                .manifest_path
                .to_string_lossy()
                .ends_with("dist/\"odd\" manifest.json"),
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
            "cdn": { "enabled": false, "url": "" }
        }"#;

        let json_path = dir.path().join("config.json");
        std::fs::write(&json_path, json_content).unwrap();

        let config =
            Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path()).unwrap();

        // Paths should be resolved against dir.path(), not CWD
        let base = dir.path().to_path_buf();
        assert!(
            config.source.js.starts_with(&base),
            "JSON source.js should be resolved against base_dir, got: {:?}",
            config.source.js
        );
        assert!(
            config.manifest_path.starts_with(&base),
            "JSON manifest_path should be resolved against base_dir, got: {:?}",
            config.manifest_path
        );
        // `../lib` escapes base_dir by its own text — include roots keep
        // the spelled `..` for watch/traversal, and the physical form is
        // resolved at comparison/registration time.
        assert_eq!(
            config.include_paths[0],
            base.join("../lib"),
            "JSON include_paths resolve against base_dir keeping the spelled '..'"
        );
        assert_eq!(
            Config::normalize_path(&config.include_paths[0]).unwrap(),
            std::fs::canonicalize(dir.path())
                .unwrap()
                .parent()
                .unwrap()
                .join("lib"),
            "the spelled root resolves to the real parent-relative directory"
        );
    }

    #[test]
    fn test_json_source_maps_defaults_false() {
        // The serde default must agree with the HOCON path and
        // `Config::default_for`: source-map generation is not implemented,
        // so an omitted key is `false` — a JSON author who never asked for
        // maps must not get the not-implemented notice either.
        let minimal = |extra: &str| {
            format!(
                r#"{{{}
                    "source": {{ "js": "src/js", "scss": "src/scss", "images": "src/images" }},
                    "output": {{ "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" }},
                    "manifest_path": "dist/manifest.json"
                }}"#,
                extra
            )
        };
        let config: Config = serde_json::from_str(&minimal("")).unwrap();
        assert!(
            !config.source_maps,
            "an omitted JSON source_maps must default to false"
        );
        let config: Config = serde_json::from_str(&minimal(r#""source_maps": false,"#)).unwrap();
        assert!(!config.source_maps, "explicit false stays false");
        let config: Config = serde_json::from_str(&minimal(r#""source_maps": true,"#)).unwrap();
        assert!(
            config.source_maps,
            "explicit true parses — the build emits the compatibility note"
        );
    }

    #[test]
    fn test_init_templates_parse_to_their_stated_settings() {
        // The templates are written in the same flat syntax parse_hocon reads,
        // so a key the parser does not recognize (storage.keep_versions instead
        // of storage.keep-versions) silently loses the setting. Parse each
        // template the way the CLI will.
        for project_type in ["basic", "react", "scala"] {
            // init refuses to overwrite an existing config, so each
            // template gets its own project directory.
            let dir = tempdir().unwrap();
            init_config(project_type, dir.path()).unwrap();
            let written = fs::read_to_string(dir.path().join("paste-assetgraph.conf")).unwrap();
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
                        config.storage.keep_versions,
                        usize::MAX,
                        "{project_type}: keep-versions = all should be read"
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
    // Retired templates.* keys fail as a named migration error
    // ============================================================

    #[test]
    fn test_config_templates_keys_are_rejected_hocon() {
        let dir = tempdir().unwrap();
        let migration_error = |content: &str| {
            let error = Config::parse_hocon(content, dir.path())
                .expect_err("templates keys must fail to load");
            let message = format!("{error:#}");
            assert!(
                message.contains("templates.source/templates.output"),
                "the rejection must name the retired keys: {message}"
            );
            assert!(
                message.contains("individual versioned assets"),
                "the rejection must state the new output contract: {message}"
            );
            message
        };

        // Both keys, the source key alone, and the output key alone —
        // a half-removed publication setting must not slip through.
        migration_error(
            "output.base = \"dist\"\ntemplates.source = \"pages\"\ntemplates.output = \"site\"\n",
        );
        migration_error("output.base = \"dist\"\ntemplates.source = \"pages\"\n");
        migration_error("output.base = \"dist\"\ntemplates.output = \"site\"\n");

        // Other jam settings still parse — the retired keys are the only
        // ones rejected.
        let config = Config::parse_hocon(
            "output.base = \"dist\"\njam.inline-threshold = 512\n",
            dir.path(),
        )
        .unwrap();
        assert!(config.templates.is_none());
        assert_eq!(config.jam.inline_threshold, Some(512));
    }

    #[test]
    fn test_config_templates_keys_are_rejected_json() {
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
        let error = Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path())
            .expect_err("a templates object must fail to load");
        let message = format!("{error:#}");
        assert!(
            message.contains("templates.source/templates.output"),
            "the rejection must name the retired keys: {message}"
        );

        // An output-only object reaches the same named rejection — the
        // missing `source` field must not fail deserialization first.
        fs::write(
            &json_path,
            r#"{
                "source": { "js": "src/js", "scss": "src/scss", "images": "src/images" },
                "output": { "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" },
                "manifest_path": "dist/manifest.json",
                "templates": { "output": "site" }
            }"#,
        )
        .unwrap();
        let error = Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path())
            .expect_err("an output-only templates object must fail to load");
        let message = format!("{error:#}");
        assert!(
            message.contains("templates.source/templates.output"),
            "the rejection must name the retired keys: {message}"
        );

        // JSON without the keys: templates None, threshold unset.
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
        assert_eq!(config.jam.inline_threshold, None);
        assert_eq!(config.jam.base_path.as_deref(), Some("/paste"));
    }

    #[test]
    fn test_init_writes_no_templates_keys() {
        // No scaffold may emit the retired publication keys: a generated
        // config that parse_hocon itself would reject would make init
        // produce an unloadable project.
        for project_type in ["basic", "react", "scala"] {
            let dir = tempdir().unwrap();
            init_config(project_type, dir.path()).unwrap();
            let written = fs::read_to_string(dir.path().join("paste-assetgraph.conf")).unwrap();
            assert!(
                !written.contains("templates.source") && !written.contains("templates.output"),
                "{project_type} must not write retired templates keys: {written}"
            );
            // Nor the inert runtime keys — a scaffold must not advertise a
            // setting the build ignores — nor features that do not exist:
            // source maps are unimplemented and `jam.enabled` does nothing.
            assert!(
                !written.contains("jam.base-path") && !written.contains("jam.inline-threshold"),
                "{project_type} must not write inert jam keys: {written}"
            );
            assert!(
                !written.contains("source-maps") && !written.contains("jam.enabled"),
                "{project_type} must not advertise unimplemented settings: {written}"
            );
            // Source discovery scaffolds `source.dirs`, the general root
            // list — not only the legacy per-type keys.
            assert!(
                written.contains("source.dirs"),
                "{project_type} must scaffold source.dirs: {written}"
            );
            let config = Config::parse_hocon(&written, dir.path()).unwrap();
            assert!(config.templates.is_none());
            assert!(
                !config.source_dirs().is_empty(),
                "{project_type}: the scaffold's source.dirs must resolve"
            );
        }
    }

    #[test]
    fn test_init_refuses_to_overwrite_an_existing_config() {
        let dir = tempdir().unwrap();
        init_config("basic", dir.path()).unwrap();
        let path = dir.path().join("paste-assetgraph.conf");
        let before = fs::read_to_string(&path).unwrap();

        // A rerun fails and leaves the file untouched — a scaffold must
        // never truncate a project's configuration.
        let error = init_config("react", dir.path()).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("already exists"),
            "the refusal must say why: {message}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn test_init_rejects_an_unknown_project_type() {
        let dir = tempdir().unwrap();
        // `html` was once an alias for `basic` — an undocumented type must
        // fail rather than silently scaffold the wrong shape.
        for project_type in ["React", "rails", "", "baisc", "html"] {
            let error = init_config(project_type, dir.path()).unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("unknown project type"),
                "{project_type:?} must name the error: {message}"
            );
            assert!(
                !dir.path().join("paste-assetgraph.conf").exists(),
                "{project_type:?} must not write a scaffold"
            );
        }
    }

    #[test]
    fn test_init_refuses_beside_a_former_name_config() {
        // A lone `paste-surface.conf` still loads — creating a competing
        // `paste-assetgraph.conf` beside it would leave two configs where
        // the build resolves one name first. init refuses and names the
        // rename.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(FORMER_CONFIG_FILE_NAME), "minify = true\n").unwrap();

        let error = init_config("basic", dir.path()).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains(FORMER_CONFIG_FILE_NAME) && message.contains(CONFIG_FILE_NAME),
            "the refusal must name both files: {message}"
        );
        assert!(
            !dir.path().join(CONFIG_FILE_NAME).exists(),
            "no competing config may be created"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(FORMER_CONFIG_FILE_NAME)).unwrap(),
            "minify = true\n",
            "the existing config is untouched"
        );

        // When BOTH names exist the new name already won resolution —
        // init refuses on the existing current file, not the former one.
        fs::write(dir.path().join(CONFIG_FILE_NAME), "minify = false\n").unwrap();
        let error = init_config("basic", dir.path()).unwrap_err();
        assert!(
            format!("{error:#}").contains("already exists"),
            "the existing current config is the refusal: {error:#}"
        );
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
        assert!(!config.debug_mode && config.jam.enabled.is_none() && !config.cdn.enabled);
        assert!(config.minify && !config.source_maps && config.cdn.fallback_to_local.is_none());
    }

    #[test]
    fn test_rejects_invalid_numeric_values() {
        let dir = tempdir().unwrap();
        for content in [
            "storage.keep-versions = -1",
            r#"storage.keep-versions = "abc""#,
            r#"storage.keep-versions = "everything""#,
            // The accepted vocabulary is exactly one word, lowercase.
            r#"storage.keep-versions = "ALL""#,
            "storage.keep-versions = alll",
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
        assert_eq!(config.jam.inline_threshold, Some(0));
    }

    #[test]
    fn test_keep_versions_accepts_all() {
        // `all` — bare or quoted — spells the keep-all default explicitly;
        // the JSON config accepts the same word.
        let dir = tempdir().unwrap();
        for content in [
            "storage.keep-versions = all",
            r#"storage.keep-versions = "all""#,
        ] {
            let config = Config::parse_hocon(content, dir.path()).unwrap();
            assert_eq!(
                config.storage.keep_versions,
                usize::MAX,
                "{content:?} must mean keep-all"
            );
        }

        let json = dir.path().join("config.json");
        std::fs::write(
            &json,
            r#"{
                "source": { "js": "src/js", "scss": "src/scss" },
                "output": { "base": "dist", "js": "dist/js", "css": "dist/css", "images": "dist/images" },
                "manifest_path": "dist/manifest.json",
                "storage": { "backend": "dist", "keep_versions": "all" }
            }"#,
        )
        .unwrap();
        let config =
            Config::load_with_base_dir(std::path::Path::new("config.json"), dir.path()).unwrap();
        assert_eq!(config.storage.keep_versions, usize::MAX);
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

    #[test]
    fn test_rejects_include_root_under_output() {
        let dir = tempdir().unwrap();
        // Equal and nested both fail — the discovery filter would prune
        // the include root at depth zero and every module it was
        // configured to contribute would vanish silently.
        for include in ["public", "public/vendor", "public/deep/nested"] {
            let content = format!(
                r#"source.dirs = ["src"]
include-paths = ["{include}"]
output.base = "public"
"#
            );
            let result = Config::parse_hocon(&content, dir.path());
            let message = match result {
                Err(e) => format!("{e:#}"),
                Ok(_) => panic!("include {include:?} under output must be rejected"),
            };
            assert!(message.contains("include path"), "{include}: got {message}");
            assert!(
                message.contains("output directory"),
                "{include}: got {message}"
            );
        }

        // The check applies to every output root, not just base — this
        // include sits under output.js while output.base is elsewhere.
        let result = Config::parse_hocon(
            r#"source.dirs = ["src"]
include-paths = ["public/js/vendor"]
output.base = "dist"
output.js = "public/js"
"#,
            dir.path(),
        );
        let message = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(message.contains("include path"), "got {message}");
        assert!(message.contains("public/js"), "got {message}");

        // A `..`-aliased include resolves to the same collision.
        let result = Config::parse_hocon(
            r#"source.dirs = ["src"]
include-paths = ["tmp/../public/vendor"]
output.base = "public"
"#,
            dir.path(),
        );
        let message = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(message.contains("include path"), "got {message}");

        // The inverse — generated output inside an include tree — stays
        // legal: the walk prunes the generated subtree and keeps the
        // real sources beside it.
        let config = Config::parse_hocon(
            r#"source.dirs = ["src"]
include-paths = ["shared"]
output.base = "shared/dist"
output.js = "shared/dist/js"
output.css = "shared/dist/css"
output.images = "shared/dist/images"
"#,
            dir.path(),
        );
        assert!(
            config.is_ok(),
            "output under an include root must remain legal — {:?}",
            config.err()
        );
    }

    /// `link/../x` resolves `link` before `..` — the directory the
    /// filesystem actually reaches is beside the link's target, not
    /// beside the link. A lexical collapse scans a different tree.
    /// Sources and include roots share the rule.
    #[cfg(unix)]
    #[test]
    fn test_parent_components_resolve_through_symlinked_ancestors() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        let target = elsewhere.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(elsewhere.join("sources")).unwrap();
        std::fs::create_dir_all(elsewhere.join("includes")).unwrap();
        std::fs::write(elsewhere.join("sources/app.js"), "// @module app\n").unwrap();
        symlink(&target, dir.path().join("link")).unwrap();

        let observed_elsewhere = std::fs::canonicalize(&elsewhere).unwrap();
        let config = Config::parse_hocon(
            r#"source.dirs = ["link/../sources"]
include-paths = ["link/../includes"]"#,
            dir.path(),
        )
        .unwrap();
        // The stored root keeps the spelled `..` — collapsing it through
        // canonicalize would bake today's link target into the config and
        // leave watch nothing to observe when the link retargets. The
        // filesystem resolves the live alias on every access, which is
        // also why a lexical pop would scan the wrong tree.
        assert_eq!(
            config.source_dirs(),
            vec![dir.path().join("link/../sources")],
            "the source root keeps its traversal spelling"
        );
        assert_eq!(
            std::fs::canonicalize(&config.source_dirs()[0]).ok(),
            Some(observed_elsewhere.join("sources")),
            "the spelled root resolves beside the link's target"
        );
        assert_eq!(
            config.include_paths,
            vec![dir.path().join("link/../includes")],
        );

        // A pure link keeps its spelling — watch mode must observe the
        // entry itself being retargeted, not only the resolved tree.
        let linked = Config::parse_hocon(r#"source.dirs = ["link"]"#, dir.path()).unwrap();
        assert_eq!(linked.source_dirs(), vec![dir.path().join("link")]);
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

    #[test]
    fn test_malformed_config_syntax_is_rejected() {
        let dir = tempdir().unwrap();
        for content in [
            // HOCON-style nested blocks — the inner key must not be read.
            "output {\n  base = \"dist\"\n}",
            "source {\n  js = \"assets\"\n}",
            // A stray closer.
            "}",
            // Colon separators are not this grammar.
            "output.base: \"dist\"",
            "debug-mode: false",
            // Substitution shapes a HOCON file might carry.
            "output.base \"dist\"",
            "output.base",
            // Inline objects.
            "output = { base: \"dist\" }",
            "jam = { enabled: true }",
            // Lines that are not assignments at all.
            "- just-a-list-item",
            "[\"a\", \"b\"]",
            "= \"orphan\"",
            "this line has no key",
        ] {
            let result = Config::parse_hocon(content, dir.path());
            let err = result.expect_err(&format!("{content:?} must fail to load"));
            let message = format!("{err:#}");
            assert!(
                message.contains("unsupported config syntax")
                    || message.contains("no longer supported"),
                "{content:?} must name the grammar or the retired key: {message}"
            );
            assert!(
                message.contains("line") || message.contains("no longer supported"),
                "{content:?} must carry the line number: {message}"
            );
        }
    }

    #[test]
    fn test_malformed_config_does_not_silently_default() {
        let dir = tempdir().unwrap();
        // A colon-spelled output.base must fail — not parse as the default
        // "src/main/public" while the operator's directory is ignored.
        for content in ["output.base: \"dist\"", "output {\n  base = \"dist\"\n}"] {
            assert!(
                Config::parse_hocon(content, dir.path()).is_err(),
                "{content:?} must not load with a defaulted output.base"
            );
        }
    }

    #[test]
    fn test_retired_keys_rejected_in_block_and_colon_shapes() {
        let dir = tempdir().unwrap();
        for (content, needle) in [
            (
                "templates {\n  source = \"pages\"\n}",
                "no longer supported",
            ),
            ("templates.output: \"site\"", "no longer supported"),
            ("cdn.fallback-to-local: true", "no longer supported"),
            (
                "cdn {\n  fallback-to-local: true\n}",
                "unsupported config syntax",
            ),
        ] {
            let err = Config::parse_hocon(content, dir.path())
                .expect_err(&format!("{content:?} must fail to load"));
            let message = format!("{err:#}");
            assert!(
                message.contains(needle),
                "{content:?} must fail with {needle:?}: {message}"
            );
        }
    }
}
