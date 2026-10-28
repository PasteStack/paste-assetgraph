// Copyright (c) 2017–present Tomshley LLC
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::graph;
use crate::manifest::{is_content_hash, stamp_epoch, Manifest, ResolvedVersion};

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

/// Which queued modules a dump emits: one asset type, or `all` — the css
/// groups then the js groups in a single dump event.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DumpScope {
    Css,
    Js,
    All,
}

/// A `jam` event: modules queued for the next dump of each type it names.
/// The same options apply to both queues — `inline` forces inlining past the
/// threshold, `sync` swaps the async loader for a script src, `media` lands
/// on the stylesheet tag, `condition` wraps the emitted tags in a downlevel
/// conditional comment (`<!--[if …]>`…`<![endif]-->`), and `perModule`
/// overrides the render's per-module setting for this event's groups in
/// either direction (`None` inherits). The event vocabulary is closed: a
/// misspelled key is a parse error, never a silent default — the adapters
/// enforce the same contract at their edge.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default, rename = "perModule")]
    pub per_module: Option<bool>,
}

/// One render event: `{"jam": {...}}` queues modules, `{"dump": "css"}` or
/// `{"dump": "js"}` emits that type's queued tags, `{"dump": "all"}` emits
/// both queues in one dump — css tags then js tags — and the bare string
/// `"finish"` seals the page — every jammed module must have been dumped
/// by then, or the render fails instead of silently dropping it.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Event {
    Jam(Jam),
    Dump(DumpScope),
    Finish,
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
    /// Deliver each resolved module through its own versioned file instead
    /// of one combo URL per group — same expansion, ordering, and
    /// loaded-set filtering. Async groups serialize their external files in
    /// dependency order through one loader block; files after the first
    /// external request stay external even when inlinable, so nothing can
    /// run before its dependency. A jam event's own `perModule` overrides
    /// this per group.
    pub per_module: bool,
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
/// fingerprint of the JS loader identity — the first 12 hex chars of md5
/// over each `name:key:version:hash` line from the manifest's loader pins,
/// declared aliases and conventional names alike, so a metadata-only
/// version increment or an alias mapping change also moves the segment
/// even when the emitted bytes are identical; followed by the ordered
/// module names from `sorted_deps` so a dependency edit that changes the
/// layering changes it too. Retired assets are absent from the pins: they
/// are not part of what a fresh loader URL may serve. It changes whenever
/// any JS identity or ordering changes, so a deterministic build (whose
/// manifest version is pinned) still gets a fresh segment — the
/// versioned-form URLs it feeds are cached for a year.
pub(crate) fn packages_segment(manifest: &Manifest) -> String {
    let mut context = md5::Context::new();
    for (name, pin) in manifest.loader_pins() {
        context.consume(format!("{name}:{}:{}:{}\n", pin.key, pin.version, pin.hash));
    }
    if let Some(deps) = manifest.sorted_deps.get("js") {
        for (module, _, _) in deps {
            context.consume(format!("{module}\n"));
        }
    }
    format!("{:x}", context.compute())[..12].to_string()
}

/// The fingerprint builds emitted before pins carried version and alias
/// identity: `key:hash` lines over the active JS assets plus the
/// sorted-deps order — the same bytes the old segment function produced.
/// Kept verbatim for the first-upgrade migration: a build records this
/// segment of the state it upgrades from so pages that saved it keep
/// resolving through the recorded pin map.
pub(crate) fn legacy_packages_segment(manifest: &Manifest) -> String {
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
/// The last event must be a dump or `finish` — a trailing jam would queue
/// modules no dump ever emits. A terminal `finish` adds no `Rendered`; it
/// only validates that every jammed module was delivered.
///
/// Request-time callers that need only the terminal dump use
/// [`render_last`] — this form exists for callers that explicitly want
/// every dump's markup.
pub fn render_all(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    events: &[Event],
) -> Result<Vec<Rendered>> {
    validate_options(opts)?;

    let planned = plan_events(manifest, events)?;
    let mut rendered = Vec::with_capacity(planned.len());
    for dump in &planned {
        rendered.push(render_planned_dump(manifest, dirs, opts, dump)?);
    }
    Ok(rendered)
}

/// Emit one planned dump. The page-level per-module setting is the
/// static-host declaration: it controls whether the runtime bootstrap
/// may fall back to a combo URL. It must come from the render settings —
/// the page API is incremental, so a jam's own `perModule` override
/// cannot retroactively change a bootstrap the first dump already
/// emitted. A page served without a combo handler sets the render-level
/// flag.
fn render_planned_dump(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    dump: &PlannedDump,
) -> Result<Rendered> {
    let per_module_bootstrap = opts.per_module;
    match dump {
        PlannedDump::Css(groups) => dump_css(manifest, dirs, opts, groups),
        PlannedDump::Js { bootstrap, groups } => dump_js(
            manifest,
            dirs,
            opts,
            bootstrap.as_deref(),
            groups,
            per_module_bootstrap,
        ),
        PlannedDump::All {
            css_groups,
            js_bootstrap,
            js_groups,
        } => {
            let css = dump_css(manifest, dirs, opts, css_groups)?;
            let js = dump_js(
                manifest,
                dirs,
                opts,
                js_bootstrap.as_deref(),
                js_groups,
                per_module_bootstrap,
            )?;
            Ok(merge_dump(css, js))
        }
    }
}

/// The request-time form: the complete event list is still planned —
/// expansion, ordering and loaded-set dedup need every earlier event —
/// but only the terminal dump is emitted. The earlier dumps' markup was
/// already delivered to the browser; re-rendering them would reopen
/// compiled files this answer does not need and could fail a valid dump
/// over an unrelated asset that vanished after its own dump succeeded.
/// An event list that yields no dump answers empty markup.
pub(crate) fn render_last(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    events: &[Event],
) -> Result<Rendered> {
    validate_options(opts)?;
    let planned = plan_events(manifest, events)?;
    match planned.last() {
        Some(dump) => render_planned_dump(manifest, dirs, opts, dump),
        None => Ok(Rendered {
            html: String::new(),
            urls: Vec::new(),
        }),
    }
}

/// One `Rendered` out of a combined dump's two halves: the css tags then
/// the js tags joined by a newline only when both sides rendered, and the
/// external URLs in the same order.
fn merge_dump(css: Rendered, js: Rendered) -> Rendered {
    let html = if css.html.is_empty() {
        js.html
    } else if js.html.is_empty() {
        css.html
    } else {
        format!("{}\n{}", css.html, js.html)
    };
    let mut urls = css.urls;
    urls.extend(js.urls);
    Rendered { html, urls }
}

/// Seal a complete page without rendering: the last event must be
/// `finish` and every module any jam queued must have been emitted by a
/// dump of its type — a css module under the same media attribute counts
/// delivered, under a different one it does not. Validation replays the
/// same plan the renderer uses, so it expands, orders and dedups exactly
/// as a render would but never opens an asset file.
pub(crate) fn validate_finished(manifest: &Manifest, events: &[Event]) -> Result<()> {
    if !matches!(events.last(), Some(Event::Finish)) {
        bail!("the last render event must be finish");
    }
    plan_events(manifest, events).map(|_| ())
}

/// One jam's contribution to a dump: the queue entry whose options shape
/// the tag and the modules it still has to deliver after already-loaded
/// ones are subtracted.
struct PlannedGroup<'a> {
    jam: &'a Jam,
    modules: Vec<String>,
}

/// A dump's plan: what its tags will cover. Computed before any tag is
/// rendered — expansion, ordering and loaded bookkeeping live here once so
/// `finish` can apply the same rules without re-reading asset files.
enum PlannedDump<'a> {
    Css(Vec<PlannedGroup<'a>>),
    Js {
        /// The runtime group, present only on the first js dump that has
        /// pending jams and has not delivered `paste` yet.
        bootstrap: Option<Vec<String>>,
        groups: Vec<PlannedGroup<'a>>,
    },
    All {
        css_groups: Vec<PlannedGroup<'a>>,
        /// Same rule as the `Js` variant's bootstrap.
        js_bootstrap: Option<Vec<String>>,
        js_groups: Vec<PlannedGroup<'a>>,
    },
}

/// Plan the whole event list: expand every queued jam at its dump, drop
/// already-loaded modules, and remember what each dump delivered. The last
/// event must be a dump or `finish`; `finish` anywhere else is an error,
/// and a `finish` that still has undumped modules fails with their names.
fn plan_events<'a>(manifest: &Manifest, events: &'a [Event]) -> Result<Vec<PlannedDump<'a>>> {
    if !matches!(events.last(), Some(Event::Dump(_) | Event::Finish)) {
        bail!("the last render event must be a dump or finish");
    }

    // Both loaded sets key on (module, media) — the delivery key — so one
    // plan covers css (keyed per jam media) and js (always None) alike.
    let mut loaded_js: HashSet<(String, Option<String>)> = HashSet::new();
    let mut loaded_css: HashSet<(String, Option<String>)> = HashSet::new();
    let mut pending_js: Vec<&Jam> = Vec::new();
    let mut pending_css: Vec<&Jam> = Vec::new();
    let mut dumps = Vec::new();

    for (i, event) in events.iter().enumerate() {
        match event {
            Event::Jam(jam) => {
                if let Some(condition) = jam.condition.as_deref() {
                    validate_condition(condition)?;
                }
                if !jam.css.is_empty() {
                    pending_css.push(jam);
                }
                if !jam.js.is_empty() {
                    pending_js.push(jam);
                }
            }
            Event::Dump(DumpScope::Css) => {
                dumps.push(PlannedDump::Css(plan_groups(
                    manifest,
                    &pending_css,
                    AssetType::Css,
                    &mut loaded_css,
                )?));
                pending_css.clear();
            }
            Event::Dump(DumpScope::Js) => {
                dumps.push(plan_js(manifest, &pending_js, &mut loaded_js)?);
                pending_js.clear();
            }
            Event::Dump(DumpScope::All) => {
                dumps.push(plan_all(
                    manifest,
                    &pending_css,
                    &mut loaded_css,
                    &pending_js,
                    &mut loaded_js,
                )?);
                pending_css.clear();
                pending_js.clear();
            }
            Event::Finish => {
                if i + 1 != events.len() {
                    bail!("finish must be the last render event");
                }
                let mut missing = Vec::new();
                let mut seen = HashSet::new();
                for (pending, asset_type, loaded) in [
                    (&pending_css, AssetType::Css, &loaded_css),
                    (&pending_js, AssetType::Js, &loaded_js),
                ] {
                    for jam in pending.iter().copied() {
                        let delivery = delivery_key(jam, asset_type);
                        for m in expand(manifest, jam_modules(jam, asset_type), asset_type)?
                            .into_iter()
                            .filter(|m| !loaded.contains(&(m.clone(), delivery.clone())))
                        {
                            // The same stylesheet under a different media is
                            // a genuinely different tag, so name it — quoted
                            // so metacharacters cannot smear the diagnostic.
                            let label = match (asset_type, &jam.media) {
                                (AssetType::Css, Some(media)) => {
                                    format!("css module {m} (media {media:?})")
                                }
                                (AssetType::Css, None) => format!("css module {m}"),
                                (AssetType::Js, _) => format!("js module {m}"),
                            };
                            if seen.insert(label.clone()) {
                                missing.push(label);
                            }
                        }
                    }
                }
                if !missing.is_empty() {
                    bail!(
                        "finish: jammed modules were never dumped: {}",
                        missing.join(", ")
                    );
                }
            }
        }
    }

    Ok(dumps)
}

/// The CDN prefix contract, shared by build configuration and runtime
/// rendering: the prefix is joined to asset paths verbatim, so anything
/// that would move the path into a query or fragment (`?`, `#`) or break
/// out of an attribute or markup context is refused — a malformed prefix
/// fails validation before a URL is ever emitted from it.
pub(crate) fn validate_cdn_prefix(cdn_base: &str) -> Result<()> {
    if cdn_base.chars().any(|c| {
        c.is_whitespace()
            || c.is_control()
            || matches!(c, '\'' | '"' | '<' | '>' | '`' | '\\' | '?' | '#')
    }) {
        bail!("cdn_base must not contain '?', '#', quotes, angle brackets, backticks, backslashes, whitespace or control characters");
    }
    // A nonempty prefix must produce document-location-independent URLs:
    // a root-relative path (`/assets`), a protocol-relative
    // `//authority`, or a scheme-qualified `http(s)://authority`. A bare
    // relative prefix (`cdn`, `assets/x`) would join into a path whose
    // meaning moves with the page's own location; a bare or truncated
    // scheme (`https:` — what a `// comment` parse-truncation leaves
    // behind) and a missing authority (`https://`, `https:///x`, `//`)
    // would join into URLs that fetch nothing.
    if cdn_base.is_empty() {
        return Ok(());
    }
    if let Some(rest) = cdn_base.strip_prefix("//") {
        let authority = rest.split('/').next().unwrap_or_default();
        anyhow::ensure!(
            !authority.is_empty(),
            "protocol-relative cdn_base {cdn_base:?} has no authority"
        );
        validate_cdn_authority(cdn_base, authority)?;
    } else if cdn_base.starts_with('/') {
        // Root-relative path — already sanitized by the character check.
    } else if let Some(scheme_end) = cdn_base.find(':') {
        let scheme = &cdn_base[..scheme_end];
        anyhow::ensure!(
            matches!(scheme, "http" | "https"),
            "cdn_base scheme must be http or https, got {cdn_base:?}"
        );
        let rest = &cdn_base[scheme_end + 1..];
        let authority = rest.strip_prefix("//").and_then(|r| r.split('/').next());
        anyhow::ensure!(
            authority.is_some_and(|a| !a.is_empty()),
            "cdn_base {cdn_base:?} has no authority — expected `scheme://host/path`"
        );
        validate_cdn_authority(cdn_base, authority.unwrap_or_default())?;
    } else {
        bail!(
            "cdn_base {cdn_base:?} is a relative prefix — expected `https://host/path`, `//host/path`, `/path`, or an empty string"
        );
    }
    Ok(())
}

/// The authority of a CDN prefix: a host that is either a bracketed IP
/// literal or a dotted reg-name, then an optional numeric port. Userinfo
/// (`user@host`) is refused — it has no place in an asset origin and
/// hides the real host from a casual read of the config. A malformed
/// authority — an empty host, stray colons, a non-numeric or
/// out-of-range port, a doubled or leading dot, a dangling percent
/// escape, a character with no defined meaning in an authority —
/// produces joined URLs that fetch nothing, so the prefix fails here.
fn validate_cdn_authority(cdn_base: &str, authority: &str) -> Result<()> {
    anyhow::ensure!(
        !authority.contains('@'),
        "cdn_base {cdn_base:?} has userinfo — expected `scheme://host[:port]/path`"
    );
    let (host, port) = if authority.starts_with('[') {
        // A bracketed literal takes only `]` or `]:port` after the
        // closing bracket — `[::1]x` is malformed.
        match authority.split_once(']') {
            Some((literal, "")) => (&literal[1..], None),
            Some((literal, rest)) if rest.starts_with(':') => (&literal[1..], Some(&rest[1..])),
            _ => bail!("cdn_base {cdn_base:?} has a malformed IP literal"),
        }
    } else {
        match authority.split_once(':') {
            Some((host, port)) => {
                anyhow::ensure!(
                    !port.contains(':'),
                    "cdn_base {cdn_base:?} has a malformed authority"
                );
                (host, Some(port))
            }
            None => (authority, None),
        }
    };
    anyhow::ensure!(!host.is_empty(), "cdn_base {cdn_base:?} has no host");
    if let Some(port) = port {
        anyhow::ensure!(
            // Digits only: `u16::from_str` also takes a leading `+`, which
            // the URL parser browsers use rejects.
            !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u16>().is_ok(),
            "cdn_base {cdn_base:?} has a malformed port — expected 0-65535"
        );
    }
    if authority.starts_with('[') {
        // Browsers parse bracketed authorities as IPv6 and nothing
        // else: RFC 3986's IPvFuture spellings, `%zone` suffixes, and
        // malformed groups all fail WHATWG parsing, so a literal that
        // survives here must be a genuine IPv6 address.
        anyhow::ensure!(
            is_ipv6_address(host),
            "cdn_base {cdn_base:?} has a malformed IP literal"
        );
    } else {
        // Decode intentional internationalized host spellings before
        // checking label structure and numeric tails. ASCII escapes are
        // refused: even a benign one conceals the authority's spelling.
        let decoded = validate_host_characters(cdn_base, host)?;
        // A reg-name's dotted labels must each be nonempty — `.example`
        // or `cdn..example` resolve nothing. One trailing dot, an
        // absolute DNS name, is permitted.
        let labels = decoded.strip_suffix('.').unwrap_or(&decoded);
        anyhow::ensure!(
            labels.split('.').all(|label| !label.is_empty()),
            "cdn_base {cdn_base:?} has a malformed host"
        );
        // A reg-name ending in a numeric label is not a reg-name to a
        // browser — WHATWG reads it as an IPv4 spelling and either
        // rejects it outright (`123.456.789`, `cdn.0x10`) or silently
        // resolves a different host (`1234` → `0.0.4.210`, `010.0.0.1`
        // → `8.0.0.1`). Only a strict dotted quad is accepted as a
        // numeric authority.
        let last = labels.rsplit('.').next().unwrap_or_default();
        let numeric_tail = last.bytes().all(|b| b.is_ascii_digit())
            || last
                .strip_prefix("0x")
                .or_else(|| last.strip_prefix("0X"))
                .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()));
        if numeric_tail {
            anyhow::ensure!(
                is_strict_ipv4(labels),
                "cdn_base {cdn_base:?} has a numeric host that is not a dotted IPv4 literal"
            );
        }
    }
    Ok(())
}

/// An IPv6 address as a browser's WHATWG parser accepts it: hex groups
/// of 1-4 digits separated by `:`, one optional `::` compressing at
/// least one group, and a strict dotted IPv4 quad allowed as the final
/// group only — it occupies the address's last 32 bits.
fn is_ipv6_address(host: &str) -> bool {
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.'))
    {
        return false;
    }
    let (head, tail) = match host.split_once("::") {
        Some((head, tail)) if !tail.contains("::") => (head, Some(tail)),
        Some(_) => return false,
        None => (host, None),
    };
    let mut groups: Vec<&str> = Vec::new();
    if !head.is_empty() {
        groups.extend(head.split(':'));
    }
    if let Some(tail) = tail {
        if !tail.is_empty() {
            groups.extend(tail.split(':'));
        }
    }
    let mut size = 0;
    for (index, group) in groups.iter().enumerate() {
        let last = index == groups.len() - 1;
        size += if group.is_empty() {
            return false;
        } else if group.contains('.') {
            // The quad is a tail — under `::` it must come from the
            // tail side, or expansion would place it mid-address.
            if !last || tail.is_some_and(|t| t.is_empty()) || !is_strict_ipv4(group) {
                return false;
            }
            2
        } else if group.len() <= 4 {
            1
        } else {
            return false;
        };
    }
    if tail.is_some() {
        size < 8
    } else {
        size == 8
    }
}

/// Four decimal labels of 0-255, no leading zeros — the only numeric
/// host worth accepting. A browser reads every other numeric spelling
/// (`1234`, `0x7f.1`, `123.45.67`, `010.0.0.1`) as a *different* IPv4
/// address than the text suggests.
fn is_strict_ipv4(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() == 4
        && labels.iter().all(|label| {
            label.bytes().all(|b| b.is_ascii_digit())
                && (label.len() == 1 || !label.starts_with('0'))
                && label.parse::<u8>().is_ok()
        })
}

/// Percent-decode the authority into UTF-8 *before* semantic checks.
/// An ASCII escape can change DNS label/numeric-host interpretation, or
/// quietly hide the spelling of the actual origin, so it is forbidden.
/// Valid UTF-8 non-ASCII alphanumerics are kept for explicitly encoded
/// internationalized names; malformed sequences, IDNA separators and
/// reserved/markup characters are refused. Path escapes are separate.
fn validate_host_characters(cdn_base: &str, host: &str) -> Result<String> {
    let raw = host.as_bytes();
    let mut decoded = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            let byte = match (raw.get(i + 1), raw.get(i + 2)) {
                (Some(a), Some(b)) => match (hex(*a), hex(*b)) {
                    (Some(hi), Some(lo)) => hi * 16 + lo,
                    _ => anyhow::bail!("cdn_base {cdn_base:?} has an invalid hostname escape"),
                },
                _ => anyhow::bail!("cdn_base {cdn_base:?} has an incomplete hostname escape"),
            };
            anyhow::ensure!(
                !byte.is_ascii(),
                "cdn_base {cdn_base:?} encodes an ASCII hostname byte — use the literal origin spelling"
            );
            decoded.push(byte);
            i += 3;
        } else {
            anyhow::ensure!(
                raw[i].is_ascii_alphanumeric() || matches!(raw[i], b'-' | b'_' | b'.' | b'~'),
                "cdn_base {cdn_base:?} has an invalid hostname character"
            );
            decoded.push(raw[i]);
            i += 1;
        }
    }
    let result = String::from_utf8(decoded).map_err(|_| {
        anyhow::anyhow!("cdn_base {cdn_base:?} has malformed UTF-8 in its hostname")
    })?;
    anyhow::ensure!(
        result.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '~')
                || (!c.is_ascii() && c.is_alphanumeric())
        }),
        "cdn_base {cdn_base:?} has a forbidden internationalized hostname character"
    );
    Ok(result)
}

/// Join the CDN prefix to a URL path with exactly one `/` separator —
/// `https://cdn.example/assets` and `https://cdn.example/assets/` produce
/// the same URL. Resource paths in this system always begin with `/`.
pub(crate) fn join_url(prefix: &str, path: &str) -> String {
    format!("{}{}", prefix.trim_end_matches('/'), path)
}

pub(crate) fn validate_options(opts: &RenderOptions) -> Result<()> {
    validate_cdn_prefix(&opts.cdn_base)?;
    validate_base_path(&opts.base_path)
}

/// `base_path` is an absolute ASCII URL path of nonempty, normal segments:
/// it is split off combo request paths verbatim, lands inside HTML
/// attributes and is embedded in the host's route pattern, so `.`/`..`
/// segments, a trailing slash, non-ASCII bytes, whitespace, control
/// characters, the URL/markup metacharacters and the route wildcards `*`
/// and `{}` are all refused.
pub(crate) fn validate_base_path(base_path: &str) -> Result<()> {
    if !base_path.is_ascii()
        || !base_path.starts_with('/')
        || base_path
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || base_path.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(
                    c,
                    '\'' | '"' | '<' | '>' | '`' | '\\' | '?' | '#' | '%' | ':' | '*' | '{' | '}'
                )
        })
    {
        bail!("base_path must be an absolute ASCII URL path with nonempty, normal segments");
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

/// The module list a jam queues for `asset_type`.
fn jam_modules(jam: &Jam, asset_type: AssetType) -> &[String] {
    match asset_type {
        AssetType::Css => &jam.css,
        AssetType::Js => &jam.js,
    }
}

/// A jam's delivery key: a stylesheet requested with a different `media`
/// attribute is a genuinely different tag, so css delivers per
/// (module, media) while js delivers per module.
fn delivery_key(jam: &Jam, asset_type: AssetType) -> Option<String> {
    match asset_type {
        AssetType::Css => jam.media.clone(),
        AssetType::Js => None,
    }
}

/// Expand each queued jam for `asset_type`, drop the modules already
/// delivered under the same delivery key, and mark the survivors loaded.
fn plan_groups<'a>(
    manifest: &Manifest,
    pending: &[&'a Jam],
    asset_type: AssetType,
    loaded: &mut HashSet<(String, Option<String>)>,
) -> Result<Vec<PlannedGroup<'a>>> {
    let mut groups = Vec::new();
    for &jam in pending {
        let delivery = delivery_key(jam, asset_type);
        let group: Vec<String> = expand(manifest, jam_modules(jam, asset_type), asset_type)?
            .into_iter()
            .filter(|m| !loaded.contains(&(m.clone(), delivery.clone())))
            .collect();
        if group.is_empty() {
            continue;
        }
        // A conditional js group is not a delivery a later unconditional
        // jam can rely on — the original filtered against a copy of the
        // loaded set for exactly that reason, so the module stays
        // deliverable. Stylesheet conditionals did mark loaded; the
        // asymmetry is the recorded contract, not an oversight here.
        if asset_type != AssetType::Js || jam_condition(jam).is_none() {
            loaded.extend(group.iter().map(|m| (m.clone(), delivery.clone())));
        }
        groups.push(PlannedGroup {
            jam,
            modules: group,
        });
    }
    Ok(groups)
}

/// Plan a js dump: the runtime bootstrap leads exactly once — on the first
/// js dump with pending jams that has not already delivered `paste` — then
/// each queued jam's surviving group.
fn plan_js<'a>(
    manifest: &Manifest,
    pending: &[&'a Jam],
    loaded: &mut HashSet<(String, Option<String>)>,
) -> Result<PlannedDump<'a>> {
    // The runtime bootstrap follows unconditional js only — a conditional
    // group cannot carry it (its wrapper may not match), matching the
    // original's "don't update the modules if there is a conditional".
    let unconditional = pending.iter().any(|jam| jam_condition(jam).is_none());
    let bootstrap = if unconditional && !loaded.contains(&("paste".to_string(), None)) {
        let group: Vec<String> = expand(manifest, &["paste".to_string()], AssetType::Js)?
            .into_iter()
            .filter(|m| !loaded.contains(&(m.clone(), None)))
            .collect();
        loaded.extend(group.iter().map(|m| (m.clone(), None)));
        Some(group)
    } else {
        None
    };

    let groups = plan_groups(manifest, pending, AssetType::Js, loaded)?;
    Ok(PlannedDump::Js { bootstrap, groups })
}

/// Plan a combined dump: the css groups then the js plan — bootstrap
/// included — against the page's loaded state, so one dump delivers
/// exactly what a css dump followed by a js dump would.
fn plan_all<'a>(
    manifest: &Manifest,
    pending_css: &[&'a Jam],
    loaded_css: &mut HashSet<(String, Option<String>)>,
    pending_js: &[&'a Jam],
    loaded_js: &mut HashSet<(String, Option<String>)>,
) -> Result<PlannedDump<'a>> {
    let css_groups = plan_groups(manifest, pending_css, AssetType::Css, loaded_css)?;
    let PlannedDump::Js {
        bootstrap,
        groups: js_groups,
    } = plan_js(manifest, pending_js, loaded_js)?
    else {
        unreachable!("plan_js always plans a Js dump")
    };
    Ok(PlannedDump::All {
        css_groups,
        js_bootstrap: bootstrap,
        js_groups,
    })
}

/// A jam's `condition`, normalized: an empty attribute behaves as absent,
/// exactly as the original's `node.get('condition')` falsy check did.
fn jam_condition(jam: &Jam) -> Option<&str> {
    jam.condition.as_deref().filter(|c| !c.is_empty())
}

/// The condition expression lands inside an HTML comment
/// (`<!--[if {cond}]>`): anything that could close the comment early,
/// break the wrapper's own brackets, or smuggle control bytes is refused
/// at plan time rather than emitted.
fn validate_condition(condition: &str) -> Result<()> {
    anyhow::ensure!(
        condition.is_empty()
            || (!condition.bytes().any(|b| b < 0x20 || b == 0x7f)
                && !condition.contains('<')
                && !condition.contains('>')
                && !condition.contains("--")),
        "jam condition cannot be written inside an HTML comment: {condition:?}"
    );
    Ok(())
}

/// One emitted tag in the downlevel conditional wrapper the original
/// produced: `<!--[if {cond}]>` newline, the tag, newline, `<![endif]-->`.
fn conditional_wrap(condition: Option<&str>, tag: String) -> String {
    match condition {
        Some(cond) => format!("<!--[if {cond}]>\n{tag}\n<![endif]-->"),
        None => tag,
    }
}

/// Run every planned group through `tag`, collecting tags and external
/// URLs in plan order — the loop both dump renders share. A group emits
/// its tags as one newline-joined block (combo mode's one tag, or one tag
/// per module under the per-module setting) and every external URL it
/// referenced.
fn dump_groups(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    groups: &[PlannedGroup],
    tag: impl Fn(
        &Manifest,
        &OutputDirs,
        &RenderOptions,
        &Jam,
        &[String],
    ) -> Result<(String, Vec<String>)>,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut tags = Vec::new();
    let mut urls = Vec::new();
    for planned in groups {
        let (t, group_urls) = tag(manifest, dirs, opts, planned.jam, &planned.modules)?;
        urls.extend(group_urls);
        tags.push(t);
    }
    Ok((tags, urls))
}

fn dump_css(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    groups: &[PlannedGroup],
) -> Result<Rendered> {
    let (tags, urls) = dump_groups(manifest, dirs, opts, groups, css_tag)?;
    Ok(Rendered {
        html: tags.join("\n"),
        urls,
    })
}

fn dump_js(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    bootstrap: Option<&[String]>,
    groups: &[PlannedGroup],
    per_module_bootstrap: bool,
) -> Result<Rendered> {
    let mut tags = Vec::new();
    let mut urls = Vec::new();

    if let Some(group) = bootstrap {
        let packages = packages_segment(manifest);
        let config = format!(
            "paste.config({{packagesUrl:'{}'}});",
            join_url(&opts.cdn_base, &format!("{}/{}/", opts.base_path, packages))
        );

        // The bootstrap is never conditional, so `-->` in the runtime is
        // harmless script data here.
        match inline_content(manifest, dirs, AssetType::Js, true, 0, group, false) {
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
            None if per_module_bootstrap => {
                // A per-module page has no combo handler behind it: the
                // runtime's own versioned files load synchronously in
                // dependency order, then the config tag as usual.
                for module in group {
                    let path = per_module_output(manifest, dirs, AssetType::Js, module)?;
                    tags.push(format!(
                        "<script type=\"text/javascript\" src=\"{}\"></script>",
                        escape_attr(&join_url(&opts.cdn_base, &path))
                    ));
                    urls.push(path);
                }
                tags.push(format!(
                    "<script type=\"text/javascript\">{}</script>",
                    config
                ));
            }
            None => {
                let refs: Vec<&str> = group.iter().map(String::as_str).collect();
                let path =
                    manifest.jam_url_with_limit(&refs, "js", &opts.base_path, false, None)?;
                eprintln!(
                    "warning: paste runtime could not be inlined, serving {}",
                    path
                );
                tags.push(format!(
                    "<script type=\"text/javascript\" src=\"{}\"></script>",
                    escape_attr(&join_url(&opts.cdn_base, &path))
                ));
                tags.push(format!(
                    "<script type=\"text/javascript\">{}</script>",
                    config
                ));
                urls.push(path);
            }
        }
    }

    let (group_tags, group_urls) = dump_groups(manifest, dirs, opts, groups, js_tag)?;
    tags.extend(group_tags);
    urls.extend(group_urls);

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
) -> Result<(String, Vec<String>)> {
    if jam.per_module.unwrap_or(opts.per_module) {
        return per_module_js_tags(manifest, dirs, opts, jam, group);
    }

    if let Some(content) = inline_content(
        manifest,
        dirs,
        AssetType::Js,
        jam.inline,
        opts.inline_threshold,
        group,
        jam_condition(jam).is_some(),
    ) {
        return Ok((
            conditional_wrap(
                jam_condition(jam),
                format!("<script type=\"text/javascript\">{}</script>", content),
            ),
            Vec::new(),
        ));
    }

    let refs: Vec<&str> = group.iter().map(String::as_str).collect();
    let path = manifest.jam_url_with_limit(&refs, "js", &opts.base_path, false, None)?;
    let url = join_url(&opts.cdn_base, &path);
    let tag = if jam.sync {
        format!(
            "<script type=\"text/javascript\" src=\"{}\"></script>",
            escape_attr(&url)
        )
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
    Ok((conditional_wrap(jam_condition(jam), tag), vec![path]))
}

/// Per-module JS emission. The group's files run in planned order and
/// keep top-level global scope, exactly as a combo response does: under
/// `sync` each module is its own inline tag or `script src` in order;
/// async emits one loader block that requests the versioned files in
/// order through `paste.require(url, callback)` — each request fires its
/// continuation, which issues the next, rather than nested callbacks
/// (linear markup for any group size). Once an async file goes external,
/// every later file in the group stays external — inlining it would run
/// it before the file it depends on — while files before the first
/// external may still inline, since nothing waits on them. All marks
/// land before the first request so a not-yet-defined external module
/// never triggers a bare-name fetch.
fn per_module_js_tags(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    jam: &Jam,
    group: &[String],
) -> Result<(String, Vec<String>)> {
    let mut tags = Vec::with_capacity(group.len());
    let mut urls = Vec::with_capacity(group.len());
    let mut marks = Vec::new();
    for module in group {
        let content = if jam.sync || urls.is_empty() {
            inline_content(
                manifest,
                dirs,
                AssetType::Js,
                jam.inline,
                opts.inline_threshold,
                std::slice::from_ref(module),
                jam_condition(jam).is_some(),
            )
        } else {
            None
        };
        if let Some(content) = content {
            tags.push(conditional_wrap(
                jam_condition(jam),
                format!("<script type=\"text/javascript\">{}</script>", content),
            ));
            continue;
        }
        let path = per_module_output(manifest, dirs, AssetType::Js, module)?;
        if jam.sync {
            tags.push(conditional_wrap(
                jam_condition(jam),
                format!(
                    "<script type=\"text/javascript\" src=\"{}\"></script>",
                    escape_attr(&join_url(&opts.cdn_base, &path))
                ),
            ));
        } else if module != "paste" {
            marks.push(format!("d('{}').isLoading", module));
        }
        urls.push(path);
    }
    if !jam.sync && !urls.is_empty() {
        let assign = if marks.is_empty() {
            String::new()
        } else {
            format!("{}=!0;", marks.join("="))
        };
        let loader = if urls.len() == 1 {
            format!("{}r('{}');", assign, join_url(&opts.cdn_base, &urls[0]))
        } else {
            let sources = urls
                .iter()
                .map(|path| format!("'{}'", join_url(&opts.cdn_base, path)))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "var u=[{}],i=0,n=function(){{var v;if(i<u.length){{v=u[i];i+=1;r(v,n);}}}};{}n();",
                sources, assign
            )
        };
        tags.push(conditional_wrap(
            jam_condition(jam),
            format!(
                "<script type=\"text/javascript\">(function(d,r){{{}}})(paste.define,paste.require);</script>",
                loader
            ),
        ));
    }
    Ok((tags.join("\n"), urls))
}

fn css_tag(
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
    jam: &Jam,
    group: &[String],
) -> Result<(String, Vec<String>)> {
    let media = jam
        .media
        .as_deref()
        .map(|m| format!(" media=\"{}\"", escape_attr(m)))
        .unwrap_or_default();

    if jam.per_module.unwrap_or(opts.per_module) {
        let mut tags = Vec::with_capacity(group.len());
        let mut urls = Vec::with_capacity(group.len());
        for module in group {
            if let Some(content) = inline_content(
                manifest,
                dirs,
                AssetType::Css,
                jam.inline,
                opts.inline_threshold,
                std::slice::from_ref(module),
                jam_condition(jam).is_some(),
            ) {
                tags.push(conditional_wrap(
                    jam_condition(jam),
                    format!("<style type=\"text/css\"{}>{}</style>", media, content),
                ));
                continue;
            }
            let path = per_module_output(manifest, dirs, AssetType::Css, module)?;
            tags.push(conditional_wrap(
                jam_condition(jam),
                format!(
                    "<link rel=\"stylesheet\" href=\"{}\"{}/>",
                    escape_attr(&join_url(&opts.cdn_base, &path)),
                    media
                ),
            ));
            urls.push(path);
        }
        return Ok((tags.join("\n"), urls));
    }

    if let Some(content) = inline_content(
        manifest,
        dirs,
        AssetType::Css,
        jam.inline,
        opts.inline_threshold,
        group,
        jam_condition(jam).is_some(),
    ) {
        return Ok((
            conditional_wrap(
                jam_condition(jam),
                format!("<style type=\"text/css\"{}>{}</style>", media, content),
            ),
            Vec::new(),
        ));
    }

    let refs: Vec<&str> = group.iter().map(String::as_str).collect();
    let path = manifest.jam_url_with_limit(&refs, "css", &opts.base_path, false, None)?;
    Ok((
        conditional_wrap(
            jam_condition(jam),
            format!(
                "<link rel=\"stylesheet\" href=\"{}\"{}/>",
                escape_attr(&join_url(&opts.cdn_base, &path)),
                media
            ),
        ),
        vec![path],
    ))
}

/// The module's current compiled file as a URL path — the versioned file
/// the manifest records — after checking the recorded bytes are still
/// there to serve. Per-module tags name real files directly: a page
/// emitted for a static host has no combo handler to turn corrupt or
/// missing bytes into a 404 at request time, so the render fails here
/// instead of writing a URL that can only serve wrong content. The
/// emitted path percent-encodes each segment — a legal output pointer
/// can carry quote, space, `#` or `%` — while the manifest record and
/// on-disk lookup stay raw.
fn per_module_output(
    manifest: &Manifest,
    dirs: &OutputDirs,
    asset_type: AssetType,
    module: &str,
) -> Result<String> {
    let entry = manifest
        .asset_key_for_module(module, asset_type.ext())
        .and_then(|key| manifest.assets.get(&key))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "module {} has no {} asset in the manifest",
                module,
                asset_type.ext()
            )
        })?;
    let readable = dirs
        .compiled_file(&entry.output, asset_type)
        .and_then(|path| verified_read(&path, &entry.hash))
        .is_some();
    if !readable {
        bail!(
            "module {} has no readable, verified compiled {} file for its current version",
            module,
            asset_type.ext()
        );
    }
    Ok(format!(
        "/{}",
        entry
            .output
            .trim_start_matches('/')
            .split('/')
            .map(url_encode_module)
            .collect::<Vec<_>>()
            .join("/")
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
    in_conditional_comment: bool,
) -> Option<String> {
    let ext = asset_type.ext();
    let separator = if asset_type == AssetType::Js {
        graph::JS_CONCAT_SEPARATOR
    } else {
        "\n"
    };

    let paths: Vec<(PathBuf, &str)> = group
        .iter()
        .map(|m| {
            manifest
                .asset_key_for_module(m, ext)
                .and_then(|key| manifest.assets.get(&key))
                .and_then(|e| {
                    dirs.compiled_file(&e.output, asset_type)
                        .map(|p| (p, e.hash.as_str()))
                })
        })
        .collect::<Option<_>>()?;

    if !forced {
        if inline_threshold == 0 {
            return None;
        }
        let total = paths.iter().try_fold(0u64, |sum, (p, _)| {
            fs::metadata(p).ok().map(|m| sum + m.len())
        });
        let separator_bytes = separator.len() as u64 * paths.len().saturating_sub(1) as u64;
        match total {
            Some(sum) if sum + separator_bytes <= inline_threshold as u64 => {}
            _ => return None,
        }
    }

    let mut contents = Vec::with_capacity(paths.len());
    for (path, hash) in &paths {
        contents.push(verified_read(path, hash)?);
    }
    let content = contents.join(separator);

    if !forced && content.len() > inline_threshold {
        return None;
    }
    if unsafe_for_inline(&content, asset_type, in_conditional_comment) {
        return None;
    }
    Some(content)
}

/// Content that could break out of an inline tag. Checked case-insensitively
/// on both types — a `</SCRIPT` written in capitals escapes the tag the same
/// as a lowercase one. Inside a conditional group's `<!--[if …]>` wrapper,
/// `-->` and `--!>` end the comment early (`while(a-->0)`, a CSS CDC token),
/// so such content is linked instead; outside one they are ordinary text.
fn unsafe_for_inline(content: &str, asset_type: AssetType, in_conditional_comment: bool) -> bool {
    let lower = content.to_lowercase();
    let closes_comment =
        in_conditional_comment && (lower.contains("-->") || lower.contains("--!>"));
    closes_comment
        || match asset_type {
            AssetType::Js => lower.contains("</script") || lower.contains("<!--"),
            AssetType::Css => lower.contains("</style"),
        }
}

/// Read a resolved compiled file, answering `None` when the bytes no
/// longer match the hash the manifest recorded for that version — a
/// year-cached URL whose file was corrupted or replaced after the build
/// must be a 404, not a permanent wrong answer. An entry without a real
/// recorded hash has nothing to verify against and answers nothing —
/// serving bytes under an unverifiable record would be indistinguishable
/// from serving the wrong bytes.
///
/// The read itself is guarded the same way: only an opened handle that
/// verifies as a regular file is read — never a FIFO planted to stall
/// the open — and the bytes are bounded by the largest plausible compiled
/// asset before the recorded hash is asked whether they are the right
/// ones. Verification answers against compile-time metadata only; it
/// never regenerates content.
fn verified_read(path: &Path, hash: &str) -> Option<String> {
    verified_bytes(path, hash).and_then(|bytes| String::from_utf8(bytes).ok())
}

/// The bytes at `path` when they are a bounded regular file whose MD5 is
/// `hash` (any hex case) — the one verified read the runtime and the
/// build's retention pass share.
pub(crate) fn verified_bytes(path: &Path, hash: &str) -> Option<Vec<u8>> {
    use crate::storage::MAX_COMPILED_ASSET_BYTES;
    if !is_content_hash(hash) {
        return None;
    }
    // Read paths may run through legitimate directory aliases — a deploy
    // that links `dist/css` to `css-real` still serves the same bytes.
    // The parent resolves before pinning; the regular-file check and the
    // recorded hash still decide what is served. Writes keep the strict
    // no-follow rule — a link there can redirect ownership.
    let parent = path.parent()?;
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let dir = crate::dirfd::OwnedDir::open(&parent).ok()?;
    // A trailing link is followed deliberately — a deployment may
    // legitimately assemble its output tree through links, and the hash
    // check below still decides whether the bytes served are the
    // recorded ones. The open itself is nonblocking, so even a
    // link-to-FIFO returns instead of waiting on a writer.
    let file = dir.open_file_follow(path.file_name()?).ok()?;
    if !crate::dirfd::OwnedDir::opened_file_is_regular(&file).ok()? {
        return None;
    }
    let mut limited = std::io::Read::take(&file, MAX_COMPILED_ASSET_BYTES + 1);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut limited, &mut bytes).ok()?;
    if bytes.len() as u64 > MAX_COMPILED_ASSET_BYTES {
        return None;
    }
    if !format!("{:x}", md5::compute(&bytes)).eq_ignore_ascii_case(hash) {
        return None;
    }
    Some(bytes)
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
    /// The group's Last-Modified as an IMF-fixdate, derived from the
    /// resolved versions' recorded timestamps — a host writes it as the
    /// Last-Modified header on a 200, and it names the value a 304's
    /// validator matched. `None` when no recorded stamp can answer for
    /// the group (a manifest written before stamping).
    pub last_modified: Option<String>,
    pub body: String,
}

fn not_found(content_type: &'static str) -> ComboResponse {
    ComboResponse {
        status: 404,
        content_type,
        cache_control: "no-cache",
        cacheable: false,
        location: None,
        last_modified: None,
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
        last_modified: None,
        body: String::new(),
    }
}

/// 304, empty body — the validator already holds these bytes. The group's
/// stamp, the cache policy, and the representation's own content type
/// travel with it: a cache may update stored metadata from the 304's
/// headers, so it must answer exactly what the 200 would have carried.
fn not_modified(last_modified: String, cacheable: bool, asset_type: AssetType) -> ComboResponse {
    ComboResponse {
        status: 304,
        content_type: content_type(asset_type),
        cache_control: if cacheable {
            "public, max-age=31536000"
        } else {
            "no-cache"
        },
        cacheable,
        location: None,
        last_modified: Some(last_modified),
        body: String::new(),
    }
}

/// An HTTP `Last-Modified`/`If-Modified-Since` value for `epoch` seconds —
/// IMF-fixdate, the only form a response may send.
fn http_date(epoch: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0)
        .map(|t| t.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
}

/// The epoch seconds a validator header names, or `None` when the header
/// is absent or not a recognizable HTTP date — an unparsable validator is
/// no validator, and the response answers 200. All three HTTP-date forms
/// are accepted — IMF-fixdate, RFC 850, and ANSI C asctime — as RFC 9110
/// §5.6.7 requires of recipients; responses still emit IMF-fixdate only.
fn validator_epoch(header: Option<&str>) -> Option<i64> {
    let header = header?.trim();
    if header.is_empty() {
        return None;
    }
    httpdate::parse_http_date(header).ok().and_then(|t| {
        t.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64)
    })
}

/// The conditional half of a combo response: when the request carried an
/// `If-Modified-Since` naming a moment at or after the group's recorded
/// stamp, the bytes it already holds are still these bytes — a 304.
fn fresh_enough(
    group_stamp: Option<i64>,
    if_modified_since: Option<&str>,
    cacheable: bool,
    asset_type: AssetType,
) -> Option<ComboResponse> {
    let stamp = group_stamp?;
    let seen = validator_epoch(if_modified_since)?;
    (seen >= stamp)
        .then(|| http_date(stamp).map(|lm| not_modified(lm, cacheable, asset_type)))
        .flatten()
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
/// seconds and the list is fully pinned — the listed order when every
/// entry carries a pin, dependency order when any entry is unpinned. A URL
/// that differs from the canonical one — missing or malformed pins, a
/// stale or non-numeric segment — is a 302 to it; a segment newer than
/// this manifest's stamp is a 404, a URL from a build this server does
/// not have.
pub fn combo(
    manifest: &Manifest,
    dirs: &OutputDirs,
    base_path: &str,
    path: &str,
    query: Option<&str>,
) -> ComboResponse {
    combo_conditional(manifest, dirs, base_path, path, query, None)
}

/// `combo` with the request's `If-Modified-Since` header. The group
/// timestamp is the maximum of the resolved versions' recorded
/// timestamps — the same manifest-derived stamp canonicalization already
/// computes — never a source file's mtime, a rebuild, or a freshness
/// scan. Canonicalization redirects and unavailable-version 404s still
/// answer first; a validator only ever shortens a 200 to a 304, and only
/// on an immutable representation — the fully pinned canonical URL or a
/// recorded-fingerprint loader response. A mutable unversioned or
/// `require` response always resolves and answers 200: a stamp that held
/// cannot prove the current manifest still names those bytes.
pub fn combo_conditional(
    manifest: &Manifest,
    dirs: &OutputDirs,
    base_path: &str,
    path: &str,
    query: Option<&str>,
    if_modified_since: Option<&str>,
) -> ComboResponse {
    // A query is echoed verbatim into a redirect's Location — a response
    // header. Anything outside ASCII graphic characters (raw CR/LF,
    // other controls, DEL, raw space, unencoded non-ASCII) must never
    // reach it, so the request fails before any parsing or redirect
    // construction. Percent-escaped bytes such as %0D stay escaped —
    // they are valid ASCII on the wire and are never decoded here.
    if let Some(query) = query {
        if !query.chars().all(|c| c.is_ascii_graphic()) {
            return not_found("text/plain; charset=utf-8");
        }
    }
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

    // The suffix is the URL's, never the module's: exactly one trailing
    // `.min.{ext}`, else `.{ext}`, stripped without consulting the
    // manifest. `.min` is never part of a module name — the build derives
    // `vendor` from `vendor.min.js` and rejects a declared `*.min` — so
    // `vendor.min.js` always names `vendor`.
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
    let raw_list = list_str;
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
    let mut duplicated = false;
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
                duplicated = true;
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

    // Immutable caching is for the versioned form only — a `require`
    // expansion can deliver different content under the same URL.
    let cacheable = versioned && !require;

    // The runtime loader's own URL — `{packagesUrl}{module}.js` — is the
    // one unpinned list a year-cached URL may serve. The segment names a
    // packages fingerprint. A recorded fingerprint answers from its
    // snapshot pins in the order the page's URL carries them — consulted
    // first because an all-digit fingerprint is rare but possible and a
    // saved page holding one must still resolve its build, and because
    // the snapshot never consults the current dependency graph. This
    // manifest's own current fingerprint — the new form or the legacy
    // form a first upgrade still serves — resolves unpinned to the
    // current set below. A fingerprint-shaped segment this manifest never
    // recorded is a 404: substituting current content would hand the page
    // a different build than the one it was rendered against. Every other
    // segment — a numeric stamp or a free-form token — canonicalization
    // decides like the rest of the versioned form.
    let loader =
        if cacheable && asset_type == AssetType::Js && pins.values().all(|pin| pin.is_none()) {
            match segment {
                Some(seg) => {
                    if let Some(snapshot) = manifest.packages_segments.get(seg) {
                        // A snapshot recorded before `.min` left module
                        // names can hold `{name}.min`; a page rendered then
                        // asked for it as `{name}.min.js`. That URL means the
                        // recorded `{name}.min` entry — its pinned key and
                        // hash, whose bytes retention kept — never the plain
                        // twin. Only such a snapshot reads it this way.
                        let mut listed = listed;
                        if suffix.starts_with(".min.") {
                            if let Some(last) = listed.last_mut() {
                                let old = format!("{last}.min");
                                if snapshot.contains_key(&old) {
                                    *last = old;
                                }
                            }
                        }
                        return historical_loader(
                            manifest,
                            dirs,
                            asset_type,
                            &listed,
                            &filter,
                            snapshot,
                            if_modified_since,
                        );
                    }
                    if seg == packages_segment(manifest) || seg == legacy_packages_segment(manifest)
                    {
                        true
                    } else if seg.len() == 12 && seg.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return not_found(content_type(asset_type));
                    } else {
                        false
                    }
                }
                None => false,
            }
        } else {
            false
        };

    // The versioned form delegates before any expansion: `versioned_combo`
    // resolves and orders on its own, so a cacheable request computed here
    // would only discard the work.
    if cacheable && !loader {
        return versioned_combo(
            manifest,
            dirs,
            base_path,
            asset_type,
            VersionedRequest {
                segment: segment.unwrap_or_default(),
                // The URL as spelled must be the one canonical spelling: no
                // duplicates, `~` separators, and names percent-encoded
                // exactly as `url_encode_module` writes them.
                spelled_canonical: !duplicated && raw_list == canonical_spelling(&listed_raw),
                listed_raw: &listed_raw,
                listed: &listed,
                pins: &pins,
                filter: &filter,
                query,
                suffix,
                if_modified_since,
            },
        );
    }

    // Expand first so listed modules come out in dependency order; without
    // `require` the expansion only orders — it never adds modules. `filter`
    // drops names after that ordering.
    let expanded = expand_dedup(manifest, &listed, asset_type.ext());
    let ordered: Vec<String> = if require {
        expanded
    } else {
        expanded
            .into_iter()
            .filter(|m| pins.contains_key(m))
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

    // Every member of the final group must resolve to a version the
    // manifest knows — supplied hashes were already checked against every
    // listed module above pin parsing, so a filtered-out wrong pin never
    // reaches this point. A partial group or a pruned history is never
    // served as a cacheable 200.
    let mut resolved: Vec<ResolvedVersion<'_>> = Vec::with_capacity(group.len());
    for m in &group {
        let pin = pins.get(m).and_then(|p| p.as_ref());
        let Some(entry) = manifest.resolve_version(m, asset_type.ext(), pin.map(|p| p.version))
        else {
            return not_found(content_type(asset_type));
        };
        resolved.push(entry);
    }

    // The stamp still travels as Last-Modified, but this path is a
    // mutable representation — an unversioned list or a `require`
    // expansion re-resolves at the current manifest, and a recorded
    // timestamp staying constant does not prove the bytes did not change
    // (a reproducible stamp, an imported source the manifest does not
    // stamp). Only the immutable forms — the fully pinned canonical URL
    // and the recorded-fingerprint loader — may shorten a 200 to a 304.
    let group_stamp = resolved
        .iter()
        .map(|e| stamp_epoch(e.timestamp))
        .try_fold(0i64, |m, s| s.map(|s| m.max(s)));

    let mut parts = Vec::new();
    for entry in &resolved {
        let content = dirs
            .compiled_file(entry.output, asset_type)
            .and_then(|p| verified_read(&p, entry.hash));
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
        last_modified: group_stamp.and_then(http_date),
        body: parts.join(separator),
    }
}

/// A recorded packages fingerprint's loader response: each listed module
/// at the version that build pinned, in the order the page's URL carries
/// them. The record maps the name as the page's URL spelled it to the
/// asset key, version, and content hash it resolved to — the module→key
/// mapping may have retired or been renamed since, and only the record
/// knows which asset the page's name meant. Every listed name validates
/// against the record first: a name outside the snapshot, a non-js or
/// unverifiable pin, a hash that does not match the recorded version, or a
/// version retention has pruned is a 404 — `filter` cannot launder a name
/// the snapshot never knew out of the check. The pin's hash must equal the
/// resolved version's recorded hash before a byte is read — a same-key,
/// same-version rebuild under different content is a different build and
/// can never stand in — then only the unfiltered members' files are read,
/// each verified against its recorded hash. A 404 is never a substitution
/// and never a partial group.
fn historical_loader(
    manifest: &Manifest,
    dirs: &OutputDirs,
    asset_type: AssetType,
    listed: &[String],
    filter: &HashSet<String>,
    pins: &BTreeMap<String, crate::manifest::SegmentPin>,
    if_modified_since: Option<&str>,
) -> ComboResponse {
    let mut resolved: Vec<(&String, ResolvedVersion<'_>)> = Vec::with_capacity(listed.len());
    for name in listed {
        let entry = pins
            .get(name)
            .filter(|pin| pin.key.starts_with("js/") && is_content_hash(&pin.hash))
            .and_then(|pin| {
                manifest
                    .resolve_key_version(&pin.key, pin.version)
                    .filter(|entry| entry.hash.eq_ignore_ascii_case(&pin.hash))
            });
        let Some(entry) = entry else {
            return not_found(content_type(asset_type));
        };
        resolved.push((name, entry));
    }

    let group_stamp = resolved
        .iter()
        .map(|(_, e)| stamp_epoch(e.timestamp))
        .try_fold(0i64, |m, s| s.map(|s| m.max(s)));

    // Eligibility first: the snapshot names immutable bytes, so a 304 is
    // legal — but only where the ordinary response would be a 200. The
    // verified reads and the nonempty check run before the validator is
    // asked anything, so a deleted or replaced file answers 404 like the
    // unconditional request.
    let mut parts = Vec::new();
    for (_, entry) in resolved.iter().filter(|(name, _)| !filter.contains(*name)) {
        match dirs
            .compiled_file(entry.output, asset_type)
            .and_then(|p| verified_read(&p, entry.hash))
        {
            Some(c) => parts.push(c),
            None => return not_found(content_type(asset_type)),
        }
    }
    if parts.is_empty() {
        return not_found(content_type(asset_type));
    }
    if let Some(response) = fresh_enough(group_stamp, if_modified_since, true, asset_type) {
        return response;
    }
    ComboResponse {
        status: 200,
        content_type: content_type(asset_type),
        cache_control: "public, max-age=31536000",
        cacheable: true,
        location: None,
        last_modified: group_stamp.and_then(http_date),
        body: parts.join(graph::JS_CONCAT_SEPARATOR),
    }
}

/// A parsed versioned request — the shape the URL's path segments described,
/// before canonicalization decides whether it already is the canonical URL.
struct VersionedRequest<'a> {
    /// The `{stamp}` path segment as written.
    segment: &'a str,
    /// Whether the list was written in its one canonical spelling.
    spelled_canonical: bool,
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
    /// The request's `If-Modified-Since` header, when it carried one.
    if_modified_since: Option<&'a str>,
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
/// canonical list is the listed order whenever every listed entry carries
/// an explicit version pin: a fully pinned URL names an exact byte
/// sequence, so two orders of the same members are two different
/// addresses and neither is rewritten to the other's. Dependency order
/// applies only when at least one entry is unpinned — the request's order
/// means nothing then, so the canonical form is the manifest's layering.
/// The canonical stamp is the newest resolved version's. A request that is
/// not already that URL — missing or malformed pins, a stale or
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
        spelled_canonical,
        listed_raw,
        listed,
        pins,
        filter,
        query,
        suffix,
        if_modified_since,
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

    // A fully pinned list names one exact byte sequence: the canonical
    // order is the order the URL carries, because a later build's
    // dependency graph can reorder the same versions and must not move
    // the bytes a saved URL already pointed at. Only a request holding an
    // unpinned entry — a bare name, which resolves at the current version
    // by definition — canonicalizes into the manifest's layering.
    let fully_pinned = listed.iter().all(|m| pins[m].is_some());
    let canonical: Vec<String> = if fully_pinned {
        listed.to_vec()
    } else {
        let position = sorted_positions(manifest, ext);
        let mut names = listed.to_vec();
        names.sort_by_key(|m| position.get(m.as_str()).copied().unwrap_or(usize::MAX));
        names
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
            if listed_raw != canonical_entries
                || !spelled_canonical
                || stamp < canonical_stamp
                || (stamp == canonical_stamp && segment != canonical_stamp.to_string())
            {
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

    // The canonical URL names immutable bytes, so a 304 is legal — but
    // only where the ordinary response would be a 200. The verified reads
    // and the nonempty check run before the validator is asked anything:
    // a filtered-out group is a 404, and a file gone since the build —
    // even one year-caching a saved page relies on — is a 404, never a
    // cache-confirmed 304 for bytes that are not there.
    let mut parts = Vec::new();
    for m in canonical.iter().filter(|m| !filter.contains(*m)) {
        let entry = &resolved[m.as_str()];
        let content = dirs
            .compiled_file(entry.output, asset_type)
            .and_then(|p| verified_read(&p, entry.hash));
        match content {
            Some(c) => parts.push(c),
            None => return not_found(content_type(asset_type)),
        }
    }

    if parts.is_empty() {
        return not_found(content_type(asset_type));
    }
    if let Some(response) = fresh_enough(Some(canonical_stamp), if_modified_since, true, asset_type)
    {
        return response;
    }
    ComboResponse {
        status: 200,
        content_type: content_type(asset_type),
        cache_control: "public, max-age=31536000",
        cacheable: true,
        location: None,
        last_modified: http_date(canonical_stamp),
        body: parts.join(separator),
    }
}

/// Position in the manifest's `sorted_deps` layering for `asset_type` — the
/// order an unpinned or partially pinned list canonicalizes to.
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
/// Decoded list entries re-spelled the way a canonical URL writes them:
/// each name through `url_encode_module`, its pin suffix literal, `~`-joined.
fn canonical_spelling(entries: &[String]) -> String {
    entries
        .iter()
        .map(|entry| match parse_list_entry(entry) {
            Some((name, _)) => format!("{}{}", url_encode_module(name), &entry[name.len()..]),
            None => entry.clone(),
        })
        .collect::<Vec<_>>()
        .join("~")
}

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

    /// Write a compiled file and re-stamp the manifest record for that
    /// output to the content's real hash — fixtures must describe the
    /// bytes honestly now that serving verifies them. A file the manifest
    /// does not record is just written.
    fn write_out(manifest: &mut Manifest, base: &Path, output: &str, content: &str) {
        let path = base.join(output.trim_start_matches('/'));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        let hash = format!("{:x}", md5::compute(content));
        for entry in manifest.assets.values_mut() {
            if entry.output == output {
                entry.hash = hash.clone();
            }
            for history in &mut entry.history {
                if history.output == output {
                    history.hash = hash.clone();
                }
            }
        }
    }

    fn opts() -> RenderOptions {
        RenderOptions {
            cdn_base: String::new(),
            base_path: "/paste".to_string(),
            inline_threshold: 1024,
            per_module: false,
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

    /// Manifest with the js runtime plus one js and one css module — enough
    /// for finish tests that need a module of each type pending at once.
    fn mixed_manifest() -> Manifest {
        let mut manifest = js_manifest(&[("app", &[])]);
        manifest
            .modules
            .insert("app".to_string(), "js/app.js".to_string());
        manifest
            .modules
            .insert("site".to_string(), "css/site.css".to_string());
        manifest
            .modules
            .insert("paste".to_string(), "js/paste.js".to_string());
        manifest.assets.insert(
            "css/site.css".to_string(),
            asset("/css/site.h.v1.min.css", 1, STAMP, &[]),
        );
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
            Event::Dump(DumpScope::Css),
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
        let mut manifest = js_manifest(&[("a", &[]), ("b", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");
        write_out(&mut manifest, dir.path(), "/js/b.h.v3.min.js", "B");

        let events = vec![
            jam(&[], &["a"]),
            Event::Dump(DumpScope::Js),
            jam(&[], &["b"]),
            Event::Dump(DumpScope::Js),
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
        let mut manifest = js_manifest(&[("a", &[]), ("b", &["a"])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        // a and b files missing → the group goes out over a URL.

        let events = vec![jam(&[], &["b"]), Event::Dump(DumpScope::Js)];
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
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        // a and b files missing → the group goes out over a URL.

        let events = vec![jam(&[], &["b", "a"]), Event::Dump(DumpScope::Js)];
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
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );

        let events = vec![jam(&[], &["b", "cyc"]), Event::Dump(DumpScope::Js)];
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
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );

        let events = vec![
            Event::Jam(Jam {
                js: vec!["a".to_string()],
                sync: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Js),
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
        let mut manifest = js_manifest(&[("a", &[]), ("b", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(
            &mut manifest,
            dir.path(),
            "/js/a.h.v2.min.js",
            &"x".repeat(510),
        );
        write_out(
            &mut manifest,
            dir.path(),
            "/js/b.h.v3.min.js",
            &format!("{}x", "é".repeat(255)),
        );

        let events = vec![jam(&[], &["a", "b"]), Event::Dump(DumpScope::Js)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(
            rendered[0].html.contains(&"é".repeat(255)),
            "1024 bytes exactly must inline: {}",
            rendered[0].html
        );
        assert!(rendered[0].urls.is_empty(), "{:?}", rendered[0].urls);

        // One byte over → external.
        write_out(
            &mut manifest,
            dir.path(),
            "/js/b.h.v3.min.js",
            &format!("{}xx", "é".repeat(255)),
        );
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(!rendered[0].html.contains('é'), "{}", rendered[0].html);
        assert_eq!(rendered[0].urls.len(), 1, "{:?}", rendered[0].urls);
    }

    #[test]
    fn test_render_forced_inline_ignores_threshold_and_zero_disables_auto() {
        let mut manifest = js_manifest(&[("big", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(
            &mut manifest,
            dir.path(),
            "/js/big.h.v2.min.js",
            &"x".repeat(5000),
        );

        // Forced inline ignores the size check entirely.
        let events = vec![
            Event::Jam(Jam {
                js: vec!["big".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Js),
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
        let events = vec![jam(&[], &["big"]), Event::Dump(DumpScope::Js)];
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
            Event::Dump(DumpScope::Js),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &options, &events).unwrap();
        assert!(rendered[0].html.contains(&"x".repeat(5000)));
    }

    // ============================================================
    // Per-module rendering
    // ============================================================

    fn per_module_opts() -> RenderOptions {
        // Threshold 0 keeps every module external so the per-module URLs
        // show; inline cases set their own value.
        RenderOptions {
            per_module: true,
            inline_threshold: 0,
            ..opts()
        }
    }

    fn write_all_js(manifest: &mut Manifest, base: &Path, outputs: &[&str]) {
        write_out(manifest, base, "/js/paste.h.v1.min.js", "PASTE_RT");
        for output in outputs {
            write_out(manifest, base, output, "x");
        }
    }

    #[test]
    fn test_per_module_and_combo_agree_on_module_set_and_order() {
        // Same events in both modes: the per-module tags name the same
        // modules in the same dependency order the combo URL lists.
        let mut manifest = js_manifest(&[("a", &[]), ("b", &["a"]), ("c", &["b"])]);
        sort_deps(&mut manifest, "js", &["a", "b", "c"]);
        manifest.assets.insert(
            "css/site.css".to_string(),
            asset("/css/site.h.v1.min.css", 1, STAMP, &[]),
        );
        manifest.assets.insert(
            "css/theme.css".to_string(),
            asset("/css/theme.h.v2.min.css", 2, STAMP, &["site"]),
        );
        let dir = tempfile::tempdir().unwrap();
        write_all_js(
            &mut manifest,
            dir.path(),
            &[
                "/js/a.h.v2.min.js",
                "/js/b.h.v3.min.js",
                "/js/c.h.v4.min.js",
            ],
        );
        write_out(&mut manifest, dir.path(), "/css/site.h.v1.min.css", "s");
        write_out(&mut manifest, dir.path(), "/css/theme.h.v2.min.css", "t");

        let events = vec![
            jam(&["theme"], &["c"]),
            Event::Dump(DumpScope::Css),
            Event::Dump(DumpScope::Js),
        ];
        // Files exist, so both modes need the threshold off for URLs to
        // show rather than inline tags.
        let external = RenderOptions {
            inline_threshold: 0,
            ..opts()
        };
        let combo = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &external,
            &events,
        )
        .unwrap();
        let per_module = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &per_module_opts(),
            &events,
        )
        .unwrap();

        // css dump: theme pulls site in first, both modes.
        assert_eq!(combo[0].urls.len(), 1, "{:?}", combo[0].urls);
        assert!(
            combo[0].urls[0].contains("site+v1.0~theme+v2.0"),
            "{}",
            combo[0].urls[0]
        );
        assert_eq!(
            per_module[0].urls,
            vec![
                "/css/site.h.v1.min.css".to_string(),
                "/css/theme.h.v2.min.css".to_string()
            ],
            "{:?}",
            per_module[0].urls
        );
        assert_eq!(per_module[0].html.matches("<link").count(), 2);

        // js dump: c expands to a,b,c — same order the combo token names.
        assert!(
            combo[1].urls[0].contains("a+v2.0~b+v3.0~c+v4.0.min.js"),
            "{}",
            combo[1].urls[0]
        );
        assert_eq!(
            per_module[1].urls,
            vec![
                "/js/a.h.v2.min.js".to_string(),
                "/js/b.h.v3.min.js".to_string(),
                "/js/c.h.v4.min.js".to_string()
            ],
            "{:?}",
            per_module[1].urls
        );
        // One ordered loader block carries the whole group: the marks land
        // before the single request chain, which names the files in the
        // same dependency order.
        assert_eq!(per_module[1].html.matches("paste.require").count(), 1);
        assert!(
            per_module[1]
                .html
                .contains("d('a').isLoading=d('b').isLoading=d('c').isLoading=!0;n();"),
            "{}",
            per_module[1].html
        );
        assert!(
            per_module[1]
                .html
                .contains("var u=['/js/a.h.v2.min.js','/js/b.h.v3.min.js','/js/c.h.v4.min.js']"),
            "{}",
            per_module[1].html
        );
    }

    #[test]
    fn test_per_module_sync_emits_script_src() {
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_all_js(&mut manifest, dir.path(), &["/js/a.h.v2.min.js"]);

        let events = vec![
            Event::Jam(Jam {
                js: vec!["a".to_string()],
                sync: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Js),
        ];
        let rendered = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &per_module_opts(),
            &events,
        )
        .unwrap();
        assert!(
            rendered[0]
                .html
                .contains("<script type=\"text/javascript\" src=\"/js/a.h.v2.min.js\"></script>"),
            "{}",
            rendered[0].html
        );
        assert!(
            !rendered[0].html.contains("paste.require"),
            "{}",
            rendered[0].html
        );
    }

    #[test]
    fn test_per_module_media_reaches_every_link() {
        let mut manifest = Manifest::new();
        manifest.hash_urls = false;
        for (key, output) in [
            ("css/a.css", "/css/a.h.v1.min.css"),
            ("css/b.css", "/css/b.h.v2.min.css"),
        ] {
            manifest
                .assets
                .insert(key.to_string(), asset(output, 1, STAMP, &[]));
        }
        let dir = tempfile::tempdir().unwrap();
        write_out(&mut manifest, dir.path(), "/css/a.h.v1.min.css", "a");
        write_out(&mut manifest, dir.path(), "/css/b.h.v2.min.css", "b");

        let events = vec![
            Event::Jam(Jam {
                css: vec!["a".to_string(), "b".to_string()],
                media: Some("print".to_string()),
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Css),
        ];
        let rendered = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &per_module_opts(),
            &events,
        )
        .unwrap();
        assert_eq!(
            rendered[0].html.matches("media=\"print\"").count(),
            2,
            "{}",
            rendered[0].html
        );
        assert!(
            rendered[0].html.contains(
                "<link rel=\"stylesheet\" href=\"/css/a.h.v1.min.css\" media=\"print\"/>"
            ),
            "{}",
            rendered[0].html
        );
    }

    #[test]
    fn test_per_module_inlines_small_module_and_links_large_one() {
        // The threshold decides per module: a (1 byte) inlines while b
        // (over 1024) keeps its URL — the split lands each on its own side.
        let mut manifest = js_manifest(&[("a", &[]), ("b", &[])]);
        sort_deps(&mut manifest, "js", &["a", "b"]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");
        write_out(
            &mut manifest,
            dir.path(),
            "/js/b.h.v3.min.js",
            &"x".repeat(2000),
        );

        let events = vec![jam(&[], &["a", "b"]), Event::Dump(DumpScope::Js)];
        let options = RenderOptions {
            per_module: true,
            ..opts()
        };
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &options, &events).unwrap();
        assert!(
            rendered[0]
                .html
                .contains("<script type=\"text/javascript\">A</script>"),
            "{}",
            rendered[0].html
        );
        assert!(
            rendered[0].html.contains("r('/js/b.h.v3.min.js')"),
            "{}",
            rendered[0].html
        );
        assert_eq!(rendered[0].urls, vec!["/js/b.h.v3.min.js".to_string()]);
    }

    #[test]
    fn test_per_module_async_group_stays_external_after_first_url() {
        // The group is one ordered request chain: once any file is
        // external, every later file stays external so nothing can run
        // before its dependency — even files small enough to inline.
        let mut manifest = js_manifest(&[("a", &[]), ("b", &["a"]), ("c", &["b"])]);
        sort_deps(&mut manifest, "js", &["a", "b", "c"]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(
            &mut manifest,
            dir.path(),
            "/js/a.h.v2.min.js",
            &"a".repeat(2000),
        );
        write_out(&mut manifest, dir.path(), "/js/b.h.v3.min.js", "B");
        write_out(&mut manifest, dir.path(), "/js/c.h.v4.min.js", "C");
        let dirs = OutputDirs::under(dir.path());
        let options = RenderOptions {
            per_module: true,
            ..opts()
        };

        // Async: a crosses the threshold, so b and c go out as URLs too —
        // one loader block names all three in dependency order and marks
        // them before the first request.
        let events = vec![jam(&[], &["c"]), Event::Dump(DumpScope::Js)];
        let rendered = render_all(&manifest, &dirs, &options, &events).unwrap();
        assert_eq!(
            rendered[0].urls,
            vec![
                "/js/a.h.v2.min.js".to_string(),
                "/js/b.h.v3.min.js".to_string(),
                "/js/c.h.v4.min.js".to_string()
            ],
            "{:?}",
            rendered[0].urls
        );
        let html = &rendered[0].html;
        assert_eq!(html.matches("paste.require").count(), 1, "{html}");
        assert!(!html.contains(">B<"), "{html}");
        assert!(!html.contains(">C<"), "{html}");
        assert!(
            html.contains("var u=['/js/a.h.v2.min.js','/js/b.h.v3.min.js','/js/c.h.v4.min.js']"),
            "{html}"
        );
        assert!(
            html.contains("d('a').isLoading=d('b').isLoading=d('c').isLoading=!0;n();"),
            "{html}"
        );

        // Sync: files are independent — only a crosses the threshold, so
        // b and c still inline in order behind a's src tag.
        let events = vec![
            Event::Jam(Jam {
                js: vec!["c".to_string()],
                sync: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Js),
        ];
        let rendered = render_all(&manifest, &dirs, &options, &events).unwrap();
        assert_eq!(
            rendered[0].urls,
            vec!["/js/a.h.v2.min.js".to_string()],
            "{:?}",
            rendered[0].urls
        );
        let html = &rendered[0].html;
        let a_tag = html.find("src=\"/js/a.h.v2.min.js\"").unwrap();
        let b_tag = html
            .find("<script type=\"text/javascript\">B</script>")
            .unwrap();
        let c_tag = html
            .find("<script type=\"text/javascript\">C</script>")
            .unwrap();
        assert!(a_tag < b_tag && b_tag < c_tag, "{html}");
    }

    #[test]
    fn test_per_module_async_forced_inline_keeps_external_group_order() {
        // `inline = true` cannot let a later file overtake an external
        // one: unsafe a must go out as a URL, and b/c follow it over URLs
        // too — inlining them first would run them before their dependency.
        let mut manifest = js_manifest(&[("a", &[]), ("b", &["a"]), ("c", &["b"])]);
        sort_deps(&mut manifest, "js", &["a", "b", "c"]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(
            &mut manifest,
            dir.path(),
            "/js/a.h.v2.min.js",
            "var a='</script>';",
        );
        write_out(&mut manifest, dir.path(), "/js/b.h.v3.min.js", "B");
        write_out(&mut manifest, dir.path(), "/js/c.h.v4.min.js", "C");

        let events = vec![
            Event::Jam(Jam {
                js: vec!["c".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Js),
        ];
        let rendered = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &per_module_opts(),
            &events,
        )
        .unwrap();
        assert_eq!(
            rendered[0].urls,
            vec![
                "/js/a.h.v2.min.js".to_string(),
                "/js/b.h.v3.min.js".to_string(),
                "/js/c.h.v4.min.js".to_string()
            ],
            "{:?}",
            rendered[0].urls
        );
        let html = &rendered[0].html;
        assert_eq!(html.matches("paste.require").count(), 1, "{html}");
        assert!(!html.contains("</script>';"), "{html}");
        assert!(!html.contains(">B<"), "{html}");
        assert!(!html.contains(">C<"), "{html}");
    }

    #[test]
    fn test_per_module_omits_modules_an_earlier_dump_delivered() {
        // a dumps on the first render; the second per-module dump emits
        // only b's tag — the loaded set is shared across both modes.
        let mut manifest = js_manifest(&[("a", &[]), ("b", &[])]);
        sort_deps(&mut manifest, "js", &["a", "b"]);
        let dir = tempfile::tempdir().unwrap();
        write_all_js(
            &mut manifest,
            dir.path(),
            &["/js/a.h.v2.min.js", "/js/b.h.v3.min.js"],
        );

        let events = vec![
            jam(&[], &["a"]),
            Event::Dump(DumpScope::Js),
            jam(&[], &["a", "b"]),
            Event::Dump(DumpScope::Js),
        ];
        let rendered = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &per_module_opts(),
            &events,
        )
        .unwrap();
        assert_eq!(rendered[0].urls, vec!["/js/a.h.v2.min.js".to_string()]);
        assert_eq!(rendered[1].urls, vec!["/js/b.h.v3.min.js".to_string()]);
        assert!(!rendered[1].html.contains("a.h.v2"), "{}", rendered[1].html);
    }

    #[test]
    fn test_per_module_event_override_both_ways() {
        // The jam's own perModule beats the setting: per-module renders a
        // Some(false) group as one combo tag; a combo render serves a
        // Some(true) group as per-module tags.
        let mut manifest = js_manifest(&[("a", &[]), ("b", &[])]);
        sort_deps(&mut manifest, "js", &["a", "b"]);
        let dir = tempfile::tempdir().unwrap();
        write_all_js(
            &mut manifest,
            dir.path(),
            &["/js/a.h.v2.min.js", "/js/b.h.v3.min.js"],
        );

        let off = Event::Jam(Jam {
            js: vec!["a".to_string(), "b".to_string()],
            per_module: Some(false),
            ..Jam::default()
        });
        let events = vec![off, Event::Dump(DumpScope::Js)];
        let rendered = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &per_module_opts(),
            &events,
        )
        .unwrap();
        assert_eq!(rendered[0].urls.len(), 1, "{:?}", rendered[0].urls);
        assert!(
            rendered[0].urls[0].starts_with("/paste/"),
            "{:?}",
            rendered[0].urls
        );

        let on = Event::Jam(Jam {
            js: vec!["a".to_string(), "b".to_string()],
            per_module: Some(true),
            ..Jam::default()
        });
        let events = vec![on, Event::Dump(DumpScope::Js)];
        let combo_opts = RenderOptions {
            inline_threshold: 0,
            ..opts()
        };
        let rendered = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &combo_opts,
            &events,
        )
        .unwrap();
        assert_eq!(
            rendered[0].urls,
            vec![
                "/js/a.h.v2.min.js".to_string(),
                "/js/b.h.v3.min.js".to_string()
            ],
            "{:?}",
            rendered[0].urls
        );
    }

    #[test]
    fn test_per_module_urls_are_output_paths() {
        // urls records the bare output path; the tag carries cdn_base +
        // path, and an empty prefix leaves the path origin-relative.
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_all_js(&mut manifest, dir.path(), &["/js/a.h.v2.min.js"]);

        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
        for (cdn_base, expected) in [
            (
                "https://cdn.example",
                "https://cdn.example/js/a.h.v2.min.js",
            ),
            ("", "/js/a.h.v2.min.js"),
        ] {
            let options = RenderOptions {
                cdn_base: cdn_base.to_string(),
                per_module: true,
                inline_threshold: 0,
                ..opts()
            };
            let rendered =
                render_all(&manifest, &OutputDirs::under(dir.path()), &options, &events).unwrap();
            assert_eq!(rendered[0].urls, vec!["/js/a.h.v2.min.js".to_string()]);
            assert!(
                rendered[0].html.contains(&format!("r('{expected}')")),
                "{cdn_base}: {}",
                rendered[0].html
            );
        }
    }

    #[test]
    fn test_per_module_bootstrap_is_unchanged() {
        // The runtime bootstrap leads the first js dump exactly as combo
        // mode — inline when it can, config on its own line, once.
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_all_js(&mut manifest, dir.path(), &["/js/a.h.v2.min.js"]);

        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
        let rendered = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &per_module_opts(),
            &events,
        )
        .unwrap();
        let html = &rendered[0].html;
        assert!(
            html.starts_with("<script type=\"text/javascript\">PASTE_RT"),
            "{html}"
        );
        assert_eq!(html.matches("paste.config").count(), 1, "{html}");
    }

    #[test]
    fn test_per_module_missing_compiled_file_fails_the_render() {
        // No combo handler stands behind a per-module URL — a module whose
        // current file is absent fails the render instead of emitting a
        // URL that can only 404 later.
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        // a's file is deliberately absent.

        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
        let err = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &per_module_opts(),
            &events,
        )
        .unwrap_err();
        assert!(err.to_string().contains("a"), "{err}");
        assert!(err.to_string().contains("js"), "{err}");
    }

    #[test]
    fn test_per_module_external_output_requires_verified_bytes() {
        // The emitted file URL is only as good as the bytes behind it: a
        // per-module render fails when the recorded file's content no
        // longer verifies — overwritten, recorded under a non-content
        // hash, or missing outright. The baseline must render first so the
        // failures come from integrity, not a bad fixture.
        let mut manifest = js_manifest(&[("a", &[])]);
        manifest.assets.insert(
            "css/site.css".to_string(),
            asset("/css/site.h.v1.min.css", 1, STAMP, &[]),
        );
        let dir = tempfile::tempdir().unwrap();
        write_all_js(&mut manifest, dir.path(), &["/js/a.h.v2.min.js"]);
        write_out(&mut manifest, dir.path(), "/css/site.h.v1.min.css", "s");

        let events = vec![jam(&["site"], &["a"]), Event::Dump(DumpScope::All)];
        let dirs = OutputDirs::under(dir.path());
        let rendered = render_all(&manifest, &dirs, &per_module_opts(), &events).unwrap();
        assert_eq!(
            rendered[0].urls,
            vec![
                "/css/site.h.v1.min.css".to_string(),
                "/js/a.h.v2.min.js".to_string()
            ],
            "{:?}",
            rendered[0].urls
        );

        // Tampered bytes fail the js module; the valid baseline proves the
        // error is integrity, not resolution.
        fs::write(dir.path().join("js/a.h.v2.min.js"), "tampered").unwrap();
        let err = render_all(&manifest, &dirs, &per_module_opts(), &events).unwrap_err();
        assert!(err.to_string().contains("readable, verified"), "{err}");
        assert!(err.to_string().contains('a'), "{err}");
        fs::write(dir.path().join("js/a.h.v2.min.js"), "x").unwrap();

        // An empty or non-hex recorded hash verifies against nothing — both
        // fail, on css as well as js.
        for key in ["js/a.js", "css/site.css"] {
            for bad in ["", "nothex"] {
                manifest.assets.get_mut(key).unwrap().hash = bad.to_string();
                let err = render_all(&manifest, &dirs, &per_module_opts(), &events).unwrap_err();
                assert!(
                    err.to_string().contains("readable, verified"),
                    "{key}: {err}"
                );
                manifest.assets.get_mut(key).unwrap().hash = format!(
                    "{:x}",
                    md5::compute(if key == "js/a.js" { "x" } else { "s" })
                );
            }
        }

        // A missing file fails the same way.
        fs::remove_file(dir.path().join("css/site.h.v1.min.css")).unwrap();
        let err = render_all(&manifest, &dirs, &per_module_opts(), &events).unwrap_err();
        assert!(err.to_string().contains("readable, verified"), "{err}");
    }

    #[test]
    fn test_per_module_output_paths_are_percent_encoded_per_segment() {
        // A legal output pointer can carry characters a raw URL cannot —
        // quote, space, #, %, &, non-ASCII. Each segment is percent-encoded
        // in the emitted tag and `urls`, while the manifest pointer and the
        // on-disk file stay raw.
        let pointer_js = "/assets/O'Reilly #50% & café/_paste_build/app.h.v2.min.js";
        let pointer_css = "/assets/O'Reilly #50% & café/_paste_build/site.h.v1.min.css";
        let mut manifest = js_manifest(&[("a", &[])]);
        manifest.assets.get_mut("js/a.js").unwrap().output = pointer_js.to_string();
        manifest.assets.insert(
            "css/site.css".to_string(),
            asset(pointer_css, 1, STAMP, &[]),
        );
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(&mut manifest, dir.path(), pointer_js, "A");
        write_out(&mut manifest, dir.path(), pointer_css, "s");

        let encoded_js =
            "/assets/O%27Reilly%20%2350%25%20%26%20caf%C3%A9/_paste_build/app.h.v2.min.js";
        let encoded_css =
            "/assets/O%27Reilly%20%2350%25%20%26%20caf%C3%A9/_paste_build/site.h.v1.min.css";

        // Async: one ordered loader block requests the group's files;
        // one link per stylesheet.
        let events = vec![jam(&["site"], &["a"]), Event::Dump(DumpScope::All)];
        let dirs = OutputDirs::under(dir.path());
        let rendered = render_all(&manifest, &dirs, &per_module_opts(), &events).unwrap();
        // The runtime inlines (safe content), so urls names only the two
        // external module files.
        assert_eq!(
            rendered[0].urls,
            vec![encoded_css.to_string(), encoded_js.to_string()],
            "{:?}",
            rendered[0].urls
        );
        let html = &rendered[0].html;
        assert!(html.contains(&format!("r('{encoded_js}')")), "{html}");
        assert!(html.contains(&format!("href=\"{encoded_css}\"")), "{html}");
        assert!(!html.contains("O'Reilly"), "{html}");
        assert!(!html.contains("café"), "{html}");

        // The recorded URL decodes back to the raw pointer, which names the
        // file that was written. The bootstrap inlines, so the only .min.js
        // URL is the module's.
        let js_url = rendered[0]
            .urls
            .iter()
            .find(|u| u.ends_with("min.js"))
            .unwrap();
        assert_eq!(percent_decode(js_url).as_deref(), Some(pointer_js));
        assert_eq!(
            fs::read_to_string(dir.path().join(pointer_js.trim_start_matches('/'))).unwrap(),
            "A"
        );

        // Sync: src/href carry the same encoded path.
        let events = vec![
            Event::Jam(Jam {
                css: vec!["site".to_string()],
                js: vec!["a".to_string()],
                sync: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::All),
        ];
        let rendered = render_all(&manifest, &dirs, &per_module_opts(), &events).unwrap();
        let html = &rendered[0].html;
        assert!(
            html.contains(&format!(
                "<script type=\"text/javascript\" src=\"{encoded_js}\"></script>"
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "<link rel=\"stylesheet\" href=\"{encoded_css}\"/>"
            )),
            "{html}"
        );
        assert!(!html.contains("O'Reilly"), "{html}");
    }

    #[test]
    fn test_per_module_output_normalizes_leading_slashes() {
        // A pointer recorded with a doubled leading slash is still a local
        // under-root file — the emitted URL must carry one `/` so it is
        // never a network-path (`//host`) reference.
        let mut manifest = js_manifest(&[("a", &[])]);
        manifest.assets.get_mut("js/a.js").unwrap().output = "//js/a.h.v2.min.js".to_string();
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(&mut manifest, dir.path(), "//js/a.h.v2.min.js", "A");

        let events = vec![
            Event::Jam(Jam {
                js: vec!["a".to_string()],
                sync: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Js),
        ];
        let rendered = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &per_module_opts(),
            &events,
        )
        .unwrap();
        assert_eq!(rendered[0].urls, vec!["/js/a.h.v2.min.js".to_string()]);
        assert!(
            rendered[0]
                .html
                .contains("<script type=\"text/javascript\" src=\"/js/a.h.v2.min.js\"></script>"),
            "{}",
            rendered[0].html
        );
        assert!(
            !rendered[0].html.contains("src=\"//"),
            "{}",
            rendered[0].html
        );
    }

    #[test]
    fn test_render_escapes_url_attributes_but_not_js_strings() {
        // `&` is legal in cdn_base and base_path, but a bare `&x;` inside
        // an attribute resolves as an HTML entity — src and href take the
        // escaped form while JS string URLs and Rendered.urls keep the
        // literal prefix. All six attribute sites are covered: bootstrap
        // fallback src (per-module files and combo), group sync src, and
        // stylesheet href in both modes.
        let cdn = "https://cdn.example/a&copy;";
        let base_path = "/assets&copy;";
        for (per_module, sync, unsafe_runtime) in [
            (true, false, false),
            (true, true, false),
            (false, false, false),
            (false, true, false),
            (true, false, true),
            (false, false, true),
        ] {
            let label = format!("per_module={per_module} sync={sync} unsafe_rt={unsafe_runtime}");
            let mut manifest = js_manifest(&[("a", &[])]);
            manifest.assets.insert(
                "css/site.css".to_string(),
                asset("/css/site.h.v1.min.css", 1, STAMP, &[]),
            );
            let dir = tempfile::tempdir().unwrap();
            write_out(
                &mut manifest,
                dir.path(),
                "/js/paste.h.v1.min.js",
                if unsafe_runtime {
                    "PASTE</script>x"
                } else {
                    "PASTE_RT"
                },
            );
            write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");
            write_out(&mut manifest, dir.path(), "/css/site.h.v1.min.css", "s");

            let events = vec![
                Event::Jam(Jam {
                    css: vec!["site".to_string()],
                    js: vec!["a".to_string()],
                    sync,
                    ..Jam::default()
                }),
                Event::Dump(DumpScope::All),
            ];
            let options = RenderOptions {
                cdn_base: cdn.to_string(),
                base_path: base_path.to_string(),
                inline_threshold: 0,
                per_module,
            };
            let rendered =
                render_all(&manifest, &OutputDirs::under(dir.path()), &options, &events).unwrap();
            let html = &rendered[0].html;

            // No raw & reaches an attribute anywhere in the dump.
            assert!(
                !html.contains("src=\"https://cdn.example/a&copy;"),
                "{label}: {html}"
            );
            assert!(
                !html.contains("href=\"https://cdn.example/a&copy;"),
                "{label}: {html}"
            );
            // The JS-string forms stay literal — the config call and any
            // async loader statements.
            assert!(
                html.contains("packagesUrl:'https://cdn.example/a&copy;/assets&copy;/"),
                "{label}: {html}"
            );
            if !sync {
                let js_prefix = if per_module { "/js/" } else { "/assets&copy;/" };
                assert!(
                    html.contains(&format!("r('https://cdn.example/a&copy;{js_prefix}")),
                    "{label}: {html}"
                );
            }
            // Every attribute site carries the escaped prefix.
            let escaped_file = "https://cdn.example/a&amp;copy;/js/";
            let escaped_combo = "https://cdn.example/a&amp;copy;/assets&amp;copy;/";
            match (per_module, sync, unsafe_runtime) {
                (true, true, _) => {
                    assert!(
                        html.contains(&format!("src=\"{escaped_file}a.h.v2.min.js\"")),
                        "{label}: {html}"
                    );
                }
                (true, _, true) => {
                    // Bootstrap per-module fallback src is escaped too.
                    assert!(
                        html.contains(&format!("src=\"{escaped_file}paste.h.v1.min.js\"")),
                        "{label}: {html}"
                    );
                    assert!(!html.contains("PASTE</script>x"), "{label}: {html}");
                }
                (false, true, _) => {
                    assert!(
                        html.contains(&format!("src=\"{escaped_combo}")),
                        "{label}: {html}"
                    );
                }
                (false, _, true) => {
                    // Bootstrap combo fallback src is escaped too.
                    assert!(
                        html.contains(&format!("src=\"{escaped_combo}")),
                        "{label}: {html}"
                    );
                    assert!(!html.contains("PASTE</script>x"), "{label}: {html}");
                }
                _ => {}
            }
            let expected_href = if per_module {
                "href=\"https://cdn.example/a&amp;copy;/css/site.h.v1.min.css\"".to_string()
            } else {
                format!("href=\"{escaped_combo}")
            };
            assert!(html.contains(&expected_href), "{label}: {html}");
            // urls keep the raw path — no entity-escaped text is recorded.
            assert!(
                rendered[0].urls.iter().all(|u| !u.contains("&amp;")),
                "{label}: {:?}",
                rendered[0].urls
            );
        }
    }

    #[test]
    fn test_per_module_bootstrap_falls_back_to_versioned_files() {
        // Runtime bytes that cannot inline would end the tag early. Under
        // the PAGE-level per-module setting there is no combo handler
        // behind the page, so the runtime's own versioned file loads
        // synchronously, then the config script. A jam's own `perModule`
        // moves only that jam's group onto file URLs — the bootstrap
        // keeps whatever the page-level setting says, since the page API
        // is incremental and a later event cannot rewrite a bootstrap the
        // first dump already emitted.
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = js_manifest(&[("a", &[])]);
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE</script>x",
        );
        write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");
        let dirs = OutputDirs::under(dir.path());

        // Page-level per-module: bootstrap file + group file.
        let rendered = render_all(
            &manifest,
            &dirs,
            &per_module_opts(),
            &[jam(&[], &["a"]), Event::Dump(DumpScope::Js)],
        )
        .unwrap();
        assert_eq!(
            rendered[0].urls,
            vec![
                "/js/paste.h.v1.min.js".to_string(),
                "/js/a.h.v2.min.js".to_string()
            ],
            "{:?}",
            rendered[0].urls
        );
        assert!(
            rendered[0].html.contains(
                "<script type=\"text/javascript\" src=\"/js/paste.h.v1.min.js\"></script>"
            ),
            "{}",
            rendered[0].html
        );
        assert!(rendered[0].html.contains("paste.config({packagesUrl:"));
        assert!(!rendered[0].html.contains("src=\"/paste/"));
        assert!(
            rendered[0].urls.iter().all(|u| !u.starts_with("/paste/")),
            "{:?}",
            rendered[0].urls
        );
        assert!(!rendered[0].html.contains("PASTE</script>x"));

        // Event-level perModule alone: the bootstrap is still a combo page
        // — the runtime loads through the combo URL — while the jam's own
        // group honors its override with a file URL.
        let rendered = render_all(
            &manifest,
            &dirs,
            &RenderOptions {
                inline_threshold: 0,
                ..opts()
            },
            &[
                Event::Jam(Jam {
                    js: vec!["a".to_string()],
                    per_module: Some(true),
                    ..Jam::default()
                }),
                Event::Dump(DumpScope::Js),
            ],
        )
        .unwrap();
        assert_eq!(
            rendered[0].urls,
            vec![
                "/paste/1705312800/paste+v1.0.min.js".to_string(),
                "/js/a.h.v2.min.js".to_string()
            ],
            "event-level perModule scopes to its own group only: {:?}",
            rendered[0].urls
        );
        assert!(
            rendered[0]
                .html
                .contains("src=\"/paste/1705312800/paste+v1.0.min.js\""),
            "{}",
            rendered[0].html
        );
        assert!(rendered[0].html.contains("paste.config"));

        // A jam naming only the runtime under event-level perModule: the
        // bootstrap still follows the page-level setting — combo for the
        // runtime, no group URLs.
        let events = vec![
            Event::Jam(Jam {
                js: vec!["paste".to_string()],
                per_module: Some(true),
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Js),
        ];
        let rendered = render_all(
            &manifest,
            &dirs,
            &RenderOptions {
                inline_threshold: 0,
                ..opts()
            },
            &events,
        )
        .unwrap();
        assert_eq!(
            rendered[0].urls,
            vec!["/paste/1705312800/paste+v1.0.min.js".to_string()],
            "{:?}",
            rendered[0].urls
        );
        assert!(
            rendered[0]
                .html
                .contains("src=\"/paste/1705312800/paste+v1.0.min.js\""),
            "{}",
            rendered[0].html
        );
        assert!(
            rendered[0].html.contains("paste.config"),
            "{}",
            rendered[0].html
        );
    }

    #[test]
    fn test_per_module_bootstrap_missing_or_corrupt_runtime_fails() {
        // The per-module fallback serves real files, so an unreadable or
        // unverifiable runtime is a render error, not a tag.
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = js_manifest(&[("a", &[])]);
        write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
        let dirs = OutputDirs::under(dir.path());

        // Baseline: the recorded runtime inlines and the render succeeds.
        let rendered = render_all(&manifest, &dirs, &per_module_opts(), &events).unwrap();
        assert!(
            rendered[0].html.contains("PASTE_RT"),
            "{}",
            rendered[0].html
        );

        // Removing the runtime file is a missing-file failure.
        fs::remove_file(dir.path().join("js/paste.h.v1.min.js")).unwrap();
        let err = render_all(&manifest, &dirs, &per_module_opts(), &events).unwrap_err();
        assert!(err.to_string().contains("paste"), "{err}");
        assert!(err.to_string().contains("readable, verified"), "{err}");

        // Bytes that no longer match the recorded hash are a corrupt-file
        // failure — the manifest still names the PASTE_RT hash.
        fs::write(dir.path().join("js/paste.h.v1.min.js"), "wrong bytes").unwrap();
        let err = render_all(&manifest, &dirs, &per_module_opts(), &events).unwrap_err();
        assert!(err.to_string().contains("paste"), "{err}");
        assert!(err.to_string().contains("readable, verified"), "{err}");
    }

    #[test]
    fn test_render_never_inlines_unsafe_content() {
        // Content that could end the tag early is served over a URL even when
        // the jam forces inlining — checked case-insensitively.
        let mut manifest = {
            let mut m = js_manifest(&[("evil", &[]), ("comment", &[])]);
            m.assets.insert(
                "css/evil.css".to_string(),
                asset("/css/evil.h.v1.min.css", 1, STAMP, &[]),
            );
            m
        };
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(
            &mut manifest,
            dir.path(),
            "/js/evil.h.v2.min.js",
            "var e=1;</SCRIPT",
        );
        write_out(
            &mut manifest,
            dir.path(),
            "/js/comment.h.v3.min.js",
            "<!--hidden",
        );
        write_out(
            &mut manifest,
            dir.path(),
            "/css/evil.h.v1.min.css",
            ".e{}</STYLE",
        );

        let events = vec![
            Event::Jam(Jam {
                js: vec!["evil".to_string(), "comment".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Js),
            Event::Jam(Jam {
                css: vec!["evil".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Css),
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
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        // a's compiled file is absent → external tag.

        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
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
            Event::Dump(DumpScope::Css),
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
        write_out(&mut manifest, dir.path(), "/css/a.h.v1.min.css", ".a{}");
        let events = vec![
            Event::Jam(Jam {
                css: vec!["a".to_string()],
                media: Some("print".to_string()),
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Css),
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
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );

        // Group members' stamps rule: the newer one wins.
        let events = vec![jam(&[], &["a", "b"]), Event::Dump(DumpScope::Js)];
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
            Event::Dump(DumpScope::Css),
            jam(&["b"], &[]),
            Event::Dump(DumpScope::Css),
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
        let events = vec![jam(&["bad name!"], &[]), Event::Dump(DumpScope::Css)];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("bad name!"), "{err}");

        let events = vec![jam(&[], &["../x"]), Event::Dump(DumpScope::Js)];
        assert!(render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).is_err());
    }

    #[test]
    fn test_render_unsafe_runtime_goes_external_with_config_script() {
        // Unsafe runtime content → external src plus a separate config script.
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE</script>x",
        );
        write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");

        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
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

        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("paste"), "{err}");
        assert!(err.to_string().contains("js"), "{err}");
    }

    #[test]
    fn test_render_unknown_module_errors_naming_module_and_type() {
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");

        let events = vec![jam(&["ghost"], &[]), Event::Dump(DumpScope::Css)];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
        assert!(err.to_string().contains("css"), "{err}");

        let events = vec![jam(&[], &["ghost"]), Event::Dump(DumpScope::Js)];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
        assert!(err.to_string().contains("js"), "{err}");

        // A dependency that resolves to nothing fails the same way.
        let mut manifest = js_manifest(&[("a", &[])]);
        manifest.assets.get_mut("js/a.js").unwrap().dependencies = vec!["ghost".to_string()];
        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn test_render_cdn_base_prefixes_tags_but_not_urls() {
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );

        let options = RenderOptions {
            cdn_base: "https://cdn.example".to_string(),
            ..opts()
        };
        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
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
        // cdn_base unusable inside a script or attribute; `?` and `#`
        // would move the asset path into query or fragment data.
        for bad in [
            "https://cdn example",
            "https://cdn'ex",
            "x`y",
            "x<y",
            "x\\y",
            "https://cdn.example?token=abc",
            "https://cdn.example#frag",
            // Encoded hosts may be rejected or reinterpreted by WHATWG URLs.
            "https://cdn.%30/x",
            "//cdn.%2eexample/x",
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

        // An explicitly UTF-8-encoded internationalized hostname is
        // valid, and rendering preserves the caller's CDN prefix bytes.
        let idn = RenderOptions {
            cdn_base: "https://cdn.%C3%A9xample".to_string(),
            ..opts()
        };
        let idn_markup =
            render_all(&manifest, &OutputDirs::under(dir.path()), &idn, &events).unwrap();
        assert!(idn_markup[0]
            .html
            .contains("https://cdn.%C3%A9xample/paste/"));

        // A trailing slash on the prefix is canonical: the emitted URL
        // carries exactly one separator, never `//`.
        let options = RenderOptions {
            cdn_base: "https://cdn.example/".to_string(),
            ..opts()
        };
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &options, &events).unwrap();
        assert!(
            rendered[0].html.contains("https://cdn.example/paste/"),
            "single separator after a slashed prefix: {}",
            rendered[0].html
        );
        assert!(
            !rendered[0].html.contains("https://cdn.example//"),
            "no doubled separator: {}",
            rendered[0].html
        );
    }

    #[test]
    fn test_render_base_path_rejects_unsafe_characters() {
        // base_path lands in the same attributes and the packagesUrl JS
        // string as cdn_base, so it is held to the same character rule on
        // top of the leading/trailing slash rule.
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");
        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];

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
    // Finish — the terminal completion check
    // ============================================================

    fn media_jam(css: &[&str], js: &[&str], media: &str) -> Event {
        Event::Jam(Jam {
            css: css.iter().map(|s| s.to_string()).collect(),
            js: js.iter().map(|s| s.to_string()).collect(),
            media: Some(media.to_string()),
            ..Jam::default()
        })
    }

    #[test]
    fn test_finish_errors_when_a_css_jam_is_never_dumped() {
        // The reported defect: the css dump already ran when the jam lands,
        // and ending on a js dump used to drop `site` silently.
        let manifest = mixed_manifest();
        let dir = tempfile::tempdir().unwrap();
        let events = vec![
            Event::Dump(DumpScope::Css),
            jam(&["site"], &["app"]),
            Event::Dump(DumpScope::Js),
            Event::Finish,
        ];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("css"), "{msg}");
        assert!(msg.contains("site"), "{msg}");
        assert!(!msg.contains("app"), "{msg}");
        assert!(validate_finished(&manifest, &events).is_err());
    }

    #[test]
    fn test_finish_errors_when_a_js_jam_is_never_dumped() {
        let manifest = mixed_manifest();
        let dir = tempfile::tempdir().unwrap();
        let events = vec![
            Event::Dump(DumpScope::Js),
            jam(&["site"], &["app"]),
            Event::Dump(DumpScope::Css),
            Event::Finish,
        ];
        let err =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("js"), "{msg}");
        assert!(msg.contains("app"), "{msg}");
        assert!(!msg.contains("site"), "{msg}");
    }

    #[test]
    fn test_finish_passes_once_every_type_is_dumped() {
        // A jam can queue both types at once; dumping them across separate
        // checkpoints and finishing is the healthy flow.
        let mut manifest = mixed_manifest();
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        let events = vec![
            jam(&["site"], &["app"]),
            Event::Dump(DumpScope::Css),
            Event::Dump(DumpScope::Js),
            Event::Finish,
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        // finish adds no Rendered — one per dump, as before.
        assert_eq!(rendered.len(), 2);
        assert!(
            rendered[0].html.contains("site+v1.0.min.css"),
            "{:?}",
            rendered[0].html
        );
        assert!(rendered[1].html.contains("app"), "{:?}", rendered[1].html);
        validate_finished(&manifest, &events).unwrap();
    }

    #[test]
    fn test_finish_dedups_by_media_and_bootstrap_counts_paste() {
        let mut manifest = mixed_manifest();
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );

        // Re-jamming a stylesheet under the SAME media after its dump is not
        // outstanding — the tag it wanted was already emitted.
        let events = vec![
            jam(&["site"], &[]),
            Event::Dump(DumpScope::Css),
            jam(&["site"], &[]),
            Event::Finish,
        ];
        validate_finished(&manifest, &events).unwrap();

        // Under a different media it is a different tag — still owed.
        let events = vec![
            jam(&["site"], &[]),
            Event::Dump(DumpScope::Css),
            media_jam(&["site"], &[], "print"),
            Event::Finish,
        ];
        let err = validate_finished(&manifest, &events).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("site"), "{msg}");
        assert!(msg.contains("print"), "{msg}");

        // The bootstrap already delivered `paste`: re-jamming it and
        // finishing without another dump is complete.
        let events = vec![
            jam(&[], &["app"]),
            Event::Dump(DumpScope::Js),
            jam(&[], &["paste"]),
            Event::Finish,
        ];
        validate_finished(&manifest, &events).unwrap();
    }

    #[test]
    fn test_finish_empty_page_and_position_rules() {
        let manifest = mixed_manifest();
        let dir = tempfile::tempdir().unwrap();

        // An empty page finishes trivially.
        validate_finished(&manifest, &[Event::Finish]).unwrap();
        assert!(render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &opts(),
            &[Event::Finish]
        )
        .unwrap()
        .is_empty());

        // finish only ends a page — anything after it is an error, and a
        // dump-terminated list is not finished.
        for events in [
            vec![Event::Finish, Event::Dump(DumpScope::Js)],
            vec![
                jam(&["site"], &[]),
                Event::Dump(DumpScope::Css),
                Event::Finish,
                Event::Dump(DumpScope::Js),
            ],
        ] {
            assert!(
                render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).is_err()
            );
        }
        assert!(
            validate_finished(&manifest, &[Event::Dump(DumpScope::Css)]).is_err(),
            "a dump does not seal the page"
        );
        assert!(validate_finished(&manifest, &[]).is_err());
    }

    // ============================================================
    // Combined dumps
    // ============================================================

    #[test]
    fn test_render_all_dump_matches_separate_dumps() {
        // A combined dump renders exactly the css dump's tags, a newline,
        // and the js dump's tags — one Rendered for the markup a template
        // would emit in two.
        let mut manifest = mixed_manifest();
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );

        let separate = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &opts(),
            &[
                jam(&["site"], &["app"]),
                Event::Dump(DumpScope::Css),
                Event::Dump(DumpScope::Js),
            ],
        )
        .unwrap();
        let combined = render_all(
            &manifest,
            &OutputDirs::under(dir.path()),
            &opts(),
            &[jam(&["site"], &["app"]), Event::Dump(DumpScope::All)],
        )
        .unwrap();

        assert_eq!(separate.len(), 2);
        assert_eq!(combined.len(), 1);
        assert_eq!(
            combined[0].html,
            format!("{}\n{}", separate[0].html, separate[1].html)
        );
        let mut urls = separate[0].urls.clone();
        urls.extend(separate[1].urls.iter().cloned());
        assert_eq!(combined[0].urls, urls);
    }

    #[test]
    fn test_render_all_after_css_dump_emits_only_js() {
        // `all` plans both queues against the loaded state: a stylesheet the
        // earlier css dump already delivered is not re-emitted, and the
        // empty css half leaves no stray newline before the js tags.
        let mut manifest = mixed_manifest();
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );

        let events = vec![
            jam(&["site"], &["app"]),
            Event::Dump(DumpScope::Css),
            Event::Dump(DumpScope::All),
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert_eq!(rendered.len(), 2);
        let only_js = &rendered[1];
        assert!(!only_js.html.contains("site"), "{}", only_js.html);
        assert!(only_js.html.starts_with("<script"), "{}", only_js.html);
        assert!(!only_js.html.contains("stylesheet"), "{}", only_js.html);
        assert!(only_js.urls.iter().all(|u| u.ends_with(".min.js")));

        // The symmetric case: an empty js half ends on the css tag.
        let events = vec![jam(&["site"], &[]), Event::Dump(DumpScope::All)];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert!(
            rendered[0].html.starts_with("<link"),
            "{}",
            rendered[0].html
        );
        assert!(
            !rendered[0].html.contains("<script"),
            "{}",
            rendered[0].html
        );
        assert!(
            !rendered[0].html.ends_with('\n'),
            "no trailing newline from an empty side"
        );
    }

    #[test]
    fn test_finish_passes_after_a_single_all_dump() {
        // One combined dump delivers both queues — finish validates it
        // exactly like separate dumps do.
        let mut manifest = mixed_manifest();
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        let events = vec![
            jam(&["site"], &["app"]),
            Event::Dump(DumpScope::All),
            Event::Finish,
        ];
        let rendered =
            render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
        assert_eq!(rendered.len(), 1);
        assert!(
            rendered[0].html.contains("site+v1.0.min.css"),
            "{}",
            rendered[0].html
        );
        assert!(rendered[0].html.contains("app"), "{}", rendered[0].html);
        validate_finished(&manifest, &events).unwrap();
    }

    #[test]
    fn test_finish_names_an_unknown_pending_module() {
        // A jammed module with no manifest asset surfaces as itself, not as
        // a generic "still pending" — the same error the dump would raise.
        let manifest = mixed_manifest();
        let events = vec![jam(&["nosuch"], &[]), Event::Finish];
        let err = validate_finished(&manifest, &events).unwrap_err();
        assert!(format!("{err:#}").contains("nosuch"), "{err:#}");
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
        write_out(&mut manifest, dir.path(), "/js/a.h.v1.min.js", "A");
        write_out(&mut manifest, dir.path(), "/js/b.h.v1.min.js", "B");
        write_out(
            &mut manifest,
            dir.path(),
            "/css/base.h.v1.min.css",
            ".base{}",
        );
        write_out(
            &mut manifest,
            dir.path(),
            "/css/theme.h.v1.min.css",
            ".theme{}",
        );
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
    fn test_combo_fully_pinned_order_is_the_address() {
        let (manifest, dir) = combo_manifest();
        let dirs = OutputDirs::under(dir.path());

        // b pinned before its dependency a is a complete address: the
        // listed order names one exact byte sequence and serves it —
        // dependency order is not consulted while every entry is pinned.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG}/b+v1.0~a+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "B\n;\nA");
        assert_eq!(r.cache_control, "public, max-age=31536000");
        assert!(r.cacheable);
        assert_eq!(r.location, None);

        // The same members in dependency order are a different address
        // serving a different byte sequence — neither redirects to the
        // other.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A\n;\nB");
        assert_eq!(r.location, None);
    }

    #[test]
    fn test_combo_unpinned_entry_reorders_to_the_sorted_url() {
        let (manifest, dir) = combo_manifest();

        // One unpinned entry is enough: the request's order is not a
        // complete address, so the canonical list is the manifest's
        // dependency order — a 302, not reordered content.
        let r = combo(
            &manifest,
            &OutputDirs::under(dir.path()),
            "/paste",
            &format!("/paste/{SEG}/b+v1.0~a.min.js"),
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
            &format!("/paste/{SEG}/b+v1.0~a.min.js"),
            Some("filter=b"),
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js?filter=b").as_str())
        );
    }

    #[test]
    fn test_combo_rejects_a_query_that_cannot_be_a_header_value() {
        // The query is echoed verbatim into a redirect's Location — a
        // response header. Raw control bytes, a raw space, or unencoded
        // non-ASCII must never reach it: the request answers 404 with no
        // location rather than emitting an unwritable or smuggling header.
        let (manifest, dir) = combo_manifest();
        let dirs = OutputDirs::under(dir.path());
        let noncanonical = format!("/paste/{SEG}/b+v1.0~a.min.js");

        for query in [
            "x=one\r\nInjected:yes",
            "x=one\nInjected:yes",
            "x=a\tb",
            "x=a\x07b",
            "x=a\u{7f}b",
            "x=a\u{9b}b",
            "x=a b",
            "x=café",
        ] {
            let r = combo(&manifest, &dirs, "/paste", &noncanonical, Some(query));
            assert_eq!(r.status, 404, "{query:?}");
            assert_eq!(r.cache_control, "no-cache", "{query:?}");
            assert_eq!(r.location, None, "{query:?}");
        }

        // A percent-escaped control stays escaped — %0D%0A is valid
        // ASCII on the wire and is never decoded into raw CR/LF.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &noncanonical,
            Some("x=%0D%0A&filter=b"),
        );
        assert_eq!(r.status, 302);
        let location = r.location.as_deref().unwrap();
        assert!(location.contains("x=%0D%0A"), "{location}");
        assert!(
            location.chars().all(|c| c.is_ascii_graphic()),
            "Location carries a non-graphic byte: {location:?}"
        );

        // The ordinary query contract is unchanged: filter, require and
        // free-form pairs pass through byte-identical.
        for query in ["filter=b", "x=1&y=2", "q=a+b%20c&flag="] {
            let r = combo(&manifest, &dirs, "/paste", &noncanonical, Some(query));
            assert_eq!(r.status, 302, "{query:?}");
            assert_eq!(
                r.location.as_deref(),
                Some(format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js?{query}").as_str()),
                "{query:?}"
            );
        }
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
        write_out(&mut manifest, dir.path(), "/js/site/café.h.v1.min.js", "C");

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
        write_out(&mut manifest, dir.path(), "/js/a.h.v1.min.js", "A");
        write_out(&mut manifest, dir.path(), "/js/b.h.v1.min.js", "B");
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
        write_out(
            &mut manifest,
            dir.path(),
            "/js/site/café.h.v1.min.js",
            "CAFE",
        );
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
    /// v1 with no history — the shape a dist manifest records for an asset
    /// that never changed. Hashes are the real MD5 of each fixture body, so
    /// the qualified URL form can pin them.
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
        write_out(&mut manifest, dir.path(), "/js/a.h.v1.min.js", "A1");
        write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A2");
        write_out(&mut manifest, dir.path(), "/js/b.h.v1.min.js", "B");
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
    fn test_combo_noncanonical_spellings_redirect() {
        let (manifest, dir) = versioned_manifest();
        let dirs = OutputDirs::under(dir.path());
        let canonical = format!("/paste/{SEG}/a+v1.0.min.js");
        assert_eq!(
            combo(&manifest, &dirs, "/paste", &canonical, None).status,
            200
        );
        for path in [
            format!("/paste/0{SEG}/a+v1.0.min.js"),
            format!("/paste/{SEG}/%61+v1.0.min.js"),
            format!("/paste/{SEG}/a%2Bv1.0.min.js"),
        ] {
            let r = combo(&manifest, &dirs, "/paste", &path, None);
            assert_eq!(r.status, 302, "{path}");
            assert_eq!(r.location.as_deref(), Some(canonical.as_str()), "{path}");
        }
    }

    #[test]
    fn test_combo_duplicate_entries_must_agree_on_version() {
        let (manifest, dir) = versioned_manifest();
        let dirs = OutputDirs::under(dir.path());

        // The same version twice is one module spelled non-canonically:
        // it redirects to the single-entry URL instead of caching a copy.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG}/a+v1.0~a+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG}/a+v1.0.min.js").as_str())
        );

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

        // A loader segment this manifest never recorded is a fingerprint
        // from another build's page — a 404, never a substitution for the
        // current build's content.
        let r = combo(&manifest, &dirs, "/paste", "/paste/deadbeef0000/a.js", None);
        assert_eq!(r.status, 404);
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
    fn test_combo_all_digit_packages_segment_resolves_the_record_not_a_stamp() {
        let (mut manifest, dir) = combo_manifest();
        let dirs = OutputDirs::under(dir.path());

        // A packages fingerprint is 12 hex chars — all digits is rare but
        // possible. A saved page holding one must resolve its record, not
        // canonicalize the segment as a stamp.
        manifest.packages_segments.insert(
            "123456789012".to_string(),
            [(
                "a".to_string(),
                crate::manifest::SegmentPin {
                    key: "js/a.js".to_string(),
                    version: 1,
                    hash: manifest.assets["js/a.js"].hash.clone(),
                },
            )]
            .into_iter()
            .collect(),
        );
        let r = combo(&manifest, &dirs, "/paste", "/paste/123456789012/a.js", None);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A");
        assert_eq!(r.cache_control, "public, max-age=31536000");
        assert!(r.cacheable);
    }

    #[test]
    fn test_packages_segment_tracks_version_and_alias_identity() {
        // The fingerprint names the loader's identity, not just its bytes:
        // a metadata-only version increment or an alias remapping changes
        // what a fresh loader URL pins even when every emitted byte and
        // the layering stay identical — the segment must move with it so a
        // saved page's URL and a new page's URL cannot collide.
        let (manifest, _dir) = combo_manifest();

        let mut bumped = manifest.clone();
        bumped.assets.get_mut("js/a.js").unwrap().version = 2;
        assert_ne!(
            packages_segment(&manifest),
            packages_segment(&bumped),
            "a version increment changes the fingerprint at identical bytes"
        );

        let mut aliased = manifest.clone();
        aliased
            .modules
            .insert("app".to_string(), "js/a.js".to_string());
        assert_ne!(
            packages_segment(&manifest),
            packages_segment(&aliased),
            "an alias mapping changes the fingerprint at identical bytes"
        );

        // The legacy fingerprint sees only bytes and order: an unchanged
        // content set keeps the segment older pages were rendered
        // against — the first-upgrade record is what lets those URLs
        // resolve their own pin map.
        assert_eq!(
            legacy_packages_segment(&manifest),
            legacy_packages_segment(&bumped)
        );
    }

    #[test]
    fn test_combo_historical_loader_serves_the_snapshot_in_listed_order() {
        let (mut manifest, dir) = versioned_manifest();
        let dirs = OutputDirs::under(dir.path());

        // A recorded snapshot pins a@v1 (a history version today) and
        // b@v1. The page's URL lists a first — the listed order is the
        // contract, not today's layering which puts b before a.
        let pin = |key: &str, version: u32, hash: &str| crate::manifest::SegmentPin {
            key: key.to_string(),
            version,
            hash: hash.to_string(),
        };
        manifest.packages_segments.insert(
            "fedcba987654".to_string(),
            [
                (
                    "a".to_string(),
                    pin("js/a.js", 1, &manifest.assets["js/a.js"].history[0].hash),
                ),
                (
                    "b".to_string(),
                    pin("js/b.js", 1, &manifest.assets["js/b.js"].hash),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/fedcba987654/a~b.js",
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(
            r.body, "A1\n;\nB",
            "the snapshot serves listed order, not current dependency order"
        );
        assert_eq!(r.cache_control, "public, max-age=31536000");

        // A name the snapshot never recorded is a 404 — the surviving
        // members are never served as a partial group.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/fedcba987654/a~ghost.js",
            None,
        );
        assert_eq!(r.status, 404);
        assert_eq!(r.cache_control, "no-cache");

        // `?filter=` still drops named members — the rest serve in the
        // listed order the page wrote.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/fedcba987654/a~b.js",
            Some("filter=b"),
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A1");

        // But filtering happens after identity validation: a name the
        // snapshot never recorded is a 404 even when `filter` names it,
        // and a member whose recorded hash no longer matches cannot be
        // filtered out of the check either.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/fedcba987654/a~ghost.js",
            Some("filter=ghost"),
        );
        assert_eq!(r.status, 404);
        assert_eq!(r.cache_control, "no-cache");

        manifest
            .packages_segments
            .get_mut("fedcba987654")
            .unwrap()
            .get_mut("b")
            .unwrap()
            .hash = "ffffffffffffffffffffffffffffffff".to_string();
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/fedcba987654/a~b.js",
            Some("filter=b"),
        );
        assert_eq!(r.status, 404);
        assert_eq!(r.cache_control, "no-cache");

        // An all-digit fingerprint the manifest never recorded is a 404 —
        // it is a fingerprint, never a timestamp to canonicalize.
        let r = combo(&manifest, &dirs, "/paste", "/paste/987654321098/a.js", None);
        assert_eq!(r.status, 404);
        assert_eq!(r.cache_control, "no-cache");

        // A plain stale stamp still canonicalizes to the pinned form —
        // non-fingerprint segments behave like every versioned request.
        let r = combo(&manifest, &dirs, "/paste", "/paste/1700000000/a.js", None);
        assert_eq!(r.status, 302);
    }

    #[test]
    fn test_combo_historical_loader_refuses_a_pin_hash_mismatch() {
        // The pin's hash is checked against the resolved version's record
        // before a byte is read: a same-key, same-version slot holding
        // different content — a local rebuild, a forged record — is a 404,
        // never a substitution.
        let (mut manifest, dir) = combo_manifest();
        let dirs = OutputDirs::under(dir.path());
        manifest.packages_segments.insert(
            "0badf00d1234".to_string(),
            [(
                "a".to_string(),
                crate::manifest::SegmentPin {
                    key: "js/a.js".to_string(),
                    version: 1,
                    hash: "ffffffffffffffffffffffffffffffff".to_string(),
                },
            )]
            .into_iter()
            .collect(),
        );
        let r = combo(&manifest, &dirs, "/paste", "/paste/0badf00d1234/a.js", None);
        assert_eq!(r.status, 404);
        assert_eq!(r.cache_control, "no-cache");

        // A pin without a verifiable hash — an old record's empty field —
        // answers the same way.
        manifest.packages_segments.insert(
            "0badf00d5678".to_string(),
            [(
                "a".to_string(),
                crate::manifest::SegmentPin {
                    key: "js/a.js".to_string(),
                    version: 1,
                    hash: String::new(),
                },
            )]
            .into_iter()
            .collect(),
        );
        let r = combo(&manifest, &dirs, "/paste", "/paste/0badf00d5678/a.js", None);
        assert_eq!(r.status, 404);
    }

    #[test]
    fn test_unverifiable_hashes_never_serve_or_inline() {
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        let output = manifest.assets["js/a.js"].output.clone();
        let version = manifest.assets["js/a.js"].version;
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        write_out(&mut manifest, dir.path(), &output, "A");
        let dirs = OutputDirs::under(dir.path());
        let pinned = format!("/paste/{SEG}/a+v{version}.0.min.js");
        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];

        // Baseline: the recorded version serves and inlines its bytes.
        let r = combo(&manifest, &dirs, "/paste", &pinned, None);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A");
        let rendered = render_all(&manifest, &dirs, &opts(), &events).unwrap();
        assert!(
            rendered[0].html.contains("A</script"),
            "baseline render inlines the module: {}",
            rendered[0].html
        );

        // A recorded hash that is not a real content hash verifies
        // nothing: pinned, loader-shaped, and unversioned requests all
        // answer 404 rather than serving bytes no record can vouch for.
        manifest.assets.get_mut("js/a.js").unwrap().hash = "not-a-hash".to_string();
        for path in [
            pinned.clone(),
            "/paste/a.js".to_string(),
            format!("/paste/{}/a.js", packages_segment(&manifest)),
        ] {
            let r = combo(&manifest, &dirs, "/paste", &path, None);
            assert_eq!(r.status, 404, "{path}");
            assert!(!r.cacheable, "{path}");
        }

        // And the renderer refuses to embed it: the group degrades to the
        // external tag instead of inlining unverifiable bytes.
        let rendered = render_all(&manifest, &dirs, &opts(), &events).unwrap();
        assert!(
            rendered[0].html.contains(".min.js"),
            "an unverifiable group falls back to the external tag: {}",
            rendered[0].html
        );
        assert!(
            !rendered[0].html.contains("A</script"),
            "unverifiable bytes must never be inlined: {}",
            rendered[0].html
        );

        // Tampered bytes are just as unservable — the record is honest,
        // the file is not.
        manifest.assets.get_mut("js/a.js").unwrap().hash = format!("{:x}", md5::compute(b"A"));
        fs::write(dir.path().join(output.trim_start_matches('/')), "TAMPERED").unwrap();
        let r = combo(&manifest, &dirs, "/paste", &pinned, None);
        assert_eq!(r.status, 404);
        let rendered = render_all(&manifest, &dirs, &opts(), &events).unwrap();
        assert!(
            !rendered[0].html.contains("TAMPERED"),
            "corrupted bytes must never be inlined: {}",
            rendered[0].html
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
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        let dirs = OutputDirs {
            base: dir.path().to_path_buf(),
            js: dir.path().join("assets-js"),
            css: dir.path().join("assets-css"),
        };
        fs::create_dir_all(&dirs.js).unwrap();
        fs::write(dirs.js.join("paste.h.v1.min.js"), "PASTE_RT").unwrap();
        fs::write(dirs.js.join("a.h.v2.min.js"), "CUSTOM_A").unwrap();
        // The recorded hashes must match the bytes serving verifies —
        // write_out re-stamps them, but this fixture's custom type
        // directories are not where it writes, so they are set directly.
        for (key, content) in [("js/paste.js", "PASTE_RT"), ("js/a.js", "CUSTOM_A")] {
            manifest.assets.get_mut(key).unwrap().hash = format!("{:x}", md5::compute(content));
        }

        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
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
        let mut manifest = js_manifest(&[("a", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT // trailing",
        );
        write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");

        let events = vec![jam(&[], &["a"]), Event::Dump(DumpScope::Js)];
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
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        fs::write(dir.path().join("outside.js"), "SECRET").unwrap();

        let events = vec![
            Event::Jam(Jam {
                js: vec!["a".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Js),
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
        let outside = dir.path().join("outside.js");
        fs::write(&outside, "SECRET").unwrap();

        let mut manifest = js_manifest(&[("a", &[])]);
        write_out(
            &mut manifest,
            dir.path(),
            "/js/paste.h.v1.min.js",
            "PASTE_RT",
        );
        manifest.assets.get_mut("js/a.js").unwrap().output =
            format!("/js/{}", outside.to_str().unwrap());

        let events = vec![
            Event::Jam(Jam {
                js: vec!["a".to_string()],
                inline: true,
                ..Jam::default()
            }),
            Event::Dump(DumpScope::Js),
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
            // Route-pattern binders — `{}` names a path matcher in the host
            // frameworks this path is embedded into.
            "/a/{rest}",
            "/{rest}",
            // Not ASCII — the value lands raw in attributes and headers.
            "/päste",
            "/日本語",
        ] {
            assert!(validate_base_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_validate_cdn_prefix_rejects_query_fragment_and_markup() {
        // A prefix is joined verbatim to asset paths: `?` would move the
        // asset path into query data, `#` into the fragment — the asset
        // would never be fetched from the CDN at all. Markup/quote
        // characters would break out of the emitted attribute.
        for good in [
            "",
            "https://cdn.example",
            "https://cdn.example/assets",
            "https://cdn.example/assets/",
            "//cdn.example/x",
            "http://localhost:8080/cdn",
        ] {
            assert!(validate_cdn_prefix(good).is_ok(), "{good}");
        }
        for bad in [
            "https://cdn.example?x=1",
            "https://cdn.example/?q=",
            "https://cdn.example#frag",
            "https://cdn.example/#",
            "https://cdn.example/\"",
            "https://cdn.example/'",
            "https://cdn.example/<tag>",
            "https://cdn.example/`",
            "https://cdn.example/a b",
            "https://cdn.example/\\path",
            "https://cdn.example/\n",
            // Truncated or incomplete origins: a `//` comment misparse
            // leaves `https:` behind; these used to pass validation and
            // record an unusable origin in the manifest.
            "https:",
            "http:",
            "https://",
            "https:///assets",
            "http:///x",
            "//",
            "///x",
            "//:8080/x",
            "ftp://cdn.example/x",
            "localhost:8080/x",
            // Bare relative prefixes: `cdn/js/...` resolves against the
            // requesting document's own path, so every page would fetch
            // from a different location.
            "cdn",
            "assets/x",
            "static",
            "./cdn",
            "../up",
        ] {
            assert!(validate_cdn_prefix(bad).is_err(), "{bad}");
        }
        for good in ["/assets", "//cdn.example/x", "http://localhost:8080/cdn"] {
            assert!(validate_cdn_prefix(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn test_validate_cdn_prefix_rejects_signed_ports() {
        for base in ["https://cdn.example.com:+80", "https://[::1]:+443/x"] {
            assert!(validate_cdn_prefix(base).is_err(), "{base}");
        }
        assert!(validate_cdn_prefix("https://cdn.example.com:8080").is_ok());
    }

    #[test]
    fn test_combo_strips_one_min_suffix_without_consulting_the_manifest() {
        // `.min` is the URL's suffix, never part of a module name: the
        // loader's `vendor.min.js` and the plain `vendor.js` both name
        // `vendor`. A manifest written before the rule that still records
        // a `vendor.min` module changes nothing about that reading — the
        // old module stays reachable only with its own name plus a
        // suffix, `vendor.min.min.js`.
        let mut manifest = js_manifest(&[("vendor", &[]), ("vendor.min", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(&mut manifest, dir.path(), "/js/vendor.h.v2.min.js", "PLAIN");
        write_out(
            &mut manifest,
            dir.path(),
            "/js/vendor/min.h.v3.min.js",
            "OLD",
        );
        manifest
            .modules
            .insert("vendor.min".into(), "js/vendor/min.js".into());
        let dirs = OutputDirs::under(dir.path());
        for path in ["/paste/vendor.min.js", "/paste/vendor.js"] {
            let r = combo(&manifest, &dirs, "/paste", path, None);
            assert_eq!(r.status, 200, "{path}: {}", r.body);
            assert_eq!(r.body, "PLAIN", "{path}");
        }
        let r = combo(&manifest, &dirs, "/paste", "/paste/vendor.min.min.js", None);
        assert_eq!(r.body, "OLD");
    }

    #[test]
    fn test_a_pre_rule_segment_never_serves_a_plain_twin_for_its_min_name() {
        // A segment recorded before `.min` left module names holds
        // `vendor`, `vendor.min` and `lib.min`. Its `vendor.min.js` and
        // `lib.min.js` meant the recorded `.min` entries: they serve those
        // pinned bytes, never `vendor`'s; `vendor.js` still serves `vendor`.
        let mut manifest = js_manifest(&[("vendor", &[]), ("vendor.min", &[]), ("lib.min", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(&mut manifest, dir.path(), "/js/vendor.h.v2.min.js", "PLAIN");
        write_out(
            &mut manifest,
            dir.path(),
            "/js/vendor/min.h.v3.min.js",
            "OLD",
        );
        write_out(
            &mut manifest,
            dir.path(),
            "/js/lib/min.h.v4.min.js",
            "LIBMIN",
        );
        let pin = |key: &str, version: u32, manifest: &Manifest| crate::manifest::SegmentPin {
            key: key.to_string(),
            version,
            hash: manifest.assets[key].hash.clone(),
        };
        let snapshot: BTreeMap<String, crate::manifest::SegmentPin> = [
            ("vendor".to_string(), pin("js/vendor.js", 2, &manifest)),
            (
                "vendor.min".to_string(),
                pin("js/vendor/min.js", 3, &manifest),
            ),
            ("lib.min".to_string(), pin("js/lib/min.js", 4, &manifest)),
        ]
        .into_iter()
        .collect();
        manifest
            .packages_segments
            .insert("123456789012".to_string(), snapshot);
        let dirs = OutputDirs::under(dir.path());
        for (path, body) in [
            ("/paste/123456789012/vendor.min.js", "OLD"),
            ("/paste/123456789012/lib.min.js", "LIBMIN"),
            ("/paste/123456789012/vendor.js", "PLAIN"),
        ] {
            let r = combo(&manifest, &dirs, "/paste", path, None);
            assert_eq!(r.status, 200, "{path}: {}", r.body);
            assert_eq!(r.body, body, "{path}");
        }
        // Outside a pre-rule segment the suffix reads as always.
        let r = combo(&manifest, &dirs, "/paste", "/paste/vendor.min.js", None);
        assert_eq!(r.body, "PLAIN");
    }

    #[test]
    fn test_pinned_list_reads_the_min_suffix_off_the_last_entry_only() {
        // A fully pinned list keeps its order; the `.min.js` belongs to the
        // URL, so the last entry is `vendor+v3.0`, not a `.min` module.
        let mut manifest = js_manifest(&[("a", &[]), ("vendor", &[])]);
        let dir = tempfile::tempdir().unwrap();
        write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");
        write_out(&mut manifest, dir.path(), "/js/vendor.h.v3.min.js", "V");
        let dirs = OutputDirs::under(dir.path());
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/1705312800/a+v2.0~vendor+v3.0.min.js",
            None,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(
            r.body.starts_with('A') && r.body.ends_with('V'),
            "{}",
            r.body
        );
        assert_eq!(r.cache_control, "public, max-age=31536000");
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/1705312800/vendor+v3.0~a+v2.0.js",
            None,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(
            r.body.starts_with('V') && r.body.ends_with('A'),
            "{}",
            r.body
        );
    }

    #[test]
    fn test_validate_cdn_prefix_rejects_malformed_authorities() {
        // Authority syntax, not just presence: a non-numeric or
        // out-of-range port, stray colons, an empty host, or a
        // leading/doubled-dot reg-name joins into a URL that fetches
        // nothing. A browser tolerates some of these spellings and
        // silently rewrites the rest — the validator rejects both.
        for good in [
            "https://cdn.example",
            "https://cdn.example:443/assets",
            "http://localhost:8080/cdn",
            "//cdn.example:8080/assets",
            "//cdn.example./x",
            "https://[::1]:8080/x",
            "https://[::ffff:1.2.3.4]/x",
            "https://[1:2:3:4:5:6:7:8]/x",
            "https://192.168.1.10:8080/x",
            "https://cdn.example/assets%20version",
            // Valid UTF-8 IDN escape remains supported; browser host is
            // the corresponding punycoded DNS name.
            "https://cdn.%C3%A9xample/assets",
        ] {
            assert!(validate_cdn_prefix(good).is_ok(), "{good}");
        }
        for bad in [
            // The reported defect: a non-numeric port was accepted.
            "https://cdn.example:bad/assets",
            // Empty port, out-of-range port, stray colon, no host.
            "https://cdn.example:/assets",
            "https://cdn.example:99999/assets",
            "https://cdn.example:80:90/x",
            "https://:8080/x",
            // Userinfo hides the real host — refused outright.
            "https://@/assets",
            "https://user@/assets",
            "https://user:pass@cdn.example/x",
            // Empty reg-name labels fetch nothing.
            "https://.example/x",
            "https://cdn..example/x",
            // Unmatched and ill-formed bracketed literals.
            "http://[::1",
            "http://[::1]:abc/assets",
            "https://[::1]x/x",
            "https://[]/x",
            "https://[nocolon]/x",
            // IPv6 content must be valid — a colon alone is not enough;
            // IPvFuture spellings and zone IDs are not browser hosts.
            "https://[::g]/x",
            "https://[12345::]/x",
            "https://[1:2:3:4:5:6:7:8:9]/x",
            "https://[1:2:3:4:5:6:7::8]/x",
            "https://[1:::2]/x",
            "https://[::ffff:1.2.3.999]/x",
            "https://[1.2.3.4::]/x",
            "https://[vF00.::1]/x",
            "https://[fe80::1%25eth0]/x",
            // Unbracketed IPv6 is indistinguishable from host:port.
            "https://::1/x",
            // Numeric-tail reg-names are IPv4 spellings to a browser:
            // invalid quads fetch nothing, and exotic spellings
            // (`1234`, octal, hex, shorthand) resolve a different host.
            "https://123.456.789/x",
            "https://1.2.3.4.5/x",
            "https://1234/x",
            "https://0x7f.1/x",
            "https://123.45.67/x",
            "https://010.0.0.1/x",
            "https://cdn.0x10/x",
            "https://cdn.010/x",
            // ASCII-encoded host spellings can alter the browser's
            // effective authority; malformed Unicode also fails.
            "https://cdn.%ff/x",
            "https://cdn.%30/x",
            "https://cdn%2eexample.com/x",
            "https://cdn.%2eexample/x",
            "https://cdn.%2d/x",
            "https://cdn.%3a/x",
            "https://cdn.%40/x",
            "https://cdn.%65xample/x",
            "https://cdn.%41xample/x",
            "//cdn.%30/x",
            "https://cdn.%zz/x",
            "https://cdn.%/x",
            "https://cdn.%4/x",
            "https://cdn.%2f/x",
            "https://cdn.%00/x",
            "https://cdn.%25/x",
            "https://cdn.%3f/x",
            "https://cdn.%7f/x",
            "https://cdn.%C3/x",
            "https://cdn.%C0%AF/x",
            "https://cdn.%EF%BC%8Eexample/x",
            "https://cdn.example|x",
            "https://cdn{example}/x",
            "https://cdn!example/x",
            // Protocol-relative carries the same contract.
            "//cdn.example:bad/assets",
            "//:8080/x",
            "//user@cdn.example/x",
            "//123.456.789/x",
        ] {
            assert!(validate_cdn_prefix(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_join_url_has_exactly_one_separator() {
        assert_eq!(join_url("", "/js/a.js"), "/js/a.js");
        assert_eq!(
            join_url("https://cdn.x", "/js/a.js"),
            "https://cdn.x/js/a.js"
        );
        assert_eq!(
            join_url("https://cdn.x/", "/js/a.js"),
            "https://cdn.x/js/a.js",
            "a trailing slash on the prefix must not double the separator"
        );
        assert_eq!(
            join_url("https://cdn.x/assets//", "/js/a.js"),
            "https://cdn.x/assets/js/a.js"
        );
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
            Event::Dump(DumpScope::Css),
            css_jam("screen"),
            Event::Dump(DumpScope::Css),
            css_jam("print"),
            Event::Dump(DumpScope::Css),
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

        // A fully pinned legacy URL is a complete address: the listed order
        // serves as written — canonicalizing the current build's layering
        // would change the bytes the URL names.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG2}/a+v2.0~b+v1.0.min.js"),
            None,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "A2\n;\nB");
        assert!(r.cacheable);

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

        // And the lowercase duplicate of the canonical pin redirects to
        // its single canonical entry.
        let r = combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{SEG2}/a+v2.0-h{ha2}~a+v2.0-h{ha2}.min.js"),
            None,
        );
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(format!("/paste/{SEG2}/a+v2.0-h{ha2}.min.js").as_str())
        );
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

    // verified_read — the compiled-file verification reads only a
    // verified regular file, bounded, before the recorded hash decides.
    mod verified_files {
        use super::*;

        fn md5_of(content: &[u8]) -> String {
            format!("{:x}", md5::compute(content))
        }

        #[cfg(unix)]
        fn mkfifo(path: &Path) {
            use std::os::unix::ffi::OsStrExt;
            let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
            assert_eq!(rc, 0, "mkfifo {} failed", path.display());
        }

        #[test]
        fn serves_bytes_whose_hash_matches_the_record() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.v1.js");
            std::fs::write(&path, b"var a = 1;").unwrap();
            assert_eq!(
                verified_read(&path, &md5_of(b"var a = 1;")).as_deref(),
                Some("var a = 1;")
            );
        }

        #[test]
        fn refuses_modified_bytes_and_missing_files() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.v1.js");
            std::fs::write(&path, b"var a = 2;").unwrap();
            assert!(
                verified_read(&path, &md5_of(b"var a = 1;")).is_none(),
                "replaced bytes must never serve under the recorded hash"
            );
            assert!(
                verified_read(&dir.path().join("gone.js"), &md5_of(b"x")).is_none(),
                "a deleted file must answer the unavailable result"
            );
        }

        #[test]
        #[cfg(unix)]
        fn follows_a_link_to_the_verified_bytes_but_not_a_fifo() {
            let dir = tempfile::tempdir().unwrap();
            let real = dir.path().join("real.js");
            std::fs::write(&real, b"var a = 1;").unwrap();
            let linked = dir.path().join("linked.js");
            std::os::unix::fs::symlink(&real, &linked).unwrap();
            assert_eq!(
                verified_read(&linked, &md5_of(b"var a = 1;")).as_deref(),
                Some("var a = 1;"),
                "a deployment-assembled link to the recorded bytes must serve"
            );

            // A FIFO planted at the path cannot stall the open — the
            // nonblocking handle and the regular-file check answer the
            // unavailable result without waiting on a writer.
            let fifo = dir.path().join("fifo.js");
            mkfifo(&fifo);
            assert!(
                verified_read(&fifo, &md5_of(b"var a = 1;")).is_none(),
                "a special file must answer the unavailable result"
            );
        }

        #[test]
        #[cfg(unix)]
        fn follows_a_directory_alias_to_the_verified_bytes() {
            // A deploy may link the containing directory — `dist/css`
            // pointing at `css-real` serves the same recorded file. The
            // parent resolves before pinning; the hash still answers.
            let dir = tempfile::tempdir().unwrap();
            let real_dir = dir.path().join("css-real");
            std::fs::create_dir_all(&real_dir).unwrap();
            std::fs::write(real_dir.join("theme.min.css"), b".theme{}").unwrap();
            std::os::unix::fs::symlink(&real_dir, dir.path().join("css")).unwrap();
            assert_eq!(
                verified_read(&dir.path().join("css/theme.min.css"), &md5_of(b".theme{}"),)
                    .as_deref(),
                Some(".theme{}"),
                "a directory alias to the recorded bytes must serve"
            );
        }

        #[test]
        fn refuses_an_oversized_replacement_before_hashing() {
            // A sparse file past the read cap answers None — the bound
            // ran before the hash could be asked about bytes that were
            // never the recorded asset.
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("huge.js");
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(64 * 1024 * 1024 + 8).unwrap();
            assert!(
                verified_read(&path, &md5_of(b"var a = 1;")).is_none(),
                "an oversized stand-in must answer the unavailable result"
            );
        }

        #[test]
        fn refuses_an_unverifiable_record() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.v1.js");
            std::fs::write(&path, b"var a = 1;").unwrap();
            assert!(verified_read(&path, "not-a-hash").is_none());
            assert!(verified_read(&path, "").is_none());
        }
    }

    // Conditional combos — the manifest-derived group stamp answers the
    // request's validator before a file is ever opened.
    mod conditional {
        use super::*;

        /// STAMP's epoch in header form — the group's Last-Modified.
        const AT_STAMP: &str = "Mon, 15 Jan 2024 10:00:00 GMT";
        const AFTER_STAMP: &str = "Mon, 15 Jan 2024 10:00:01 GMT";
        const BEFORE_STAMP: &str = "Mon, 15 Jan 2024 09:59:59 GMT";

        fn ask(path: &str, ims: Option<&str>) -> ComboResponse {
            let (manifest, dir) = combo_manifest();
            combo_conditional(
                &manifest,
                &OutputDirs::under(dir.path()),
                "/paste",
                path,
                None,
                ims,
            )
        }

        #[test]
        fn a_matching_or_later_validator_answers_304() {
            for ims in [AT_STAMP, AFTER_STAMP] {
                let r = ask(&format!("/paste/{SEG}/a+v1.0~b+v1.0.js"), Some(ims));
                assert_eq!(r.status, 304, "validator {ims} did not match");
                assert!(r.body.is_empty(), "a 304 carries no body");
                assert_eq!(r.last_modified.as_deref(), Some(AT_STAMP));
                assert_eq!(r.cache_control, "public, max-age=31536000");
            }
        }

        #[test]
        fn an_earlier_absent_or_invalid_validator_serves_the_group() {
            for ims in [Some(BEFORE_STAMP), Some("not a date"), None] {
                let r = ask(&format!("/paste/{SEG}/a+v1.0~b+v1.0.js"), ims);
                assert_eq!(r.status, 200, "validator {ims:?} answered 304");
                assert_eq!(r.body, "A\n;\nB");
                assert_eq!(r.last_modified.as_deref(), Some(AT_STAMP));
            }
        }

        #[test]
        fn canonicalization_answers_before_the_validator() {
            // A non-canonical URL redirects first — the validator only
            // shortens a 200, it never suppresses the canonicalization
            // a saved page still needs.
            let r = ask(&format!("/paste/{SEG}/b+v1.0~a.min.js"), Some(AFTER_STAMP));
            assert_eq!(r.status, 302);
            assert_eq!(
                r.location.as_deref(),
                Some(format!("/paste/{SEG}/a+v1.0~b+v1.0.min.js").as_str())
            );
            assert_eq!(r.last_modified, None);
        }

        #[test]
        fn mutable_forms_never_answer_304() {
            let (manifest, dir) = combo_manifest();
            let dirs = OutputDirs::under(dir.path());
            // `require` and the unversioned list are mutable
            // representations — they re-resolve at the current manifest,
            // and a stamp that held cannot prove the bytes did not
            // change. The stamp still travels as Last-Modified, but every
            // validator answer is a resolved 200.
            for path in [
                "/paste/require/a~b.js".to_string(),
                "/paste/a~b.js".to_string(),
            ] {
                for ims in [Some(AFTER_STAMP), Some(BEFORE_STAMP), None] {
                    let r = combo_conditional(&manifest, &dirs, "/paste", &path, None, ims);
                    assert_eq!(r.status, 200, "{path} validator {ims:?}");
                    assert_eq!(r.body, "A\n;\nB", "{path} validator {ims:?}");
                    assert_eq!(r.last_modified.as_deref(), Some(AT_STAMP));
                    assert_eq!(r.cache_control, "no-cache");
                }
            }
        }

        #[test]
        fn a_304_never_covers_an_unavailable_group() {
            let (manifest, dir) = combo_manifest();
            let dirs = OutputDirs::under(dir.path());
            // Every member filtered away — the unconditional answer is a
            // 404, so the validator never shortens it to a 304.
            let r = combo_conditional(
                &manifest,
                &dirs,
                "/paste",
                &format!("/paste/{SEG}/a+v1.0~b+v1.0.js"),
                Some("filter=a~b"),
                Some(AFTER_STAMP),
            );
            assert_eq!(r.status, 404, "all-filtered + validator: {}", r.status);
            assert_eq!(r.last_modified, None);

            // A member file gone since the build — same rule: the
            // representation cannot be served, so it cannot be confirmed.
            let present = combo_conditional(
                &manifest,
                &dirs,
                "/paste",
                &format!("/paste/{SEG}/a+v1.0~b+v1.0.js"),
                None,
                Some(AFTER_STAMP),
            );
            assert_eq!(present.status, 304);
            std::fs::remove_file(dir.path().join("js/b.h.v1.min.js")).unwrap();
            let r = combo_conditional(
                &manifest,
                &dirs,
                "/paste",
                &format!("/paste/{SEG}/a+v1.0~b+v1.0.js"),
                None,
                Some(AFTER_STAMP),
            );
            assert_eq!(r.status, 404, "missing member + validator: {}", r.status);
            assert_eq!(r.cache_control, "no-cache");
        }

        #[test]
        fn every_http_date_form_validates_the_same_epoch() {
            // RFC 9110 §5.6.7: recipients accept IMF-fixdate, RFC 850,
            // and asctime — the same instant in each spelling is the
            // same validator.
            let plain = ask(&format!("/paste/{SEG}/a+v1.0~b+v1.0.js"), None);
            for ims in [
                "Mon, 15 Jan 2024 10:00:00 GMT",
                "Monday, 15-Jan-24 10:00:00 GMT",
                "Mon Jan 15 10:00:00 2024",
            ] {
                let r = ask(&format!("/paste/{SEG}/a+v1.0~b+v1.0.js"), Some(ims));
                assert_eq!(r.status, 304, "validator {ims:?} did not match");
                assert_eq!(
                    r.content_type, plain.content_type,
                    "a 304 must carry the 200's content type — a cache may \
                     update stored headers from it"
                );
            }
            assert_eq!(validator_epoch(Some("not a date")), None);
            assert_eq!(validator_epoch(Some("")), None);
            assert_eq!(validator_epoch(None), None);
        }

        #[test]
        fn historical_loader_304_never_covers_a_missing_file() {
            let (mut manifest, dir) = versioned_manifest();
            let dirs = OutputDirs::under(dir.path());
            manifest.packages_segments.insert(
                "123456789012".to_string(),
                [(
                    "a".to_string(),
                    crate::manifest::SegmentPin {
                        key: "js/a.js".to_string(),
                        version: 1,
                        hash: manifest
                            .resolve_version("a", "js", Some(1))
                            .unwrap()
                            .hash
                            .to_string(),
                    },
                )]
                .into_iter()
                .collect(),
            );
            let r = combo_conditional(
                &manifest,
                &dirs,
                "/paste",
                "/paste/123456789012/a.js",
                None,
                Some(AFTER_STAMP),
            );
            assert_eq!(r.status, 304);
            // The pinned file deleted — the conditional answer must fall
            // to the same 404 the unconditional one gives.
            let path = manifest
                .resolve_version("a", "js", Some(1))
                .unwrap()
                .output
                .trim_start_matches('/')
                .to_string();
            std::fs::remove_file(dir.path().join(&path)).unwrap();
            let r = combo_conditional(
                &manifest,
                &dirs,
                "/paste",
                "/paste/123456789012/a.js",
                None,
                Some(AFTER_STAMP),
            );
            assert_eq!(r.status, 404);
        }

        #[test]
        fn historical_pins_compare_against_their_own_stamps() {
            let (manifest, dir) = versioned_manifest();
            let dirs = OutputDirs::under(dir.path());
            // a@v1 is a retained history entry stamped STAMP — a request
            // validated against the CURRENT version's stamp (STAMP2)
            // still 304s, because the group's stamp is the history
            // entry's own.
            let r = combo_conditional(
                &manifest,
                &dirs,
                "/paste",
                &format!("/paste/{SEG}/a+v1.0.min.js"),
                None,
                Some(AT_STAMP),
            );
            assert_eq!(r.status, 304);
            assert!(r.body.is_empty());
            // And between the two stamps: after the history version's
            // stamp but before the current's — the same history-pinned
            // group is still a match.
            let r = combo_conditional(
                &manifest,
                &dirs,
                "/paste",
                &format!("/paste/{SEG}/a+v1.0.min.js"),
                None,
                Some("Mon, 15 Jan 2024 12:00:00 GMT"),
            );
            assert_eq!(r.status, 304);
        }
    }

    // Conditional markup — a jam's `condition` wraps the emitted tags in
    // `<!--[if …]>`…`<![endif]-->` and, for js, keeps the group out of the
    // loaded set exactly as the original did.
    mod conditional_markup {
        use super::*;

        fn cond_jam(css: &[&str], js: &[&str], condition: Option<&str>) -> Event {
            Event::Jam(Jam {
                css: css.iter().map(|s| s.to_string()).collect(),
                js: js.iter().map(|s| s.to_string()).collect(),
                condition: condition.map(|c| c.to_string()),
                ..Jam::default()
            })
        }

        #[test]
        fn comment_closing_content_is_linked_not_inlined() {
            // `a-->0` would end the `<!--[if …]>` wrapper early and leak the
            // rest of the script into the page as text.
            let mut manifest = js_manifest(&[("a", &[])]);
            let dir = tempfile::tempdir().unwrap();
            write_out(
                &mut manifest,
                dir.path(),
                "/js/paste.h.v1.min.js",
                "PASTE_RT",
            );
            write_out(
                &mut manifest,
                dir.path(),
                "/js/a.h.v2.min.js",
                "var a=3;while(a-->0){}",
            );
            let events = vec![
                Event::Jam(Jam {
                    js: vec!["a".into()],
                    sync: true,
                    condition: Some("lt IE 9".into()),
                    ..Jam::default()
                }),
                Event::Dump(DumpScope::Js),
            ];
            let rendered =
                render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
            let html = &rendered[0].html;
            assert!(!html.contains("a-->0"), "{html}");
            assert_eq!(
                html.matches("-->").count(),
                1,
                "only the wrapper closes: {html}"
            );
        }

        #[test]
        fn comment_closers_still_inline_outside_a_conditional_group() {
            // `-->` is ordinary script data in an unconditional <script>.
            let mut manifest = js_manifest(&[("a", &[])]);
            let dir = tempfile::tempdir().unwrap();
            write_out(
                &mut manifest,
                dir.path(),
                "/js/paste.h.v1.min.js",
                "PASTE_RT",
            );
            write_out(
                &mut manifest,
                dir.path(),
                "/js/a.h.v2.min.js",
                "var a=3;while(a-->0){}",
            );
            let events = vec![
                Event::Jam(Jam {
                    js: vec!["a".into()],
                    sync: true,
                    ..Jam::default()
                }),
                Event::Dump(DumpScope::Js),
            ];
            let rendered =
                render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
            assert!(rendered[0].html.contains("a-->0"), "{}", rendered[0].html);
        }

        #[test]
        fn conditional_tags_wrap_and_js_groups_stay_unloaded() {
            let mut manifest = js_manifest(&[("a", &[])]);
            let dir = tempfile::tempdir().unwrap();
            write_out(
                &mut manifest,
                dir.path(),
                "/js/paste.h.v1.min.js",
                "PASTE_RT",
            );
            write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");

            // A conditional js group emits its tag wrapped — and does NOT
            // mark a loaded. The later unconditional jam emits it again.
            let events = vec![
                cond_jam(&[], &["a"], Some("lt IE 9")),
                Event::Dump(DumpScope::Js),
                jam(&[], &["a"]),
                Event::Dump(DumpScope::Js),
            ];
            let mut opts = opts();
            opts.inline_threshold = 0;
            let rendered =
                render_all(&manifest, &OutputDirs::under(dir.path()), &opts, &events).unwrap();
            assert_eq!(rendered.len(), 2);

            let first = &rendered[0].html;
            assert!(first.contains("<!--[if lt IE 9]>\n"), "{first}");
            assert!(first.contains("\n<![endif]-->"), "{first}");
            assert!(
                first.contains("<script type=\"text/javascript\">(function(d,r)"),
                "{first}"
            );
            // A conditional group cannot carry the unconditional runtime
            // bootstrap — paste.config never lands inside the wrapper.
            assert!(!first.contains("paste.config"), "{first}");

            // The unconditional dump carries the bootstrap AND re-serves a.
            let second = &rendered[1].html;
            assert!(second.contains("paste.config"), "{second}");
            assert!(!second.contains("<!--[if"), "{second}");
            assert!(
                second.contains("(function(d,r){d('a').isLoading=!0;r('"),
                "{second}"
            );
        }

        #[test]
        fn a_conditional_stylesheet_still_marks_loaded() {
            let mut manifest = Manifest::new();
            manifest.hash_urls = false;
            manifest.assets.insert(
                "css/site.css".to_string(),
                asset("/css/site.h.v1.min.css", 1, STAMP, &[]),
            );
            let dir = tempfile::tempdir().unwrap();
            write_out(
                &mut manifest,
                dir.path(),
                "/css/site.h.v1.min.css",
                "BODY{}",
            );

            // Asymmetric with js, as the original was: a conditional css
            // group DID mark loaded, so the later unconditional jam finds
            // nothing pending.
            let events = vec![
                cond_jam(&["site"], &[], Some("lt IE 9")),
                Event::Dump(DumpScope::Css),
                jam(&["site"], &[]),
                Event::Dump(DumpScope::Css),
            ];
            let mut opts = opts();
            opts.inline_threshold = 0;
            let rendered =
                render_all(&manifest, &OutputDirs::under(dir.path()), &opts, &events).unwrap();
            assert_eq!(rendered.len(), 2);
            let first = &rendered[0].html;
            assert!(first.contains("<!--[if lt IE 9]>"), "{first}");
            assert!(first.contains("<link rel=\"stylesheet\""), "{first}");
            assert_eq!(rendered[1].html, "", "{:?}", rendered[1].html);
        }

        #[test]
        fn a_mixed_dump_bootstraps_from_the_unconditional_group() {
            let mut manifest = js_manifest(&[("a", &[]), ("b", &[])]);
            let dir = tempfile::tempdir().unwrap();
            write_out(
                &mut manifest,
                dir.path(),
                "/js/paste.h.v1.min.js",
                "PASTE_RT",
            );
            write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");
            write_out(&mut manifest, dir.path(), "/js/b.h.v3.min.js", "B");

            // One dump with a conditional jam then an unconditional one:
            // the bootstrap leads once (for the unconditional group) and
            // both groups emit — the conditional wrapped, the rest not.
            let events = vec![
                cond_jam(&[], &["a"], Some("IE 8")),
                jam(&[], &["b"]),
                Event::Dump(DumpScope::Js),
            ];
            let mut opts = opts();
            opts.inline_threshold = 0;
            let rendered =
                render_all(&manifest, &OutputDirs::under(dir.path()), &opts, &events).unwrap();
            let html = &rendered[0].html;
            assert!(html.contains("paste.config"), "{html}");
            assert_eq!(html.matches("<!--[if IE 8]>").count(), 1, "{html}");
            assert!(
                html.contains("(function(d,r){d('a').isLoading=!0;r('"),
                "{html}"
            );
            assert!(
                html.contains("(function(d,r){d('b').isLoading=!0;r('"),
                "{html}"
            );
        }

        #[test]
        fn an_empty_condition_is_no_condition_at_all() {
            let mut manifest = js_manifest(&[("a", &[])]);
            let dir = tempfile::tempdir().unwrap();
            write_out(
                &mut manifest,
                dir.path(),
                "/js/paste.h.v1.min.js",
                "PASTE_RT",
            );
            write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");

            // The original's attribute falsy check: `Some("")` renders and
            // loads exactly like an unconditional jam — bootstrap, no
            // wrapper, and a marks loaded (a second jam emits nothing).
            let events = vec![
                cond_jam(&[], &["a"], Some("")),
                jam(&[], &["a"]),
                Event::Dump(DumpScope::Js),
            ];
            let rendered =
                render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).unwrap();
            let html = &rendered[0].html;
            assert!(html.contains("paste.config"), "{html}");
            assert!(!html.contains("<!--[if"), "{html}");
        }

        #[test]
        fn markup_breaking_conditions_are_refused() {
            let mut manifest = js_manifest(&[("a", &[])]);
            let dir = tempfile::tempdir().unwrap();
            write_out(
                &mut manifest,
                dir.path(),
                "/js/paste.h.v1.min.js",
                "PASTE_RT",
            );
            for bad in [
                "IE]--><script>alert(1)</script><!--",
                "lt IE 9 -->",
                "<if",
                "gt>IE",
                "lt\u{1f}IE",
            ] {
                let events = vec![cond_jam(&[], &["a"], Some(bad)), Event::Dump(DumpScope::Js)];
                assert!(
                    render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events)
                        .is_err(),
                    "condition {bad:?} was not refused"
                );
            }
            // Conditions the original emitted still pass.
            for good in ["lt IE 9", "gte IE 8", "IE 6", "!IE", "(lt IE 9)&(gt IE 6)"] {
                let events = vec![
                    cond_jam(&[], &["a"], Some(good)),
                    Event::Dump(DumpScope::Js),
                ];
                assert!(
                    render_all(&manifest, &OutputDirs::under(dir.path()), &opts(), &events).is_ok(),
                    "condition {good:?} was refused"
                );
            }
        }

        #[test]
        fn per_module_tags_each_carry_the_wrapper() {
            let mut manifest = js_manifest(&[("a", &[]), ("b", &["a"])]);
            let dir = tempfile::tempdir().unwrap();
            write_out(
                &mut manifest,
                dir.path(),
                "/js/paste.h.v1.min.js",
                "PASTE_RT",
            );
            write_out(&mut manifest, dir.path(), "/js/a.h.v2.min.js", "A");
            write_out(&mut manifest, dir.path(), "/js/b.h.v3.min.js", "B");
            let mut opts = opts();
            opts.per_module = true;
            opts.inline_threshold = 0;

            let events = vec![
                cond_jam(&[], &["b"], Some("IE 9")),
                Event::Dump(DumpScope::Js),
            ];
            let rendered =
                render_all(&manifest, &OutputDirs::under(dir.path()), &opts, &events).unwrap();
            let html = &rendered[0].html;
            // The ordered loader block is one tag → one wrapper.
            assert_eq!(html.matches("<!--[if IE 9]>").count(), 1, "{html}");
            assert!(html.contains("\n<![endif]-->"), "{html}");
        }
    }
}
