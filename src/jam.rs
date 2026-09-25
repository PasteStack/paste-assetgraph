//! Render-time JAM tag generation and request-time combo resolution.
//!
//! `render_all` replays a page's jam/dump events into the tags the original
//! paste emitted — the runtime bootstrap, dependency-ordered loader URLs and
//! inline groups — while `combo` resolves one combo URL (versioned,
//! `require`, or unversioned) into the response a server returns. The `render`
//! and `combo` commands funnel through here so every runtime speaks the same
//! URL contract.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::graph;
use crate::manifest::{stamp_epoch, Manifest, ResolvedVersion};

/// Asset type a jam queues or a dump/combo serves.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetType {
    Js,
    Css,
}

impl AssetType {
    pub fn ext(self) -> &'static str {
        match self {
            AssetType::Js => "js",
            AssetType::Css => "css",
        }
    }
}

/// A `jam` event: modules queued for the next dump of each type it names.
/// The same options apply to both queues — `inline` forces inlining past the
/// threshold, `sync` swaps the async loader for a script src, `media` lands
/// on the stylesheet tag.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Jam {
    #[serde(default)]
    pub css: Vec<String>,
    #[serde(default)]
    pub js: Vec<String>,
    #[serde(default)]
    pub inline: bool,
    #[serde(default)]
    pub sync: bool,
    #[serde(default)]
    pub media: Option<String>,
}

/// One render event: `{"jam": {...}}` queues modules, `{"dump": "css"}` or
/// `{"dump": "js"}` emits that type's queued tags.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Event {
    Jam(Jam),
    Dump(AssetType),
}

/// Options controlling URL shape and automatic inlining for `render_all`.
pub struct RenderOptions {
    /// Prefix for emitted URLs (a CDN origin); `Rendered::urls` always holds
    /// the path part without it.
    pub cdn_base: String,
    /// Path prefix combo URLs are built under — "/paste" by default.
    pub base_path: String,
    /// Automatic inline limit in UTF-8 bytes — 1024 by default; 0 disables
    /// automatic inlining (a jam's own `inline` flag still applies).
    pub inline_threshold: usize,
    /// Per-component byte limit for emitted URL paths. Hash-qualified module
    /// tokens are long, so a rendered combo list can exceed a filesystem's
    /// 255-byte name limit; with `Some(n)` the list continues over
    /// `/`-separated components no longer than `n`. `None` emits one
    /// component — used where the URL is never materialized as a file.
    pub url_component_limit: Option<usize>,
}

/// One dump's output: the tags plus every external URL it references.
#[derive(Debug)]
pub struct Rendered {
    pub html: String,
    /// URL paths without `cdn_base`, one per external tag emitted.
    pub urls: Vec<String>,
}

/// Where compiled files live. The build writes each asset into its type
/// directory — `config.output.js` / `config.output.css`, `base/js` /
/// `base/css` by default — while the manifest records a URL path like
/// `/js/paste/dom.h.v1.min.js`. Every read of a compiled file resolves
/// through here so a configured type directory is honoured everywhere.
#[derive(Clone)]
pub struct OutputDirs {
    pub base: PathBuf,
    pub js: PathBuf,
    pub css: PathBuf,
}

impl OutputDirs {
    /// The conventional layout: type directories directly under `base`.
    pub fn under(base: &Path) -> Self {
        OutputDirs {
            base: base.to_path_buf(),
            js: base.join("js"),
            css: base.join("css"),
        }
    }

    /// The compiled file a manifest `output` path resolves to, or None when
    /// the path is not a plain relative path — an output is a URL that lands
    /// in an HTTP response and on disk, so `..`, `.`, an empty or doubled
    /// segment (a doubled `/` would survive `Path::components` normalization
    /// and let a `js/` prefix-strip leave an absolute remainder), a
    /// backslash, or a `:` (a Windows drive/ADSM marker even on Unix, where a
    /// manifest could be produced on either) resolve to nothing rather than
    /// a file outside `base`.
    pub(crate) fn compiled_file(
        &self,
        entry_output: &str,
        asset_type: AssetType,
    ) -> Option<PathBuf> {
        let trimmed = entry_output.trim_start_matches('/');
        if trimmed.is_empty()
            || trimmed
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || trimmed.contains(['\\', ':'])
            || Path::new(trimmed)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        let type_prefix = format!("{}/", asset_type.ext());
        Some(match trimmed.strip_prefix(&type_prefix) {
            Some(rest) => match asset_type {
                AssetType::Js => self.js.join(rest),
                AssetType::Css => self.css.join(rest),
            },
            None => self.base.join(trimmed),
        })
    }
}

/// The packages segment of `packagesUrl` and the loader files under it: a
/// content fingerprint of the JS assets — the first 12 hex chars of md5
/// over each `key:hash` line, sorted by key, followed by the ordered module
/// names from `sorted_deps` so a metadata-only dependency edit that changes
/// the layering also changes the segment. Retired assets are omitted: they
/// are not part of what a fresh loader URL may serve. It changes whenever
/// any JS content or ordering changes, so a deterministic build (whose
/// manifest version is pinned) still gets a fresh segment — the
/// versioned-form URLs it feeds are cached for a year.
pub(crate) fn packages_segment(manifest: &Manifest) -> String {
    let mut pairs: Vec<(&String, &str)> = manifest
        .assets
        .iter()
        .filter(|(key, entry)| key.starts_with("js/") && !entry.retired)
        .map(|(key, entry)| (key, entry.hash.as_str()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let mut context = md5::Context::new();
    for (key, hash) in pairs {
        context.consume(format!("{key}:{hash}\n"));
    }
    if let Some(deps) = manifest.sorted_deps.get("js") {
        for (module, _, _) in deps {
            context.consume(format!("{module}\n"));
        }
    }
    let digest = format!("{:x}", context.compute());
    digest[..12].to_string()
}

/// Replay `events` and return one `Rendered` per dump, in order.
///
/// Rendering is stateless: the whole event list is replayed on every call.
/// The last event must be a dump — a trailing jam would queue modules no
/// dump ever emits.
pub fn render_all(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    events: &[Event],
) -> Result<Vec<Rendered>> {
    validate_options(opts)?;

    if !matches!(events.last(), Some(Event::Dump(_))) {
        bail!("the last render event must be a dump");
    }

    let mut loaded_js: HashSet<String> = HashSet::new();
    let mut loaded_css: HashSet<(String, Option<String>)> = HashSet::new();
    let mut pending_js: Vec<Jam> = Vec::new();
    let mut pending_css: Vec<Jam> = Vec::new();
    let mut rendered = Vec::new();

    for event in events {
        match event {
            Event::Jam(jam) => {
                if !jam.css.is_empty() {
                    pending_css.push(jam.clone());
                }
                if !jam.js.is_empty() {
                    pending_js.push(jam.clone());
                }
            }
            Event::Dump(AssetType::Css) => {
                rendered.push(dump_css(
                    manifest,
                    dirs,
                    opts,
                    &pending_css,
                    &mut loaded_css,
                )?);
                pending_css.clear();
            }
            Event::Dump(AssetType::Js) => {
                rendered.push(dump_js(manifest, dirs, opts, &pending_js, &mut loaded_js)?);
                pending_js.clear();
            }
        }
    }

    Ok(rendered)
}

pub(crate) fn validate_options(opts: &RenderOptions) -> Result<()> {
    if opts.cdn_base.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, '\'' | '"' | '<' | '>' | '`' | '\\')
    }) {
        bail!("cdn_base must not contain quotes, angle brackets, backticks, backslashes, whitespace or control characters");
    }
    validate_base_path(&opts.base_path)
}

/// `base_path` is an absolute URL path of nonempty, normal segments: it is
/// split off combo request paths verbatim, lands inside HTML attributes and
/// is embedded in the server's route pattern, so `.`/`..` segments, a
/// trailing slash, whitespace, control characters, the URL/markup
/// metacharacters and `*` (an axum wildcard that would panic route
/// registration) are all refused.
pub(crate) fn validate_base_path(base_path: &str) -> Result<()> {
    if !base_path.starts_with('/')
        || base_path
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || base_path.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(
                    c,
                    '\'' | '"' | '<' | '>' | '`' | '\\' | '?' | '#' | '%' | ':' | '*'
                )
        })
    {
        bail!("base_path must be an absolute URL path with nonempty, normal segments");
    }
    Ok(())
}

/// Dependency-first expansion of `modules` for rendering. Every requested
/// name must be a valid module name, and every module in the expanded group —
/// jammed or reached as a dependency — must have a manifest asset of the
/// dump's type; an unresolvable module is a render error.
fn expand(manifest: &Manifest, modules: &[String], asset_type: AssetType) -> Result<Vec<String>> {
    for m in modules {
        if !graph::is_module_name(m) {
            bail!("invalid module name in jam event: {}", m);
        }
    }
    let group = expand_dedup(manifest, modules, asset_type.ext());
    for m in &group {
        if manifest.asset_key_for_module(m, asset_type.ext()).is_none() {
            bail!(
                "module {} has no {} asset in the manifest",
                m,
                asset_type.ext()
            );
        }
    }
    Ok(group)
}

/// Dependencies first, first occurrence wins — the serving expansion shared
/// by the renderer and the combo resolver. The expanded set is ordered by
/// position in the manifest's `sorted_deps` layering; names absent from it
/// (cyclic modules, or a manifest written before this ordering) follow the
/// sorted ones in expansion order rather than being dropped.
fn expand_dedup(manifest: &Manifest, modules: &[String], asset_type: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut expanded = Vec::new();
    for module in modules {
        for m in manifest.get_dependencies_for_type(module, asset_type) {
            if seen.insert(m.clone()) {
                expanded.push(m);
            }
        }
    }
    let position = sorted_positions(manifest, asset_type);
    // Stable sort: the absent all share the same key and keep expansion order.
    expanded.sort_by_key(|m| position.get(m.as_str()).copied().unwrap_or(usize::MAX));
    expanded
}

fn dump_css(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    pending: &[Jam],
    loaded: &mut HashSet<(String, Option<String>)>,
) -> Result<Rendered> {
    let mut tags = Vec::new();
    let mut urls = Vec::new();
    for jam in pending {
        // Dedup is per (module, media): the same stylesheet requested with a
        // different media attribute is a genuinely different tag.
        let group: Vec<String> = expand(manifest, &jam.css, AssetType::Css)?
            .into_iter()
            .filter(|m| !loaded.contains(&(m.clone(), jam.media.clone())))
            .collect();
        if group.is_empty() {
            continue;
        }
        let (tag, url) = css_tag(manifest, dirs, opts, jam, &group)?;
        if let Some(path) = url {
            urls.push(path);
        }
        tags.push(tag);
        loaded.extend(group.into_iter().map(|m| (m, jam.media.clone())));
    }
    Ok(Rendered {
        html: tags.join("\n"),
        urls,
    })
}

fn dump_js(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    pending: &[Jam],
    loaded: &mut HashSet<String>,
) -> Result<Rendered> {
    let mut tags = Vec::new();
    let mut urls = Vec::new();

    // The runtime goes first, exactly once: the first js dump with pending
    // jams delivers `paste` itself before any module group.
    if !pending.is_empty() && !loaded.contains("paste") {
        let group: Vec<String> = expand(manifest, &["paste".to_string()], AssetType::Js)?
            .into_iter()
            .filter(|m| !loaded.contains(m))
            .collect();

        let packages = packages_segment(manifest);
        let config = format!(
            "paste.config({{packagesUrl:'{}{}/{}/'}});",
            opts.cdn_base, opts.base_path, packages
        );

        match inline_content(manifest, dirs, AssetType::Js, true, 0, &group) {
            // The runtime bootstrap always inlines when it can — the config
            // has to run after `paste` exists, so they share one script. The
            // concat separator keeps the config on its own line: a runtime
            // ending in a `//` comment (a debug build keeps comments) would
            // otherwise swallow the config call.
            Some(content) => {
                tags.push(format!(
                    "<script type=\"text/javascript\">{}{}{}</script>",
                    content,
                    graph::JS_CONCAT_SEPARATOR,
                    config
                ));
            }
            None => {
                let refs: Vec<&str> = group.iter().map(String::as_str).collect();
                let path = manifest.jam_url_with_limit(
                    &refs,
                    "js",
                    &opts.base_path,
                    false,
                    opts.url_component_limit,
                )?;
                eprintln!(
                    "warning: paste runtime could not be inlined, serving {}",
                    path
                );
                tags.push(format!(
                    "<script type=\"text/javascript\" src=\"{}{}\"></script>",
                    opts.cdn_base, path
                ));
                tags.push(format!(
                    "<script type=\"text/javascript\">{}</script>",
                    config
                ));
                urls.push(path);
            }
        }
        loaded.extend(group);
    }

    for jam in pending {
        let group: Vec<String> = expand(manifest, &jam.js, AssetType::Js)?
            .into_iter()
            .filter(|m| !loaded.contains(m))
            .collect();
        if group.is_empty() {
            continue;
        }
        let (tag, url) = js_tag(manifest, dirs, opts, jam, &group)?;
        if let Some(path) = url {
            urls.push(path);
        }
        tags.push(tag);
        loaded.extend(group);
    }

    Ok(Rendered {
        html: tags.join("\n"),
        urls,
    })
}

fn js_tag(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    jam: &Jam,
    group: &[String],
) -> Result<(String, Option<String>)> {
    if let Some(content) = inline_content(
        manifest,
        dirs,
        AssetType::Js,
        jam.inline,
        opts.inline_threshold,
        group,
    ) {
        return Ok((
            format!("<script type=\"text/javascript\">{}</script>", content),
            None,
        ));
    }

    let refs: Vec<&str> = group.iter().map(String::as_str).collect();
    let path = manifest.jam_url_with_limit(
        &refs,
        "js",
        &opts.base_path,
        false,
        opts.url_component_limit,
    )?;
    let url = format!("{}{}", opts.cdn_base, path);
    let tag = if jam.sync {
        format!("<script type=\"text/javascript\" src=\"{}\"></script>", url)
    } else {
        // The async loader marks every member as loading so paste defers
        // activation until the combo response arrives.
        let marks: Vec<String> = group
            .iter()
            .filter(|m| m.as_str() != "paste")
            .map(|m| format!("d('{}').isLoading", m))
            .collect();
        let assign = if marks.is_empty() {
            String::new()
        } else {
            format!("{}=!0;", marks.join("="))
        };
        format!(
            "<script type=\"text/javascript\">(function(d,r){{{}r('{}');}})(paste.define,paste.require);</script>",
            assign, url
        )
    };
    Ok((tag, Some(path)))
}

fn css_tag(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    jam: &Jam,
    group: &[String],
) -> Result<(String, Option<String>)> {
    let media = jam
        .media
        .as_deref()
        .map(|m| format!(" media=\"{}\"", escape_attr(m)))
        .unwrap_or_default();

    if let Some(content) = inline_content(
        manifest,
        dirs,
        AssetType::Css,
        jam.inline,
        opts.inline_threshold,
        group,
    ) {
        return Ok((
            format!("<style type=\"text/css\"{}>{}</style>", media, content),
            None,
        ));
    }

    let refs: Vec<&str> = group.iter().map(String::as_str).collect();
    let path = manifest.jam_url_with_limit(
        &refs,
        "css",
        &opts.base_path,
        false,
        opts.url_component_limit,
    )?;
    Ok((
        format!(
            "<link rel=\"stylesheet\" href=\"{}{}\"{}/>",
            opts.cdn_base, path, media
        ),
        Some(path),
    ))
}

/// The group's joined content when it may be inlined, else None.
///
/// On-disk sizes plus separator bytes bound the group before anything is
/// read, so a group already over the threshold is rejected without reading
/// it on every render. Forced inline skips the size check but never the
/// safety check: content that could end the tag early is always served
/// through a URL instead.
fn inline_content(
    manifest: &Manifest,
    dirs: &OutputDirs,
    asset_type: AssetType,
    forced: bool,
    inline_threshold: usize,
    group: &[String],
) -> Option<String> {
    let ext = asset_type.ext();
    let separator = if asset_type == AssetType::Js {
        graph::JS_CONCAT_SEPARATOR
    } else {
        "\n"
    };

    let paths: Vec<PathBuf> = group
        .iter()
        .map(|m| {
            manifest
                .asset_key_for_module(m, ext)
                .and_then(|key| manifest.assets.get(&key))
                .and_then(|e| dirs.compiled_file(&e.output, asset_type))
        })
        .collect::<Option<_>>()?;

    if !forced {
        if inline_threshold == 0 {
            return None;
        }
        let total = paths
            .iter()
            .try_fold(0u64, |sum, p| fs::metadata(p).ok().map(|m| sum + m.len()));
        let separator_bytes = separator.len() as u64 * paths.len().saturating_sub(1) as u64;
        match total {
            Some(sum) if sum + separator_bytes <= inline_threshold as u64 => {}
            _ => return None,
        }
    }

    let mut contents = Vec::with_capacity(paths.len());
    for path in &paths {
        contents.push(fs::read_to_string(path).ok()?);
    }
    let content = contents.join(separator);

    if !forced && content.len() > inline_threshold {
        return None;
    }
    if unsafe_for_inline(&content, asset_type) {
        return None;
    }
    Some(content)
}

/// Content that could break out of an inline tag. Checked case-insensitively
/// on both types — a `</SCRIPT` written in capitals escapes the tag the same
/// as a lowercase one.
fn unsafe_for_inline(content: &str, asset_type: AssetType) -> bool {
    let lower = content.to_lowercase();
    match asset_type {
        AssetType::Js => lower.contains("</script") || lower.contains("<!--"),
        AssetType::Css => lower.contains("</style"),
    }
}

/// Escape a value for a double-quoted HTML attribute.
fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The response for one combo request — also the JSON shape the `combo`
/// command prints, so thin runtimes can pass it through untouched.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComboResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub cache_control: &'static str,
    pub cacheable: bool,
    /// Redirect target for a non-canonical versioned URL — the host server
    /// writes it as the Location header on a 302.
    pub location: Option<String>,
    pub body: String,
}

fn not_found(content_type: &'static str) -> ComboResponse {
    ComboResponse {
        status: 404,
        content_type,
        cache_control: "no-cache",
        cacheable: false,
        location: None,
        body: "/* modules not found */".to_string(),
    }
}

/// 302 to the canonical URL, never cached — the client refetches and lands
/// on the immutable URL the build rendered.
fn redirect(location: String) -> ComboResponse {
    ComboResponse {
        status: 302,
        content_type: "text/plain; charset=utf-8",
        cache_control: "no-cache",
        cacheable: false,
        location: Some(location),
        body: String::new(),
    }
}

fn content_type(asset_type: AssetType) -> &'static str {
    match asset_type {
        AssetType::Js => "application/javascript; charset=utf-8",
        AssetType::Css => "text/css; charset=utf-8",
    }
}

/// Resolve one combo URL against the manifest. `path` is the request path
/// (starting with `base_path`), `query` the raw query string. The tag side
/// names the exact filtered module list, so the list is served as named —
/// dependencies expand only when the request asks (`?require` or
/// `/require/`), and `?filter=` drops names after ordering.
///
/// The versioned form — `{base_path}/{stamp}/{list}.{min.}{js|css}` — is
/// canonicalized: the stamp is the newest member's last-modified epoch
/// seconds and the list is fully pinned in dependency order (the listed
/// order for a historical pin). A URL that differs from the canonical one —
/// wrong order, missing or malformed pins, a stale or non-numeric segment —
/// is a 302 to it; a segment newer than this manifest's stamp is a 404, a
/// URL from a build this server does not have.
pub fn combo(
    manifest: &Manifest,
    dirs: &OutputDirs,
    base_path: &str,
    path: &str,
    query: Option<&str>,
) -> ComboResponse {
    // An invalid base_path cannot form a combo URL — answer like any other
    // non-combo path rather than trusting the strip below.
    if validate_base_path(base_path).is_err() {
        return not_found("text/plain; charset=utf-8");
    }
    let rest = match path
        .strip_prefix(base_path)
        .and_then(|r| r.strip_prefix('/'))
    {
        Some(rest) => rest,
        None => return not_found("text/plain; charset=utf-8"),
    };
    let segments: Vec<&str> = rest.split('/').collect();
    // A long list may continue over several path components — complete
    // `~`-joined tokens each — so the component parts are joined back into
    // one list before parsing; an empty component is rejected outright.
    let (list_part, require, versioned, segment): (String, bool, bool, Option<&str>) =
        match segments.as_slice() {
            [single] if !single.is_empty() => ((*single).to_string(), false, false, None),
            [first, rest @ ..]
                if *first == "require"
                    && !rest.is_empty()
                    && rest.iter().all(|s| !s.is_empty()) =>
            {
                (rest.join("~"), true, false, None)
            }
            [first, rest @ ..]
                if !first.is_empty() && !rest.is_empty() && rest.iter().all(|s| !s.is_empty()) =>
            {
                (rest.join("~"), false, true, Some(*first))
            }
            _ => return not_found("text/plain; charset=utf-8"),
        };

    let (list_str, asset_type, suffix) = if let Some(l) = list_part.strip_suffix(".min.js") {
        (l, AssetType::Js, ".min.js")
    } else if let Some(l) = list_part.strip_suffix(".min.css") {
        (l, AssetType::Css, ".min.css")
    } else if let Some(l) = list_part.strip_suffix(".js") {
        (l, AssetType::Js, ".js")
    } else if let Some(l) = list_part.strip_suffix(".css") {
        (l, AssetType::Css, ".css")
    } else {
        return not_found("text/plain; charset=utf-8");
    };

    // The list is percent-encoded (`site.caf%C3%A9`); `+` stays literal —
    // in the path it is the version marker, not a space.
    let Some(list_str) = percent_decode(list_str) else {
        return not_found(content_type(asset_type));
    };

    // Every list entry must name a module, optionally pinned at a major
    // version (`name+vN`, `name+vN.M`): an empty or invalid entry, a
    // malformed version token, or the same module pinned at two different
    // versions 404s the whole response rather than silently narrowing or
    // disambiguating the group.
    let mut pins: HashMap<String, Option<VersionPin>> = HashMap::new();
    let mut listed: Vec<String> = Vec::new();
    // The deduped entries as written — canonicalization compares them to the
    // pinned form the URL must carry, so a bare name redirects.
    let mut listed_raw: Vec<String> = Vec::new();
    for entry in list_str.split(['~', ',']) {
        let Some((name, pin)) = parse_list_entry(entry) else {
            return not_found(content_type(asset_type));
        };
        if !graph::is_module_name(name) {
            return not_found(content_type(asset_type));
        }
        match pins.entry(name.to_string()) {
            Entry::Occupied(slot) => {
                if *slot.get() != pin {
                    return not_found(content_type(asset_type));
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(pin);
                listed.push(name.to_string());
                listed_raw.push(entry.to_string());
            }
        }
    }

    // A supplied hash names bytes, not just a version: it must equal the
    // resolved record's hash exactly. Check every listed module now — after
    // pin parsing, before `require`/`filter` decide the served group and
    // before any redirect — so a wrong hash can never be laundered into a
    // served response or a redirect by filtering its module away.
    for (module, pin) in &pins {
        if let Some(VersionPin {
            version,
            hash: Some(hash),
        }) = pin
        {
            let Some(resolved) = manifest.resolve_version(module, asset_type.ext(), Some(*version))
            else {
                return not_found(content_type(asset_type));
            };
            if !resolved.hash.eq_ignore_ascii_case(hash) {
                return not_found(content_type(asset_type));
            }
        }
    }

    let params = query.map(parse_query).unwrap_or_default();
    let require = require || params.iter().any(|(k, _)| k == "require");
    let filter: HashSet<String> = params
        .iter()
        .filter(|(k, _)| k == "filter")
        .flat_map(|(_, v)| v.split(['~', ',']))
        .map(str::trim)
        .filter(|m| graph::is_module_name(m))
        .map(str::to_string)
        .collect();

    // Expand first so listed modules come out in dependency order; without
    // `require` the expansion only orders — it never adds modules. `filter`
    // drops names after that ordering.
    let expanded = expand_dedup(manifest, &listed, asset_type.ext());
    let ordered: Vec<String> = if require {
        expanded
    } else {
        expanded
            .into_iter()
            .filter(|m| listed.contains(m))
            .collect()
    };
    let group: Vec<String> = ordered
        .into_iter()
        .filter(|m| !filter.contains(m))
        .collect();

    let separator = if asset_type == AssetType::Js {
        graph::JS_CONCAT_SEPARATOR
    } else {
        "\n"
    };

    // Immutable caching is for the versioned form only — a `require`
    // expansion can deliver different content under the same URL.
    let cacheable = versioned && !require;

    if cacheable {
        // The runtime loader's own URL — `{packagesUrl}{module}.js` — is the
        // one unpinned list a year-cached URL may serve, and only under this
        // manifest's packages fingerprint: a loader URL rendered by another
        // build must not get this build's content. Everything else in the
        // versioned form is canonicalized first.
        let loader = asset_type == AssetType::Js
            && pins.values().all(|pin| pin.is_none())
            && segment == Some(packages_segment(manifest).as_str());
        if !loader {
            return versioned_combo(
                manifest,
                dirs,
                base_path,
                asset_type,
                VersionedRequest {
                    segment: segment.unwrap_or_default(),
                    listed_raw: &listed_raw,
                    listed: &listed,
                    pins: &pins,
                    filter: &filter,
                    query,
                    suffix,
                },
            );
        }
    }

    // Every member of the final group must resolve to a readable compiled
    // file at the version its list entry pinned — supplied hashes were
    // already checked against every listed module above pin parsing, so a
    // filtered-out wrong pin never reaches this point. A partial group, a
    // version the manifest does not know, or a pruned history file is never
    // served as a cacheable 200.
    let mut parts = Vec::new();
    for m in &group {
        let pin = pins.get(m).and_then(|p| p.as_ref());
        let Some(entry) = manifest.resolve_version(m, asset_type.ext(), pin.map(|p| p.version))
        else {
            return not_found(content_type(asset_type));
        };
        let content = dirs
            .compiled_file(entry.output, asset_type)
            .and_then(|p| fs::read_to_string(p).ok());
        match content {
            Some(c) => parts.push(c),
            None => return not_found(content_type(asset_type)),
        }
    }

    if parts.is_empty() {
        return not_found(content_type(asset_type));
    }
    ComboResponse {
        status: 200,
        content_type: content_type(asset_type),
        cache_control: if cacheable {
            "public, max-age=31536000"
        } else {
            "no-cache"
        },
        cacheable,
        location: None,
        body: parts.join(separator),
    }
}

/// A parsed versioned request — the shape the URL's path segments described,
/// before canonicalization decides whether it already is the canonical URL.
struct VersionedRequest<'a> {
    /// The `{stamp}` path segment as written.
    segment: &'a str,
    /// The list segment's entries as written — pins and all, deduplicated.
    listed_raw: &'a [String],
    /// The deduplicated module names those entries name.
    listed: &'a [String],
    /// Each listed module's `+vN[-h<hash>]` pin; `None` for a bare name.
    pins: &'a HashMap<String, Option<VersionPin>>,
    /// The `?filter=` names to drop after ordering.
    filter: &'a HashSet<String>,
    /// The request's raw query string, preserved onto a redirect.
    query: Option<&'a str>,
    /// The filename suffix (`.min.js`/`.js`, likewise for css).
    suffix: &'a str,
}

/// The canonical answer to a versioned combo URL — `{stamp}/{list}{suffix}`.
///
/// Every listed module resolves at its pin (a bare name means the current
/// version); an unknown module or a version the manifest no longer retains
/// is a no-cache 404. A `-h<32 hex>` pin must equal the resolved version's
/// recorded hash exactly — checked on every listed module in `combo` before
/// this function runs, on filtered-out members too — and a mismatch 404s
/// rather than redirecting onto different bytes. The canonical token form
/// is hash-qualified when the manifest's `hash_urls` policy is on or any
/// request pin carries a hash, so a legacy request redirects to the
/// qualified canonical URL; a qualified request never downgrades. The
/// canonical list is the listed modules in sorted_deps
/// order while every resolved version is current — a historical pin keeps
/// the listed order, the way the build that minted the URL arranged it. The
/// canonical stamp is the newest resolved version's. A request that is not
/// already that URL — wrong order, missing or malformed pins, a stale or
/// non-numeric segment — is a 302 to it with the query preserved; a segment
/// newer than the canonical stamp names a build this manifest does not have
/// and 404s. An exact canonical URL serves the resolved versions in
/// canonical order, cached for a year.
fn versioned_combo(
    manifest: &Manifest,
    dirs: &OutputDirs,
    base_path: &str,
    asset_type: AssetType,
    request: VersionedRequest<'_>,
) -> ComboResponse {
    let ext = asset_type.ext();
    let VersionedRequest {
        segment,
        listed_raw,
        listed,
        pins,
        filter,
        query,
        suffix,
    } = request;

    let mut resolved: HashMap<&str, ResolvedVersion<'_>> = HashMap::new();
    for module in listed {
        let Some(entry) =
            manifest.resolve_version(module, ext, pins[module].as_ref().map(|p| p.version))
        else {
            return not_found(content_type(asset_type));
        };
        resolved.insert(module.as_str(), entry);
    }

    // Qualified tokens carry the content hash. The manifest's build-time
    // policy is the default; any qualified pin in the request switches the
    // whole canonical form to qualified so the URL and its Location agree —
    // a correctly qualified URL keeps resolving even under an opted-out
    // manifest.
    let qualified = manifest.hash_urls || pins.values().flatten().any(|p| p.hash.is_some());

    let canonical: Vec<String> = if resolved.values().all(|entry| entry.current) {
        // All-current lists canonicalize into dependency order — the
        // manifest's layering is the one truth of what order executes.
        let position = sorted_positions(manifest, ext);
        let mut names = listed.to_vec();
        names.sort_by_key(|m| position.get(m.as_str()).copied().unwrap_or(usize::MAX));
        names
    } else {
        listed.to_vec()
    };

    let mut canonical_stamp = 0i64;
    for module in listed {
        match stamp_epoch(resolved[module.as_str()].timestamp) {
            Some(stamp) => canonical_stamp = canonical_stamp.max(stamp),
            // No stamp, no canonical URL: a manifest written before stamping
            // answers not-found until a build rewrites it.
            None => return not_found(content_type(asset_type)),
        }
    }

    // A canonical token is `name+vN.0` or `name+vN.0-h<32 hex>`; under the
    // qualified form a manifest whose records carry no valid hash cannot
    // answer at all — the unqualified token would name different bytes than
    // the request pinned.
    let mut token_suffixes: Vec<String> = Vec::with_capacity(canonical.len());
    for m in &canonical {
        let entry = &resolved[m.as_str()];
        let Ok(token_suffix) =
            crate::manifest::version_suffix(entry.version, entry.hash, qualified)
        else {
            return not_found(content_type(asset_type));
        };
        token_suffixes.push(token_suffix);
    }
    let canonical_entries: Vec<String> = canonical
        .iter()
        .zip(&token_suffixes)
        .map(|(m, s)| format!("{m}{s}"))
        .collect();
    // The Location is a header: module names can carry characters outside
    // the URL-safe set (the list arrives percent-decoded), so the URL spells
    // each name percent-encoded — the pins and `~` separators stay literal.
    let canonical_url = format!(
        "{base_path}/{canonical_stamp}/{}{suffix}",
        canonical
            .iter()
            .zip(&token_suffixes)
            .map(|(m, s)| format!("{}{}", url_encode_module(m), s))
            .collect::<Vec<String>>()
            .join("~")
    );
    let location = match query {
        Some(query) => format!("{canonical_url}?{query}"),
        None => canonical_url,
    };

    // The segment must be a plain epoch — the original's digits check.
    let requested_stamp = if !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit()) {
        segment.parse::<i64>().ok()
    } else {
        None
    };
    match requested_stamp {
        // A non-numeric (or overflowing) segment cannot name a stamp.
        None => return redirect(location),
        Some(stamp) => {
            if listed_raw != canonical_entries || stamp < canonical_stamp {
                return redirect(location);
            }
            // A newer stamp names a build this manifest does not have.
            if stamp > canonical_stamp {
                return not_found(content_type(asset_type));
            }
        }
    }

    let separator = if asset_type == AssetType::Js {
        graph::JS_CONCAT_SEPARATOR
    } else {
        "\n"
    };

    let mut parts = Vec::new();
    for m in canonical.iter().filter(|m| !filter.contains(*m)) {
        let content = dirs
            .compiled_file(resolved[m.as_str()].output, asset_type)
            .and_then(|p| fs::read_to_string(p).ok());
        match content {
            Some(c) => parts.push(c),
            None => return not_found(content_type(asset_type)),
        }
    }

    if parts.is_empty() {
        return not_found(content_type(asset_type));
    }
    ComboResponse {
        status: 200,
        content_type: content_type(asset_type),
        cache_control: "public, max-age=31536000",
        cacheable: true,
        location: None,
        body: parts.join(separator),
    }
}

/// Position in the manifest's `sorted_deps` layering for `asset_type` — the
/// order a canonical current-version list takes.
fn sorted_positions<'m>(manifest: &'m Manifest, asset_type: &str) -> HashMap<&'m str, usize> {
    manifest
        .sorted_deps
        .get(asset_type)
        .map(|deps| {
            deps.iter()
                .enumerate()
                .map(|(i, (name, _, _))| (name.as_str(), i))
                .collect()
        })
        .unwrap_or_default()
}

/// A `+vN[.M][-h<32 hex>]` pin on a list entry: the major version plus an
/// optional content hash — the full 32-character lowercase-hex MD5 the
/// build records. Two entries for one module must agree on both fields; a
/// version match with a hash on only one spelling, or two different hashes,
/// is a conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VersionPin {
    version: u32,
    hash: Option<String>,
}

/// One `~`/`,`-separated list entry: `name`, `name+vN`, `name+vN.M`, or
/// either versioned form followed by `-h<32 hex>` to pin the content hash
/// (hex is accepted in either case; the canonical form emits lowercase). A
/// bare name cannot carry a hash — the qualifier lives inside the version
/// suffix after `+`. The renderer always emits `.0`, so the minor is parsed
/// then ignored.
/// Returns `None` for a malformed token — an empty or non-hex hash, a hash
/// of the wrong length, a doubled `-h` marker, or a malformed or
/// overflowing version 404s the whole response.
fn parse_list_entry(entry: &str) -> Option<(&str, Option<VersionPin>)> {
    let Some((name, version)) = entry.split_once('+') else {
        return Some((entry, None));
    };
    let rest = version.strip_prefix('v')?;
    // Parsing here is inside the version suffix after `+` — module names
    // may contain `-`, but a `-` in this suffix is unambiguously the `-h`
    // hash qualifier and anything else after it fails.
    let (version_rest, hash) = match rest.split_once('-') {
        Some((v, h)) => (v, Some(h.strip_prefix('h')?)),
        None => (rest, None),
    };
    let (major, minor) = match version_rest.split_once('.') {
        Some((major, minor)) => (major, Some(minor)),
        None => (version_rest, None),
    };
    if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(minor) = minor {
        if minor.is_empty() || !minor.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let hash = match hash {
        Some(hash) if crate::manifest::is_content_hash(hash) => Some(hash.to_ascii_lowercase()),
        Some(_) => return None,
        None => None,
    };
    Some((
        name,
        Some(VersionPin {
            version: major.parse().ok()?,
            hash,
        }),
    ))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode a module name for a URL header or path: every UTF-8 byte
/// outside `[A-Za-z0-9._-]` becomes `%XX` (uppercase hex). Module names are
/// validated with Unicode `\w`, so a name like `site.café` would otherwise
/// put raw non-ASCII into a `Location` header, which HTTP stacks reject.
fn url_encode_module(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Percent-decoding for a path segment: `%XX` becomes a byte and the result
/// must be UTF-8. `+` stays literal — in the path it is the version marker.
/// A malformed escape or non-UTF-8 bytes decode to None, which the caller
/// treats as not found.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => return None,
            },
            b'%' => return None,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Minimal form decoding for query strings: `+` becomes a space, `%XX` a
/// byte, malformed escapes pass through literally.
fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (form_decode(k), form_decode(v)),
            None => (form_decode(pair), String::new()),
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{AssetEntry, AssetVersion};

    /// The stamp every fixture asset carries — 1705312800 epoch — unless a
    /// test overrides it. A second, newer stamp: 1705399200.
    const STAMP: &str = "2024-01-15T10:00:00+00:00";
    const STAMP2: &str = "2024-01-16T10:00:00+00:00";
    /// Their epoch forms, as the URL segment writes them.
    const SEG: &str = "1705312800";
    const SEG2: &str = "1705399200";

    /// The recorded hash is a real MD5 — of the output path, since these
    /// fixtures have no bytes — so the qualified URL form can render them.
    fn asset(output: &str, version: u32, timestamp: &str, dependencies: &[&str]) -> AssetEntry {
        AssetEntry {
            source: "src/x".to_string(),
            output: output.to_string(),
            hash: format!("{:x}", md5::compute(output)),
            version,
            timestamp: timestamp.to_string(),
            dependencies: dependencies.iter().map(|d| d.to_string()).collect(),
            history: Vec::new(),
            retired: false,
        }
    }

    fn write_out(base: &Path, output: &str, content: &str) {
        let path = base.join(output.trim_start_matches('/'));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn opts() -> RenderOptions {
        RenderOptions {
            cdn_base: String::new(),
            base_path: "/paste".to_string(),
            inline_threshold: 1024,
            url_component_limit: None,
        }
    }

    fn jam(css: &[&str], js: &[&str]) -> Event {
        Event::Jam(Jam {
            css: css.iter().map(|s| s.to_string()).collect(),
            js: js.iter().map(|s| s.to_string()).collect(),
            ..Jam::default()
        })
    }

    /// Manifest with js/paste.js plus the given js assets — enough for a js
    /// dump to emit the runtime inline. Every entry carries STAMP.
    fn js_manifest(entries: &[(&str, &[&str])]) -> Manifest {
        let mut manifest = Manifest::new();
        // Existing fixtures exercise the legacy token shape; the qualified
        // form has its own tests below.
        manifest.hash_urls = false;
        manifest.version = "42".to_string();
        manifest.assets.insert(
            "js/paste.js".to_string(),
            asset("/js/paste.h.v1.min.js", 1, STAMP, &[]),
        );
        for (i, (module, deps)) in entries.iter().enumerate() {
            let key = format!("js/{}.js", module.replace('.', "/"));
            let output = format!("/js/{}.h.v{}.min.js", module.replace('.', "/"), i + 2);
            manifest
                .assets
                .insert(key, asset(&output, (i + 2) as u32, STAMP, deps));
        }
        manifest
    }

    // ============================================================
    // Renderer
    // ============================================================

    #[test]
    fn test_render_css_dedups_loaded_and_url_lists_exact_group() {
        // Two css jams share dependency `base`: the second tag names only
        // what is still pending, and the URL lists the exact group in
        // dependency order with `~` and +vN.0.
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "css/base.css".to_string(),
            asset(
                "/css/base.h.v1.min.css",
                1,
                "2024-01-15T10:00:00+00:00",
                &[],
            ),
        );
        manifest.assets.insert(
            "css/a.css".to_string(),
            asset(
                "/css/a.h.v2.min.css",
                2,
                "2024-01-16T10:00:00+00:00",
                &["base"],
            ),
        );
        manifest.assets.insert(
            "css/b.css".to_string(),
            asset(
                "/css/b.h.v3.min.css",
                3,
                "2024-01-17T10:00:00+00:00",
                &["base"],
            ),
        );
        let dir = tempfile::tempdir().unwrap();
        // No compiled files — every tag is external, so the URLs show.

        let events = vec![
            jam(&["a"], &[]),
            jam(&["b"], &[]),
            Event::Dump(AssetType::Css),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert_eq!(rendered.len(), 1);
        assert_eq!(
            rendered[0].urls,
            vec![
                "/paste/1705399200/base+v1.0~a+v2.0.min.css".to_string(),
                "/paste/1705485600/b+v3.0.min.css".to_string()
            ],
            "urls: {:?}",
            rendered[0].urls
        );
        assert_eq!(rendered[0].html.matches("<link").count(), 2);
    }

    #[test]
    fn test_render_js_runtime_inlines_once_across_dumps() {
        // The runtime script carries the config and is emitted exactly once,
        // on the first js dump that has pending jams.
        let manifest = js_manifest(&[("a", &[]), ("b", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        write_out(dir.path(), "/js/a.h.v2.min.js", "A");
        write_out(dir.path(), "/js/b.h.v3.min.js", "B");

        let events = vec![
            jam(&[], &["a"]),
            Event::Dump(AssetType::Js),
            jam(&[], &["b"]),
            Event::Dump(AssetType::Js),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert_eq!(rendered.len(), 2);

        let first = &rendered[0].html;
        assert!(
            first.contains(&format!(
                "PASTE_RT{}paste.config({{packagesUrl:'/paste/{}/'}});",
                graph::JS_CONCAT_SEPARATOR,
                packages_segment(&manifest)
            )),
            "{first}"
        );
        assert_eq!(first.matches("paste.config").count(), 1, "{first}");
        assert!(
            first.contains("<script type=\"text/javascript\">A</script>"),
            "{first}"
        );

        let second = &rendered[1].html;
        assert!(!second.contains("paste.config"), "{second}");
        assert!(!second.contains("PASTE_RT"), "{second}");
    }

    #[test]
    fn test_render_async_loader_marks_members_and_names_url() {
        let manifest = js_manifest(&[("a", &[]), ("b", &["a"])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        // a and b files missing → the group goes out over a URL.

        let events = vec![jam(&[], &["b"]), Event::Dump(AssetType::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        let html = &rendered[0].html;

        assert!(
            html.starts_with("<script type=\"text/javascript\">PASTE_RT\n;\npaste.config"),
            "{html}"
        );
        assert!(
            html.contains("(function(d,r){d('a').isLoading=d('b').isLoading=!0;r('"),
            "{html}"
        );
        assert!(
            html.contains(".min.js');})(paste.define,paste.require);</script>"),
            "{html}"
        );
        assert_eq!(rendered[0].urls.len(), 1);
        assert!(
            rendered[0].urls[0].contains("a+v2.0~b+v3.0.min.js"),
            "{}",
            rendered[0].urls[0]
        );
    }

    #[test]
    fn test_render_js_group_url_follows_sorted_deps() {
        // Independent modules jammed as [b, a]: the group URL names them in
        // sorted_deps order — a first.
        let mut manifest = js_manifest(&[("a", &[]), ("b", &[])]);
        sort_deps(&mut manifest, "js", &["a", "b"]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        // a and b files missing → the group goes out over a URL.

        let events = vec![jam(&[], &["b", "a"]), Event::Dump(AssetType::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(
            rendered[0].urls[0].contains("a+v2.0~b+v3.0.min.js"),
            "{}",
            rendered[0].urls[0]
        );
    }

    #[test]
    fn test_render_places_unsorted_modules_after_sorted() {
        // cyc is absent from sorted_deps (the a↔cyc cycle cannot sort): it
        // still renders, placed after the sorted modules.
        let mut manifest = js_manifest(&[("a", &["cyc"]), ("cyc", &["a"]), ("b", &[])]);
        sort_deps(&mut manifest, "js", &["a", "b"]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");

        let events = vec![jam(&[], &["b", "cyc"]), Event::Dump(AssetType::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(
            rendered[0].urls[0].contains("a+v2.0~b+v4.0~cyc+v3.0.min.js"),
            "{}",
            rendered[0].urls[0]
        );
    }

    #[test]
    fn test_render_sync_tag_is_script_src() {
        let manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");

        let events = vec![
            Event::Jam(Jam {
                js: vec!["a".to_string()],
                sync: true,
                ..Jam::default()
            }),
            Event::Dump(AssetType::Js),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        let html = &rendered[0].html;

        assert!(
            html.contains("<script type=\"text/javascript\" src=\"/paste/"),
            "{html}"
        );
        assert!(!html.contains("(function(d,r)"), "{html}");
    }

    #[test]
    fn test_render_threshold_boundary_measured_in_utf8_bytes() {
        // "é" is two UTF-8 bytes: byte length, not char count, decides.
        // a (510 bytes) + separator (3) + b (511 bytes) = exactly 1024.
        let manifest = js_manifest(&[("a", &[]), ("b", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        write_out(dir.path(), "/js/a.h.v2.min.js", &"x".repeat(510));
        let b_file = dir.path().join("js/b.h.v3.min.js");
        fs::create_dir_all(b_file.parent().unwrap()).unwrap();
        fs::write(&b_file, format!("{}x", "é".repeat(255))).unwrap();

        let events = vec![jam(&[], &["a", "b"]), Event::Dump(AssetType::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(
            rendered[0].html.contains(&"é".repeat(255)),
            "1024 bytes exactly must inline: {}",
            rendered[0].html
        );
        assert!(rendered[0].urls.is_empty(), "{:?}", rendered[0].urls);

        // One byte over → external.
        fs::write(&b_file, format!("{}xx", "é".repeat(255))).unwrap();
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(!rendered[0].html.contains('é'), "{}", rendered[0].html);
        assert_eq!(rendered[0].urls.len(), 1, "{:?}", rendered[0].urls);
    }

    #[test]
    fn test_render_forced_inline_ignores_threshold_and_zero_disables_auto() {
        let manifest = js_manifest(&[("big", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        write_out(dir.path(), "/js/big.h.v2.min.js", &"x".repeat(5000));

        // Forced inline ignores the size check entirely.
        let events = vec![
            Event::Jam(Jam {
                js: vec!["big".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(AssetType::Js),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(
            rendered[0].html.contains(&"x".repeat(5000)),
            "{}",
            rendered[0].html
        );

        // Threshold 0 disables automatic inlining even for tiny content.
        let options = RenderOptions {
            inline_threshold: 0,
            ..opts()
        };
        let events = vec![jam(&[], &["big"]), Event::Dump(AssetType::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &options, &events).unwrap();
        assert_eq!(rendered[0].urls.len(), 1, "{:?}", rendered[0].urls);
        assert!(!rendered[0].html.contains(&"x".repeat(5000)));

        // …but a jam's own inline flag still applies at threshold 0.
        let events = vec![
            Event::Jam(Jam {
                js: vec!["big".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(AssetType::Js),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &options, &events).unwrap();
        assert!(rendered[0].html.contains(&"x".repeat(5000)));
    }

    #[test]
    fn test_render_never_inlines_unsafe_content() {
        // Content that could end the tag early is served over a URL even when
        // the jam forces inlining — checked case-insensitively.
        let manifest = {
            let mut m = js_manifest(&[("evil", &[]), ("comment", &[])]);
            m.assets.insert(
                "css/evil.css".to_string(),
                asset("/css/evil.h.v1.min.css", 1, STAMP, &[]),
            );
            m
        };
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        write_out(dir.path(), "/js/evil.h.v2.min.js", "var e=1;</SCRIPT");
        write_out(dir.path(), "/js/comment.h.v3.min.js", "<!--hidden");
        write_out(dir.path(), "/css/evil.h.v1.min.css", ".e{}</STYLE");

        let events = vec![
            Event::Jam(Jam {
                js: vec!["evil".to_string(), "comment".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(AssetType::Js),
            Event::Jam(Jam {
                css: vec!["evil".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(AssetType::Css),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();

        let js_html = &rendered[0].html;
        assert!(!js_html.contains("var e=1;"), "{js_html}");
        assert!(!js_html.contains("<!--hidden"), "{js_html}");
        assert_eq!(rendered[0].urls.len(), 1, "{:?}", rendered[0].urls);

        let css_html = &rendered[1].html;
        assert!(css_html.contains("<link"), "{css_html}");
        assert!(!css_html.contains("</style"), "{css_html}");
    }

    #[test]
    fn test_render_missing_compiled_file_goes_external() {
        let manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        // a's compiled file is absent → external tag.

        let events = vec![jam(&[], &["a"]), Event::Dump(AssetType::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert_eq!(rendered[0].urls.len(), 1);
        assert!(rendered[0].html.contains(".min.js"), "{}", rendered[0].html);
    }

    #[test]
    fn test_render_media_on_link_and_style_attribute_escaped() {
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "css/a.css".to_string(),
            asset("/css/a.h.v1.min.css", 1, STAMP, &[]),
        );
        let dir = tempfile::tempdir().unwrap();

        // External link carries media, escaped for an attribute context.
        let events = vec![
            Event::Jam(Jam {
                css: vec!["a".to_string()],
                media: Some("x\"onload=\"evil".to_string()),
                ..Jam::default()
            }),
            Event::Dump(AssetType::Css),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        let html = &rendered[0].html;
        assert!(
            html.contains(" media=\"x&quot;onload=&quot;evil\""),
            "{html}"
        );
        assert!(!html.contains("media=\"x\"onload"), "{html}");

        // Inline style carries it too.
        write_out(dir.path(), "/css/a.h.v1.min.css", ".a{}");
        let events = vec![
            Event::Jam(Jam {
                css: vec!["a".to_string()],
                media: Some("print".to_string()),
                ..Jam::default()
            }),
            Event::Dump(AssetType::Css),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(
            rendered[0]
                .html
                .contains("<style type=\"text/css\" media=\"print\">.a{}</style>"),
            "{}",
            rendered[0].html
        );
    }

    #[test]
    fn test_render_url_stamp_max_over_group_and_missing_stamp_errors() {
        let mut manifest = js_manifest(&[("a", &[]), ("b", &[])]);
        manifest.assets.get_mut("js/b.js").unwrap().timestamp = STAMP2.to_string();
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");

        // Group members' stamps rule: the newer one wins.
        let events = vec![jam(&[], &["a", "b"]), Event::Dump(AssetType::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(
            rendered[0].urls[0].starts_with("/paste/1705399200/"),
            "{}",
            rendered[0].urls[0]
        );

        // A member without a parseable stamp cannot name a segment — the
        // manifest must be rebuilt; there is no constant fallback.
        let mut manifest = js_manifest(&[("a", &[]), ("b", &[])]);
        manifest
            .assets
            .get_mut("js/a.js")
            .unwrap()
            .timestamp
            .clear();
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("a"), "{err}");
        assert!(err.to_string().contains("rebuild"), "{err}");
    }

    #[test]
    fn test_render_jam_after_dump_emits_only_new_modules() {
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "css/base.css".to_string(),
            asset("/css/base.h.v1.min.css", 1, STAMP, &[]),
        );
        manifest.assets.insert(
            "css/a.css".to_string(),
            asset("/css/a.h.v1.min.css", 1, STAMP, &["base"]),
        );
        manifest.assets.insert(
            "css/b.css".to_string(),
            asset("/css/b.h.v1.min.css", 1, STAMP, &["a"]),
        );
        let dir = tempfile::tempdir().unwrap();

        let events = vec![
            jam(&["a"], &[]),
            Event::Dump(AssetType::Css),
            jam(&["b"], &[]),
            Event::Dump(AssetType::Css),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert_eq!(rendered.len(), 2);
        assert!(
            rendered[0].urls[0].contains("base+v1.0~a+v1.0"),
            "{}",
            rendered[0].urls[0]
        );
        assert_eq!(
            rendered[1].urls,
            vec![format!("/paste/{SEG}/b+v1.0.min.css")],
            "{:?}",
            rendered[1].urls
        );
    }

    #[test]
    fn test_render_invalid_module_name_errors() {
        let manifest = js_manifest(&[]);
        let dir = tempfile::tempdir().unwrap();
        let events = vec![jam(&["bad name!"], &[]), Event::Dump(AssetType::Css)];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("bad name!"), "{err}");

        let events = vec![jam(&[], &["../x"]), Event::Dump(AssetType::Js)];
        assert!(render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).is_err());
    }

    #[test]
    fn test_render_unsafe_runtime_goes_external_with_config_script() {
        // Unsafe runtime content → external src plus a separate config script.
        let manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE</script>x");
        write_out(dir.path(), "/js/a.h.v2.min.js", "A");

        let events = vec![jam(&[], &["a"]), Event::Dump(AssetType::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        let html = &rendered[0].html;
        assert!(
            html.contains("<script type=\"text/javascript\" src=\"/paste/"),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "</script>\n<script type=\"text/javascript\">paste.config({{packagesUrl:'/paste/{}/'}});</script>",
                packages_segment(&manifest)
            )),
            "{html}"
        );
        // The unsafe runtime content never reaches the page.
        assert!(!html.contains("PASTE</script>x"), "{html}");
    }

    #[test]
    fn test_render_js_dump_without_runtime_module_errors() {
        // A jammed js dump needs the `paste` runtime; a manifest without it
        // cannot produce a working page — render errors rather than emitting
        // a URL nothing can serve.
        let mut manifest = js_manifest(&[("a", &[])]);
        manifest.assets.remove("js/paste.js");
        let dir = tempfile::tempdir().unwrap();

        let events = vec![jam(&[], &["a"]), Event::Dump(AssetType::Js)];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("paste"), "{err}");
        assert!(err.to_string().contains("js"), "{err}");
    }

    #[test]
    fn test_render_unknown_module_errors_naming_module_and_type() {
        let manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        write_out(dir.path(), "/js/a.h.v2.min.js", "A");

        let events = vec![jam(&["ghost"], &[]), Event::Dump(AssetType::Css)];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
        assert!(err.to_string().contains("css"), "{err}");

        let events = vec![jam(&[], &["ghost"]), Event::Dump(AssetType::Js)];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
        assert!(err.to_string().contains("js"), "{err}");

        // A dependency that resolves to nothing fails the same way.
        let mut manifest = js_manifest(&[("a", &[])]);
        manifest.assets.get_mut("js/a.js").unwrap().dependencies = vec!["ghost".to_string()];
        let events = vec![jam(&[], &["a"]), Event::Dump(AssetType::Js)];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn test_render_cdn_base_prefixes_tags_but_not_urls() {
        let manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");

        let options = RenderOptions {
            cdn_base: "https://cdn.example".to_string(),
            ..opts()
        };
        let events = vec![jam(&[], &["a"]), Event::Dump(AssetType::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &options, &events).unwrap();
        assert!(
            rendered[0].html.contains("https://cdn.example/paste/"),
            "{}",
            rendered[0].html
        );
        assert!(rendered[0].html.contains(&format!(
            "packagesUrl:'https://cdn.example/paste/{}/'",
            packages_segment(&manifest)
        )));
        assert!(
            rendered[0].urls[0].starts_with("/paste/"),
            "urls hold paths without cdn_base: {:?}",
            rendered[0].urls
        );

        // Quotes, brackets, backticks, backslashes or whitespace make a
        // cdn_base unusable inside a script or attribute.
        for bad in [
            "https://cdn example",
            "https://cdn'ex",
            "x`y",
            "x<y",
            "x\\y",
        ] {
            let options = RenderOptions {
                cdn_base: bad.to_string(),
                ..opts()
            };
            assert!(
                render_all(&manifest, &OutputDirs::under(dir.path()), &options, &events).is_err(),
                "cdn_base {:?} must error",
                bad
            );
        }
    }

    #[test]
    fn test_render_base_path_rejects_unsafe_characters() {
        // base_path lands in the same attributes and the packagesUrl JS
        // string as cdn_base, so it is held to the same character rule on
        // top of the leading/trailing slash rule.
        let manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        write_out(dir.path(), "/js/a.h.v2.min.js", "A");
        let events = vec![jam(&[], &["a"]), Event::Dump(AssetType::Js)];

        for bad in [
            "/pa ste",
            "/paste'",
            "/paste\"",
            "/paste<x",
            "/paste`x",
            "/paste\\x",
            "paste",
            "/paste/",
            "/",
            "/../../escape",
            "/paste?x",
            "/paste#x",
            "//host",
            "/a/./b",
            "/a/../b",
            "/a:b",
        ] {
            let options = RenderOptions {
                base_path: bad.to_string(),
                ..opts()
            };
            assert!(
                render_all(&manifest, &OutputDirs::under(dir.path()), &options, &events).is_err(),
                "base_path {:?} must error",
                bad
            );
        }
    }

    #[test]
    fn test_render_requires_trailing_dump() {
        let manifest = js_manifest(&[]);
        let dir = tempfile::tempdir().unwrap();
        let events = vec![jam(&[], &["a"])];
        assert!(render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).is_err());
        assert!(render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &[]).is_err());
    }

    // ============================================================
    // Expansion (the serving order shared with combo)
    // ============================================================

    #[test]
    fn test_expand_dedup_diamond_order_and_unknown_passthrough() {
        // d <- b,c <- a keeps d once and deps before dependents; unknown
        // names pass through unexpanded so the caller can name them — render
        // fails on them, the combo answers 404.
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        for (key, deps) in [
            ("js/d.js", vec![]),
            ("js/b.js", vec!["d".to_string()]),
            ("js/c.js", vec!["d".to_string()]),
            ("js/a.js", vec!["b".to_string(), "c".to_string()]),
        ] {
            manifest.assets.insert(
                key.to_string(),
                asset(
                    "/x.h.v1.min.js",
                    1,
                    "",
                    &deps.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
                ),
            );
        }

        let result = expand_dedup(&manifest, &["a".to_string(), "c".to_string()], "js");
        assert_eq!(result.iter().filter(|m| *m == "d").count(), 1, "{result:?}");
        let pos = |m: &str| result.iter().position(|x| x == m).unwrap();
        assert!(pos("d") < pos("b") && pos("d") < pos("c"));
        assert!(pos("b") < pos("a") && pos("c") < pos("a"));

        assert_eq!(
            expand_dedup(&manifest, &["ghost".to_string()], "js"),
            vec!["ghost"]
        );

        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "css/base.css".to_string(),
            asset("/css/base.h.v1.min.css", 1, "", &[]),
        );
        manifest.assets.insert(
            "css/theme.css".to_string(),
            asset("/css/theme.h.v1.min.css", 1, "", &["base"]),
        );
        assert_eq!(
            expand_dedup(&manifest, &["theme".to_string()], "css"),
            vec!["base", "theme"]
        );
    }

    /// Give a fixture manifest a `sorted_deps` layering for one type.
    fn sort_deps(manifest: &mut Manifest, asset_type: &str, names: &[&str]) {
        manifest.sorted_deps.insert(
            asset_type.to_string(),
            names
                .iter()
                .map(|name| (name.to_string(), String::new(), 1))
                .collect(),
        );
    }

    #[test]
    fn test_expand_dedup_orders_by_sorted_deps() {
        // The layering decides order, not the request: listed [b, a] comes
        // out [a, b]; a name absent from sorted_deps follows the sorted ones
        // rather than being dropped.
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        for key in ["js/a.js", "js/b.js", "js/z.js"] {
            manifest
                .assets
                .insert(key.to_string(), asset("/x.h.v1.min.js", 1, "", &[]));
        }
        sort_deps(&mut manifest, "js", &["a", "b"]);

        assert_eq!(
            expand_dedup(&manifest, &["b".to_string(), "a".to_string()], "js"),
            vec!["a", "b"]
        );
        assert_eq!(
            expand_dedup(
                &manifest,
                &["z".to_string(), "b".to_string(), "a".to_string()],
                "js"
            ),
            vec!["a", "b", "z"]
        );
    }

    // ============================================================
    // Combo resolution
    // ============================================================

    fn combo_manifest() -> (Manifest, tempfile::TempDir) {
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "js/a.js".to_string(),
            asset("/js/a.h.v1.min.js", 1, STAMP, &[]),
        );
        manifest.assets.insert(
            "js/b.js".to_string(),
            asset("/js/b.h.v1.min.js", 1, STAMP, &["a"]),
        );
        manifest.assets.insert(
            "css/base.css".to_string(),
            asset("/css/base.h.v1.min.css", 1, STAMP, &[]),
        );
        manifest.assets.insert(
            "css/theme.css".to_string(),
            asset("/css/theme.h.v1.min.css", 1, STAMP, &["base"]),
        );
        // The layering a real build records: dependencies before dependents.
        sort_deps(&mut manifest, "js", &["a", "b"]);
        sort_deps(&mut manifest, "css", &["base", "theme"]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/a.h.v1.min.js", "A");
        write_out(dir.path(), "/js/b.h.v1.min.js", "B");
        write_out(dir.path(), "/css/base.h.v1.min.css", ".base{}");
        write_out(dir.path(), "/css/theme.h.v1.min.css", ".theme{}");
        (manifest, dir)
    }

    #[test]
    fn test_combo_versioned_serves_exact_list_in_dependency_order() {
        let (manifest, dir) = combo_manifest();

        // The canonical URL: every member pinned at current, dependency
        // order (a before b), the group stamp — served year-cached.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/a+v1.0~b+v1.0.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A\n;\nB");
        assert_eq!(r.content_type, "application/javascript; charset=utf-8");
        assert_eq!(r.cache_control, "public, max-age=31536000");
        assert!(r.cacheable);
        assert_eq!(r.location, None);

        // Dependency not listed → not served.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/b+v1.0.js"),
            None,
        );
        assert_eq!(r.body, "B");

        // .min or not, the same compiled output serves.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js"),
            None,
        );
        assert_eq!(r.body, "A\n;\nB");
    }

    #[test]
    fn test_combo_wrong_order_redirects_to_the_sorted_url() {
        let (manifest, dir) = combo_manifest();

        // b listed before its dependency a is not the canonical list — the
        // answer is a 302 to the sorted URL, not the reordered content.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/b+v1.0~a+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js").as_str())
        );
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);
        assert!(r.body.is_empty());
    }

    #[test]
    fn test_combo_stale_or_ahead_or_nonnumeric_segments() {
        let (manifest, dir) = combo_manifest();
        let dirs = OutputDirs::under(dir.path());

        // An older stamp is a stale URL — the client is sent to the current
        // canonical address.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/1705312799/a+v1.0.min.js",
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG}/a+v1.0.min.js").as_str())
        );
        assert_eq!(r.cache_control, "no-cache");

        // A newer stamp names a build this manifest does not have.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/1705312801/a+v1.0.min.js",
            None,
        );
        assert_eq!(r.status, 404);
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);

        // A non-numeric segment (a manifest-version label, say) cannot be a
        // canonical URL — it canonicalizes like any other misspelling.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/static/a+v1.0.min.js",
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG}/a+v1.0.min.js").as_str())
        );
    }

    #[test]
    fn test_combo_redirect_preserves_the_query() {
        let (manifest, dir) = combo_manifest();

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/b+v1.0~a+v1.0.min.js"),
            Some("filter=b"),
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js?filter=b").as_str())
        );
    }

    #[test]
    fn test_combo_redirect_location_is_percent_encoded_ascii() {
        // Module names accept Unicode (\w is Unicode-aware) and the request
        // list arrives percent-decoded, so a redirect's Location must
        // re-encode the name — a raw `é` is not a legal header value.
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "js/site/café.js".to_string(),
            asset("/js/site/café.h.v1.min.js", 1, STAMP, &[]),
        );
        sort_deps(&mut manifest, "js", &["site.café"]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/site/café.h.v1.min.js", "C");

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/1705312799/site.caf%C3%A9+v1.0.min.js",
            None,
        );
        assert_eq!(r.status, 302);
        let location = r.location.as_deref().unwrap();
        assert_eq!(location, format!("/paste/{SEG}/site.caf%C3%A9+v1.0.min.js"));
        assert!(
            location.is_ascii(),
            "Location must be pure ASCII: {location}"
        );
    }

    #[test]
    fn test_combo_unpinned_name_redirects_to_the_pinned_url() {
        let (manifest, dir) = combo_manifest();

        // A bare name resolves to current — but the canonical URL pins it,
        // so the unpinned spelling redirects rather than serving a
        // year-cached mutable URL.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/a.min.js"),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG}/a+v1.0.min.js").as_str())
        );
    }

    #[test]
    fn test_combo_historical_pins_serve_in_listed_order() {
        let (manifest, dir) = versioned_manifest();
        let dirs = OutputDirs::under(dir.path());

        // a is pinned at retained v1 — a URL minted while the dependency
        // order was different. A historical pin keeps the listed order (the
        // order the page that rendered it depends on), not the current
        // layering's.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A1\n;\nB");
        assert_eq!(r.cache_control, "public, max-age=31536000");
        assert!(r.cacheable);
    }

    #[test]
    fn test_combo_require_expands_either_way_and_is_no_cache() {
        let (manifest, dir) = combo_manifest();

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/123/b.js",
            Some("require"),
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A\n;\nB");
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/require/b.js",
            None,
        );
        assert_eq!(r.body, "A\n;\nB");
        assert!(!r.cacheable);

        // require with a value expands the same way.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/123/b.js",
            Some("require=deep"),
        );
        assert_eq!(r.body, "A\n;\nB");
    }

    /// Two independent js modules layered [a, b] — enough to show the
    /// layering, not the request order, decides serving order.
    fn layered_manifest() -> (Manifest, tempfile::TempDir) {
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "js/a.js".to_string(),
            asset("/js/a.h.v1.min.js", 1, STAMP, &[]),
        );
        manifest.assets.insert(
            "js/b.js".to_string(),
            asset("/js/b.h.v1.min.js", 1, STAMP, &[]),
        );
        sort_deps(&mut manifest, "js", &["a", "b"]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/a.h.v1.min.js", "A");
        write_out(dir.path(), "/js/b.h.v1.min.js", "B");
        (manifest, dir)
    }

    #[test]
    fn test_combo_exact_list_orders_by_sorted_deps() {
        let (manifest, dir) = layered_manifest();

        // The canonical list follows sorted_deps (a first), and that exact
        // list serves A before B.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A\n;\nB");
    }

    #[test]
    fn test_combo_require_output_follows_sorted_deps() {
        let (manifest, dir) = layered_manifest();

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/123/b+v1.0~a+v1.0.min.js",
            Some("require"),
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A\n;\nB");
        assert!(!r.cacheable);
    }

    #[test]
    fn test_combo_filter_drops_after_ordering() {
        let (manifest, dir) = layered_manifest();

        // ?filter= drops delivered modules after the canonical ordering —
        // it does not participate in the canonicalization itself.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js"),
            Some("filter=a"),
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "B");
    }

    #[test]
    fn test_combo_filter_drops_after_expansion() {
        let (manifest, dir) = combo_manifest();

        // ?filter= drops delivered modules from the expanded group — comma
        // and tilde separators both work, invalid names are ignored.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/123/b.js",
            Some("require&filter=a"),
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "B");

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/123/b.js",
            Some("require&filter=a~b"),
        );
        assert_eq!(r.status, 404);

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/123/b.js",
            Some("require&filter=a,b"),
        );
        assert_eq!(r.status, 404);

        // A filter name that decodes outside the module alphabet is dropped,
        // not applied.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/123/b.js",
            Some("require&filter=a+v1"),
        );
        assert_eq!(r.body, "A\n;\nB");
    }

    #[test]
    fn test_combo_unversioned_is_no_cache() {
        let (manifest, dir) = combo_manifest();
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/a.js",
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A");
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);
    }

    #[test]
    fn test_combo_any_unresolvable_module_is_404_no_cache() {
        let (manifest, dir) = combo_manifest();
        let dirs = OutputDirs::under(dir.path());

        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/123/missing+v1.0.js",
            None,
        );
        assert_eq!(r.status, 404);
        assert_eq!(r.body, "/* modules not found */");
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);

        // One unknown member in a listed group sinks the whole response —
        // a partial group is never served under a cacheable 200.
        for path in [
            "/paste/123/a+v1.0~missing+v1.0.js",
            "/paste/123/missing+v1.0~a+v1.0.min.js",
            "/paste/a~missing.js",
        ] {
            let r = combo(&manifest, &dirs, "/paste", path, None);
            assert_eq!(r.status, 404, "{path}");
            assert_eq!(r.cache_control, "no-cache", "{path}");
            assert!(!r.cacheable, "{path}");
            assert_eq!(r.body, "/* modules not found */", "{path}");
        }

        // The `?require` expansion resolves no better.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/123/a~missing.js",
            Some("require"),
        );
        assert_eq!(r.status, 404);
        assert!(!r.cacheable);

        // A listed module whose compiled file is missing fails the same way,
        // versioned and under `?require`.
        fs::remove_file(dir.path().join("js/a.h.v1.min.js")).unwrap();
        let missing_file_path = format!("/paste/{SEG}/a+v1.0~b+v1.0.js");
        for (path, query) in [
            (missing_file_path.as_str(), None),
            ("/paste/123/b.js", Some("require")),
        ] {
            let r = combo(&manifest, &dirs, "/paste", path, query);
            assert_eq!(r.status, 404, "{path}");
            assert_eq!(r.cache_control, "no-cache", "{path}");
            assert!(!r.cacheable, "{path}");
        }
    }

    #[test]
    fn test_combo_invalid_or_empty_entry_is_404_no_cache() {
        let (manifest, dir) = combo_manifest();
        let dirs = OutputDirs::under(dir.path());

        // An entry that is empty or outside the module-name alphabet 404s the
        // whole response rather than silently narrowing the group.
        for path in [
            "/paste/123/a+v1.0~bad%20name+v1.0.min.js",
            "/paste/123/a+v1.0~%3Cx%3E+v1.0.min.js",
            "/paste/123/a+v1.0~~.min.js",
        ] {
            let r = combo(&manifest, &dirs, "/paste", path, None);
            assert_eq!(r.status, 404, "{path}");
            assert_eq!(r.cache_control, "no-cache", "{path}");
            assert!(!r.cacheable, "{path}");
            assert_eq!(r.body, "/* modules not found */", "{path}");
        }
    }

    #[test]
    fn test_combo_path_like_names_cannot_escape_output_base() {
        let (manifest, dir) = combo_manifest();
        // A file beside output_base must never be reachable through the URL.
        fs::write(dir.path().join("outside.js"), "SECRET").unwrap();

        // `..` survives the module-name alphabet but resolves no asset —
        // lookup is by manifest key, never by path, and an unresolvable
        // member sinks the whole group.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/123/..+v1.0~a+v1.0.js",
            None,
        );
        assert_eq!(r.status, 404);
        assert!(!r.body.contains("SECRET"));

        // A long list may continue over `/` components, but an empty
        // component is not a continuation — the URL is malformed.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/123/a//b.js",
            None,
        );
        assert_eq!(r.status, 404);

        // An encoded slash inside a name decodes to a character the module
        // alphabet rejects — `a%2Fb` is not a module, encoded or not.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/123/a%2Fb+v1.0.js",
            None,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn test_combo_joiners_per_type() {
        let (manifest, dir) = combo_manifest();

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/base+v1.0~theme+v1.0.css"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "text/css; charset=utf-8");
        assert_eq!(r.body, ".base{}\n.theme{}");

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/a+v1.0~b+v1.0.js"),
            None,
        );
        assert_eq!(r.body, "A\n;\nB");
    }

    #[test]
    fn test_combo_percent_decodes_the_list_segment() {
        // Module names outside ASCII arrive percent-encoded; `+` stays the
        // version marker (it must not decode to a space here).
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "js/site/café.js".to_string(),
            asset("/js/site/café.h.v1.min.js", 1, STAMP, &[]),
        );
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/site/café.h.v1.min.js", "CAFE");
        let dirs = OutputDirs::under(dir.path());

        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG}/site.caf%C3%A9+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200, "{:?}", r.body);
        assert_eq!(r.body, "CAFE");

        // A malformed escape or non-UTF-8 bytes are simply not found.
        for bad in [
            "/paste/1/site.caf%C3.js",
            "/paste/1/site.caf%zz.js",
            "/paste/1/site.caf%FF.js",
        ] {
            let r = combo(&manifest, &dirs, "/paste", bad, None);
            assert_eq!(r.status, 404, "{bad}");
        }
    }

    /// `a` is at v2 with v1 retained in history and depends on `b`; `b` is
    /// v1, current only — the shape `storage.keep_versions` leaves behind on
    /// dist/s3 backends. Hashes are the real MD5 of each fixture body, so the
    /// qualified URL form can pin them.
    fn versioned_manifest() -> (Manifest, tempfile::TempDir) {
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        let mut a = asset("/js/a.h.v2.min.js", 2, STAMP2, &["b"]);
        a.hash = format!("{:x}", md5::compute(b"A2"));
        a.history.push(AssetVersion {
            hash: format!("{:x}", md5::compute(b"A1")),
            version: 1,
            output: "/js/a.h.v1.min.js".to_string(),
            timestamp: STAMP.to_string(),
        });
        manifest.assets.insert("js/a.js".to_string(), a);
        let mut b = asset("/js/b.h.v1.min.js", 1, STAMP, &[]);
        b.hash = format!("{:x}", md5::compute(b"B"));
        manifest.assets.insert("js/b.js".to_string(), b);
        // Today's layering puts b before a (a depends on b) — so a URL that
        // lists a@v1 first can only ever have been minted under the earlier
        // layering, and serving it must keep its listed order.
        sort_deps(&mut manifest, "js", &["b", "a"]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/a.h.v1.min.js", "A1");
        write_out(dir.path(), "/js/a.h.v2.min.js", "A2");
        write_out(dir.path(), "/js/b.h.v1.min.js", "B");
        (manifest, dir)
    }

    #[test]
    fn test_combo_serves_the_pinned_version_or_404s() {
        let (manifest, dir) = versioned_manifest();
        let dirs = OutputDirs::under(dir.path());

        // The current version pinned at its own stamp serves the current
        // content, year-cached.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG2}/a+v2.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A2");
        assert_eq!(r.cache_control, "public, max-age=31536000");
        assert!(r.cacheable);

        // A retained history version at its own (older) stamp serves its own
        // file, still year-cached — the URL names the content exactly.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG}/a+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A1");
        assert_eq!(r.cache_control, "public, max-age=31536000");
        assert!(r.cacheable);

        // A version newer than this server knows is never answered with
        // another version's content.
        let r = combo(&manifest, &dirs, "/paste", "/paste/123/a+v3.0.min.js", None);
        assert_eq!(r.status, 404);
        assert_eq!(r.body, "/* modules not found */");
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);

        // A history version whose compiled file was pruned fails the same way.
        fs::remove_file(dir.path().join("js/a.h.v1.min.js")).unwrap();
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG}/a+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 404);
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);
    }

    #[test]
    fn test_combo_group_mixes_current_and_history_versions() {
        let (manifest, dir) = versioned_manifest();
        let dirs = OutputDirs::under(dir.path());

        // Each member is served at the version its entry named — a at the
        // retained v1, b at the current v1. A historical pin keeps the
        // LISTED order (a first, as the page that minted this URL wrote it),
        // not today's layering, which puts b before a.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A1\n;\nB");
        assert_eq!(r.cache_control, "public, max-age=31536000");
        assert!(r.cacheable);
    }

    #[test]
    fn test_combo_malformed_version_tokens_are_404() {
        let (manifest, dir) = versioned_manifest();
        let dirs = OutputDirs::under(dir.path());

        for path in [
            "/paste/123/a+.min.js",
            "/paste/123/a+v.min.js",
            "/paste/123/a+1.0.min.js",
            "/paste/123/a+vX.0.min.js",
            "/paste/123/a+v1.0+v2.0.min.js",
            "/paste/123/a+v99999999999.0.min.js",
        ] {
            let r = combo(&manifest, &dirs, "/paste", path, None);
            assert_eq!(r.status, 404, "{path}");
            assert_eq!(r.body, "/* modules not found */", "{path}");
            assert_eq!(r.cache_control, "no-cache", "{path}");
            assert!(!r.cacheable, "{path}");
        }
    }

    #[test]
    fn test_combo_duplicate_entries_must_agree_on_version() {
        let (manifest, dir) = versioned_manifest();
        let dirs = OutputDirs::under(dir.path());

        // The same version twice dedupes to one module.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG}/a+v1.0~a+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A1");

        // Two different pins for one module are ambiguous — not served.
        for path in [
            "/paste/123/a+v1.0~a+v2.0.min.js",
            "/paste/123/a+v1.0~a.min.js",
        ] {
            let r = combo(&manifest, &dirs, "/paste", path, None);
            assert_eq!(r.status, 404, "{path}");
            assert!(!r.cacheable, "{path}");
        }
    }

    #[test]
    fn test_combo_unversioned_entries_under_cacheable_form_are_loader_only() {
        let (manifest, dir) = combo_manifest();
        let dirs = OutputDirs::under(dir.path());
        let segment = packages_segment(&manifest);

        // The runtime loader's own URL — `{packagesUrl}{module}.js` — is the
        // one unversioned name served under a year-cached URL.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{segment}/a.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A");
        assert_eq!(r.cache_control, "public, max-age=31536000");
        assert!(r.cacheable);

        // A loader segment from another build's page is just a versioned
        // URL with a non-numeric segment: it redirects to the canonical
        // pinned URL rather than serving content.
        let r = combo(&manifest, &dirs, "/paste", "/paste/deadbeef0000/a.js", None);
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG}/a+v1.0.js").as_str())
        );
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);

        // The loader segment serves js only — a css list under it behaves
        // like any versioned css request: redirect to the pinned URL.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{segment}/theme.css"),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG}/theme+v1.0.css").as_str())
        );

        // A timestamp segment pinning no version redirects too — the
        // canonical URL pins every listed module.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/1770462000/a.min.js",
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG}/a+v1.0.min.js").as_str())
        );
    }

    #[test]
    fn test_combo_require_pins_the_listed_module_and_serves_current_deps() {
        let (manifest, dir) = versioned_manifest();

        // `require` expands the list: a's dependency b joins unpinned and
        // resolves to current, while a itself serves the pinned history
        // version — dependency order puts b first.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/require/a+v1.0.js",
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "B\n;\nA1");
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);
    }

    #[test]
    fn test_compiled_files_resolve_through_configured_type_directories() {
        // The build writes each asset into its type directory, which need
        // not be base/js — a custom output.js is honoured by both the
        // renderer and the combo.
        let manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        let dirs = OutputDirs {
            base: dir.path().to_path_buf(),
            js: dir.path().join("assets-js"),
            css: dir.path().join("assets-css"),
        };
        fs::create_dir_all(&dirs.js).unwrap();
        fs::write(dirs.js.join("paste.h.v1.min.js"), "PASTE_RT").unwrap();
        fs::write(dirs.js.join("a.h.v2.min.js"), "CUSTOM_A").unwrap();

        let events = vec![jam(&[], &["a"]), Event::Dump(AssetType::Js)];
        let rendered = render_all(&manifest, &dirs, &opts(), &events).unwrap();
        assert!(
            rendered[0]
                .html
                .contains("<script type=\"text/javascript\">CUSTOM_A</script>"),
            "{}",
            rendered[0].html
        );

        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG}/a+v2.0.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "CUSTOM_A");
    }

    #[test]
    fn test_render_runtime_ending_in_line_comment_keeps_config() {
        // A debug build keeps comments: a runtime ending in `//` must not
        // swallow the config call — the concat separator puts it on its own
        // line.
        let manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT // trailing");
        write_out(dir.path(), "/js/a.h.v2.min.js", "A");

        let events = vec![jam(&[], &["a"]), Event::Dump(AssetType::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        let html = &rendered[0].html;
        assert!(
            html.contains(&format!(
                "PASTE_RT // trailing{}paste.config({{packagesUrl:'/paste/{}/'}});",
                graph::JS_CONCAT_SEPARATOR,
                packages_segment(&manifest)
            )),
            "{html}"
        );
    }

    #[test]
    fn test_packages_segment_follows_js_content() {
        // Deterministic for identical manifests; one changed js hash moves
        // the segment so the year-cached packages URLs refresh.
        let manifest_a = js_manifest(&[("a", &[])]);
        let manifest_b = js_manifest(&[("a", &[])]);
        assert_eq!(packages_segment(&manifest_a), packages_segment(&manifest_b));
        assert_eq!(packages_segment(&manifest_a).len(), 12);
        assert!(packages_segment(&manifest_a)
            .chars()
            .all(|c| c.is_ascii_hexdigit()));

        let mut manifest_c = js_manifest(&[("a", &[])]);
        manifest_c.assets.get_mut("js/a.js").unwrap().hash = "changed".to_string();
        assert_ne!(packages_segment(&manifest_a), packages_segment(&manifest_c));

        // A css-only difference does not move it.
        let mut manifest_d = js_manifest(&[("a", &[])]);
        manifest_d.assets.insert(
            "css/site.css".to_string(),
            asset("/css/site.h.v1.min.css", 1, "", &[]),
        );
        assert_eq!(packages_segment(&manifest_a), packages_segment(&manifest_d));
    }

    // ============================================================
    // Output path safety and option validation
    // ============================================================

    #[test]
    fn test_compiled_file_rejects_unsafe_outputs() {
        let dirs = OutputDirs::under(Path::new("/base"));
        // Plain outputs resolve under the type or base directories.
        assert_eq!(
            dirs.compiled_file("/js/a.v1.min.js", AssetType::Js),
            Some(PathBuf::from("/base/js/a.v1.min.js"))
        );
        assert_eq!(
            dirs.compiled_file("/css/a.v1.min.css", AssetType::Css),
            Some(PathBuf::from("/base/css/a.v1.min.css"))
        );
        assert_eq!(
            dirs.compiled_file("/other/file.txt", AssetType::Js),
            Some(PathBuf::from("/base/other/file.txt"))
        );
        // Traversal, drive markers, backslashes and empties resolve to
        // nothing — on every platform, not just Windows.
        for bad in [
            "/js/../../outside.js",
            "/js/../a.js",
            "/./a.js",
            "/js//tmp/outside.js",
            "",
            "/",
            "js\\a.js",
            "a:b.js",
            "/C:/a.js",
        ] {
            assert_eq!(dirs.compiled_file(bad, AssetType::Js), None, "{bad}");
        }
    }

    #[test]
    fn test_combo_crafted_output_path_is_404() {
        // A manifest output that traverses outside the output tree must
        // never be served — the member resolves to no file and sinks the
        // group.
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "js/evil.js".to_string(),
            asset("/js/../../outside.js", 1, "", &[]),
        );
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("outside.js"), "SECRET").unwrap();

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/evil.js",
            None,
        );
        assert_eq!(r.status, 404);
        assert!(!r.body.contains("SECRET"));
    }

    #[test]
    fn test_combo_doubled_separator_output_cannot_escape() {
        // `/js//var/.../outside.js` used to strip the `js/` prefix and leave
        // an absolute remainder that `Path::join` resolves outside `base`.
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.js");
        fs::write(&outside, "SECRET").unwrap();
        let absolute = outside.to_str().unwrap().to_string();

        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "js/evil.js".to_string(),
            asset(&format!("/js/{absolute}"), 1, "", &[]),
        );
        assert_eq!(
            OutputDirs::under(dir.path())
                .compiled_file(&manifest.assets["js/evil.js"].output, AssetType::Js),
            None
        );

        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            "/paste/evil.js",
            None,
        );
        assert_eq!(r.status, 404);
        assert!(!r.body.contains("SECRET"));
    }

    #[test]
    fn test_render_forced_inline_with_unsafe_output_goes_external() {
        // inline=true but the manifest output is unsafe: the file is never
        // read and the tag falls back to external.
        let mut manifest = js_manifest(&[("a", &[])]);
        manifest.assets.get_mut("js/a.js").unwrap().output = "/js/../outside.js".to_string();
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        fs::write(dir.path().join("outside.js"), "SECRET").unwrap();

        let events = vec![
            Event::Jam(Jam {
                js: vec!["a".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(AssetType::Js),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(!rendered[0].html.contains("SECRET"), "{}", rendered[0].html);
        assert_eq!(rendered[0].urls.len(), 1, "{:?}", rendered[0].urls);
    }

    #[test]
    fn test_render_forced_inline_doubled_separator_output_goes_external() {
        // Same escape as forced-inline `..`, but through `/js//abs/path`:
        // the doubled separator leaves an absolute remainder after the type
        // prefix strip, so the file must never be read.
        let dir = tempfile::tempdir().unwrap();
        write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
        let outside = dir.path().join("outside.js");
        fs::write(&outside, "SECRET").unwrap();

        let mut manifest = js_manifest(&[("a", &[])]);
        manifest.assets.get_mut("js/a.js").unwrap().output =
            format!("/js/{}", outside.to_str().unwrap());

        let events = vec![
            Event::Jam(Jam {
                js: vec!["a".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(AssetType::Js),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(!rendered[0].html.contains("SECRET"), "{}", rendered[0].html);
    }

    #[test]
    fn test_validate_base_path_shape() {
        assert!(validate_base_path("/paste").is_ok());
        assert!(validate_base_path("/a/b/c").is_ok());
        for bad in [
            "",
            "paste",
            "/",
            "/paste/",
            "//host",
            "/a/./b",
            "/a/../b",
            "/../../escape",
            "/paste?x",
            "/paste#x",
            "/paste%x",
            "/a:b",
            "/pa ste",
            "/paste\n",
            "/a/*wild",
            "/*path",
        ] {
            assert!(validate_base_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_combo_with_an_invalid_base_path_is_404() {
        let (manifest, dir) = combo_manifest();
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/a/../b",
            "/a/../b/123/a+v1.0.js",
            None,
        );
        assert_eq!(r.status, 404);
    }

    // ============================================================
    // Media-keyed CSS dedup
    // ============================================================

    #[test]
    fn test_render_css_dedup_is_per_media() {
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        manifest.assets.insert(
            "css/a.css".to_string(),
            asset("/css/a.h.v1.min.css", 1, STAMP, &[]),
        );
        let dir = tempfile::tempdir().unwrap();

        let css_jam = |media: &str| {
            Event::Jam(Jam {
                css: vec!["a".to_string()],
                media: Some(media.to_string()),
                ..Jam::default()
            })
        };
        let events = vec![
            css_jam("print"),
            Event::Dump(AssetType::Css),
            css_jam("screen"),
            Event::Dump(AssetType::Css),
            css_jam("print"),
            Event::Dump(AssetType::Css),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert_eq!(rendered.len(), 3);
        assert!(
            rendered[0].html.contains("media=\"print\""),
            "{}",
            rendered[0].html
        );
        assert!(
            rendered[1].html.contains("media=\"screen\""),
            "{}",
            rendered[1].html
        );
        // Same (module, media) again → nothing new to emit.
        assert!(rendered[2].html.is_empty(), "{}", rendered[2].html);
        assert!(rendered[2].urls.is_empty());
    }

    // ============================================================
    // Hash-qualified combo URLs
    // ============================================================

    /// The versioned_manifest shape with the qualified policy on — the
    /// fixture's recorded hashes are already the MD5 of each body.
    fn qualified_manifest() -> (Manifest, tempfile::TempDir) {
        let (mut manifest, dir) = versioned_manifest();
        manifest.hash_urls = true;
        (manifest, dir)
    }

    fn md5hex(bytes: &[u8]) -> String {
        format!("{:x}", md5::compute(bytes))
    }

    #[test]
    fn test_combo_qualified_url_serves_and_checks_hash() {
        let (manifest, dir) = qualified_manifest();
        let dirs = OutputDirs::under(dir.path());
        let (ha2, hb) = (md5hex(b"A2"), md5hex(b"B"));

        // The canonical qualified URL — dependency order (b before a), the
        // group's max stamp — serves year-cached.
        let url = format!("/paste/{SEG2}/b+v1.0-h{hb}~a+v2.0-h{ha2}.min.js");
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 200, "{url}");
        assert_eq!(r.body, "B\n;\nA2");
        assert_eq!(r.cache_control, "public, max-age=31536000");
        assert!(r.cacheable);

        // Uppercase hex is the same pin — canonicalizes to lowercase.
        let url = format!("/paste/{SEG2}/a+v2.0-h{}.min.js", ha2.to_uppercase());
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 302, "{url}");
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG2}/a+v2.0-h{ha2}.min.js").as_str())
        );

        // A wrong hash names different bytes — never redirected, never
        // served: 404 with no-cache.
        let url = format!("/paste/{SEG2}/a+v2.0-h{}.min.js", md5hex(b"not-a2"));
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 404, "{url}");
        assert!(!r.cacheable);
        assert_eq!(r.location, None);

        // History pins qualify the same way — the retained record's hash.
        let ha1 = md5hex(b"A1");
        let url = format!("/paste/{SEG}/a+v1.0-h{ha1}.min.js");
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 200, "{url}");
        assert_eq!(r.body, "A1");

        // A correct version pin with the current version's hash swapped in
        // still 404s — the hash must match THAT version's record.
        let url = format!("/paste/{SEG}/a+v1.0-h{ha2}.min.js");
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 404, "{url}");
    }

    #[test]
    fn test_combo_enabled_policy_redirects_legacy_to_qualified() {
        let (manifest, dir) = qualified_manifest();
        let dirs = OutputDirs::under(dir.path());
        let ha2 = md5hex(b"A2");

        // A legacy token under an enabled manifest is not the canonical URL:
        // 302 onto the qualified form, never a silent legacy 200.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG2}/a+v2.0.min.js"),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG2}/a+v2.0-h{ha2}.min.js").as_str())
        );

        // A fully canonical qualified list under a FUTURE stamp still 404s —
        // hash policy does not weaken the timestamp check.
        let url = format!(
            "/paste/{}/a+v2.0-h{ha2}.min.js",
            SEG2.parse::<i64>().unwrap() + 1
        );
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 404, "{url}");
        assert!(!r.cacheable);
    }

    #[test]
    fn test_combo_optout_serves_legacy_and_still_validates_qualified() {
        // versioned_manifest is the opt-out shape: hash_urls = false.
        let (manifest, dir) = versioned_manifest();
        let dirs = OutputDirs::under(dir.path());
        let (ha2, hb) = (md5hex(b"A2"), md5hex(b"B"));

        // Legacy URLs serve as before — and canonical redirects stay legacy.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG2}/a+v2.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A2");

        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG2}/a+v2.0~b+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG2}/b+v1.0~a+v2.0.min.js").as_str())
        );

        // A correctly qualified request still resolves under opt-out — its
        // own hash flips the canonical form, which it already is.
        let url = format!("/paste/{SEG2}/a+v2.0-h{ha2}.min.js");
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 200, "{url}");
        assert_eq!(r.body, "A2");

        // And a wrong hash still 404s under opt-out.
        let url = format!("/paste/{SEG2}/a+v2.0-h{}.min.js", md5hex(b"bogus"));
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 404, "{url}");

        // A mixed list under opt-out canonicalizes to all-qualified — the
        // qualified pin decides the form, not the flag.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG2}/b+v1.0-h{hb}~a+v2.0.min.js"),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG2}/b+v1.0-h{hb}~a+v2.0-h{ha2}.min.js").as_str())
        );
    }

    #[test]
    fn test_combo_mixed_list_under_enabled_policy_qualifies_all() {
        let (manifest, dir) = qualified_manifest();
        let dirs = OutputDirs::under(dir.path());
        let (ha2, hb) = (md5hex(b"A2"), md5hex(b"B"));

        // One qualified pin is enough to make the canonical form qualified;
        // the bare sibling picks up its hash in the redirect.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG2}/b+v1.0-h{hb}~a+v2.0.min.js"),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG2}/b+v1.0-h{hb}~a+v2.0-h{ha2}.min.js").as_str())
        );
    }

    #[test]
    fn test_combo_qualified_historical_order_is_preserved() {
        let (manifest, dir) = qualified_manifest();
        let dirs = OutputDirs::under(dir.path());
        let (ha1, hb) = (md5hex(b"A1"), md5hex(b"B"));

        // A historical pin keeps the LISTED order — a first, the way the
        // build that minted the URL layered it — even though today's
        // canonical order puts b first.
        let url = format!("/paste/{SEG}/a+v1.0-h{ha1}~b+v1.0-h{hb}.min.js");
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 200, "{url}");
        assert_eq!(r.body, "A1\n;\nB");
        assert!(r.cacheable);
    }

    #[test]
    fn test_combo_retired_asset_serves_only_its_exact_qualified_pin() {
        let (mut manifest, dir) = qualified_manifest();
        manifest.assets.get_mut("js/b.js").unwrap().retired = true;
        let dirs = OutputDirs::under(dir.path());
        let hb = md5hex(b"B");

        // The exact historical pin still serves.
        let url = format!("/paste/{SEG}/b+v1.0-h{hb}.min.js");
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 200, "{url}");
        assert_eq!(r.body, "B");

        // The bare name does not — retired entries never satisfy unpinned
        // lookups.
        let r = combo(&manifest, &dirs, "/paste", "/paste/b.min.js", None);
        assert_eq!(r.status, 404);

        // And a retired entry with a wrong hash still 404s.
        let url = format!("/paste/{SEG}/b+v1.0-h{}.min.js", md5hex(b"bogus"));
        let r = combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 404, "{url}");
    }

    #[test]
    fn test_combo_hash_mismatch_404s_even_for_a_filtered_member() {
        let (manifest, dir) = qualified_manifest();
        let dirs = OutputDirs::under(dir.path());
        let hb = md5hex(b"B");

        // `a` carries a wrong hash AND would be dropped by ?filter=a — the
        // hash check runs first, so the wrong pin can never be laundered
        // into a served or redirected response.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!(
                "/paste/{SEG2}/b+v1.0-h{hb}~a+v2.0-h{}.min.js",
                md5hex(b"bad")
            ),
            Some("filter=a"),
        );
        assert_eq!(r.status, 404);
        assert!(!r.cacheable);
        assert_eq!(r.location, None);
    }

    #[test]
    fn test_combo_conflicting_duplicate_hashes_404() {
        let (manifest, dir) = qualified_manifest();
        let dirs = OutputDirs::under(dir.path());
        let (ha2, other) = (md5hex(b"A2"), md5hex(b"other-hash-bytes-pad-pad!"));

        // Same module, two different hash pins — ambiguous, not served.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/123/a+v2.0-h{ha2}~a+v2.0-h{other}.min.js"),
            None,
        );
        assert_eq!(r.status, 404);

        // A bare pin and a qualified pin for the same module disagree.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/123/a+v2.0~a+v2.0-h{ha2}.min.js"),
            None,
        );
        assert_eq!(r.status, 404);

        // The same pin twice — hash case aside — still dedupes; the
        // uppercase spelling canonicalizes onto the lowercase form.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!(
                "/paste/{SEG2}/a+v2.0-h{}~a+v2.0-h{ha2}.min.js",
                ha2.to_uppercase()
            ),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG2}/a+v2.0-h{ha2}.min.js").as_str())
        );

        // And the lowercase duplicate of the canonical pin serves.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG2}/a+v2.0-h{ha2}~a+v2.0-h{ha2}.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A2");
    }

    #[test]
    fn test_combo_malformed_hash_suffixes_are_404() {
        let (manifest, dir) = qualified_manifest();
        let dirs = OutputDirs::under(dir.path());
        let ha2 = md5hex(b"A2");

        for path in [
            // empty, short, long, non-hex hashes
            "/paste/123/a+v2.0-h.min.js",
            "/paste/123/a+v2.0-h0123456789abcdef0123456789abc.min.js",
            "/paste/123/a+v2.0-h0123456789abcdef0123456789abcdef0.min.js",
            "/paste/123/a+v2.0-h0123456789abcdef0123456789abcg!.min.js",
            // a doubled qualifier and a foreign qualifier letter
            &format!("/paste/123/a+v2.0-h{ha2}-h{ha2}.min.js"),
            "/paste/123/a+v2.0-x0123456789abcdef0123456789abcdef.min.js",
            // trailing garbage after a valid hash
            &format!("/paste/123/a+v2.0-h{ha2}.9.min.js"),
            // hash marker with no version, and an overflowing version
            &format!("/paste/123/a+v-h{ha2}.min.js"),
            &format!("/paste/123/a+v99999999999.0-h{ha2}.min.js"),
        ] {
            let r = combo(&manifest, &dirs, "/paste", path, None);
            assert_eq!(r.status, 404, "{path}");
            assert_eq!(r.location, None, "{path}");
        }
    }

    #[test]
    fn test_combo_noncacheable_forms_validate_a_supplied_hash() {
        let (manifest, dir) = qualified_manifest();
        let dirs = OutputDirs::under(dir.path());
        let ha2 = md5hex(b"A2");

        // The unversioned single-segment form accepts a pin — and checks it.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/a+v2.0-h{ha2}.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A2");
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);

        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/a+v2.0-h{}.min.js", md5hex(b"bad")),
            None,
        );
        assert_eq!(r.status, 404);
        assert!(!r.cacheable);

        // require expands dependencies — and checks the pin on the listed
        // module before serving. Non-cacheable either way.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/require/a+v2.0-h{ha2}.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "B\n;\nA2");
        assert_eq!(r.cache_control, "no-cache");
        assert!(!r.cacheable);

        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/require/a+v2.0-h{}.min.js", md5hex(b"bad")),
            None,
        );
        assert_eq!(r.status, 404);
        assert!(!r.cacheable);
    }

    /// A supplied hash must be checked even when `filter` would drop the
    /// module from the served group — a filtered-out wrong pin is still a
    /// 404, never a laundering step onto a 200. Runs under both manifest
    /// policies: `qualified_manifest` (hash_urls on) and `versioned_manifest`
    /// (opt-out) carry the same recorded hashes.
    #[test]
    fn test_combo_filter_cannot_launder_a_wrong_hash() {
        let hb = md5hex(b"B");
        for (manifest, dir) in [qualified_manifest(), versioned_manifest()] {
            let dirs = OutputDirs::under(dir.path());
            let wrong = md5hex(b"bad");
            for (path, query) in [
                // require path segment, filtered listed module
                (
                    format!("/paste/require/a+v2.0-h{wrong}.min.js"),
                    Some("filter=a"),
                ),
                // ?require on the timestamped form, filtered listed module
                (
                    format!("/paste/{SEG2}/a+v2.0-h{wrong}.min.js"),
                    Some("require&filter=a"),
                ),
                // the unversioned form with a surviving filtered sibling
                (
                    format!("/paste/a+v2.0-h{wrong}~b+v1.0-h{hb}.min.js"),
                    Some("filter=a"),
                ),
            ] {
                let r = combo(&manifest, &dirs, "/paste", &path, query);
                assert_eq!(r.status, 404, "{path}?{}", query.unwrap_or(""));
                assert!(!r.cacheable);
                assert_eq!(r.location, None);
                assert_eq!(r.cache_control, "no-cache");
            }

            // Correct hashes under the same filters still serve what
            // remains — b alone — no-cache.
            let ha2 = md5hex(b"A2");
            for (path, query) in [
                (
                    format!("/paste/require/a+v2.0-h{ha2}.min.js"),
                    Some("filter=a"),
                ),
                (
                    format!("/paste/{SEG2}/a+v2.0-h{ha2}.min.js"),
                    Some("require&filter=a"),
                ),
                (
                    format!("/paste/a+v2.0-h{ha2}~b+v1.0-h{hb}.min.js"),
                    Some("filter=a"),
                ),
            ] {
                let r = combo(&manifest, &dirs, "/paste", &path, query);
                assert_eq!(r.status, 200, "{path}?{}", query.unwrap_or(""));
                assert_eq!(r.body, "B");
                assert!(!r.cacheable);
                assert_eq!(r.cache_control, "no-cache");
            }
        }
    }

    #[test]
    fn test_combo_continued_list_components_join_to_one_list() {
        let (manifest, dir) = qualified_manifest();
        let dirs = OutputDirs::under(dir.path());
        let (ha2, hb) = (md5hex(b"A2"), md5hex(b"B"));

        // The same canonical list split over two path components resolves
        // the same — component boundaries are separators, not tokens.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG2}/b+v1.0-h{hb}/a+v2.0-h{ha2}.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "B\n;\nA2");

        // A stale stamp on a segmented URL redirects to the normal
        // single-component canonical URL.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/1/b+v1.0-h{hb}/a+v2.0-h{ha2}.min.js"),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG2}/b+v1.0-h{hb}~a+v2.0-h{ha2}.min.js").as_str())
        );

        // Legacy lists segment the same way.
        let (manifest, dir) = combo_manifest();
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/a+v1.0/b+v1.0.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A\n;\nB");

        // A list component may not be empty — `/` inside the list means a
        // continuation boundary, never a stray segment.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/a+v1.0//b+v1.0.js"),
            None,
        );
        assert_eq!(r.status, 404);
    }
}
