//! Build-time rendering of `paste:` template directives.
//!
//! A plain HTML site needs no request-time server: `paste:jam` and
//! `paste:dump` comments in `templates.source` are replaced by the tags
//! `jam::render_all` emits, and every external URL those tags reference —
//! the combo responses plus the per-module files the paste loader fetches —
//! is materialized under `output.base`. Runs at the end of every successful
//! build when `config.templates` is set.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::config::Config;
use crate::jam::{self, AssetType, DumpScope, Event, Jam, OutputDirs, RenderOptions};
use crate::manifest::Manifest;

/// Render every `*.html` file under `templates.source` into
/// `templates.output`, then write the combo responses and loader files the
/// emitted tags reference. Other files are left alone — templates.output
/// receives rendered HTML only, so pair it with `output.base` (or copy
/// assets yourself) when the site serves both.
pub fn render_templates(config: &Config, manifest: &Manifest) -> Result<()> {
    let Some(templates) = &config.templates else {
        return Ok(());
    };

    // Rendering into or under the source tree would overwrite templates or
    // re-read its own output on the next build.
    let source_resolved = canonical_or_pending(&templates.source)?;
    let output_resolved = canonical_or_pending(&templates.output)?;
    if output_resolved.starts_with(&source_resolved) {
        bail!(
            "templates.output {} must not equal or nest inside templates.source {}",
            templates.output.display(),
            templates.source.display()
        );
    }

    // Reads of compiled files go through the layout's one resolver — flat
    // output under output.base, beside-source output under base_dir. The
    // combo and loader files the emitted tags reference always land under
    // output.base: it is the hosted directory in either layout.
    let dirs = config.output_dirs();
    let opts = RenderOptions {
        cdn_base: if config.cdn.enabled {
            config.cdn.url.clone()
        } else {
            String::new()
        },
        base_path: config.jam.base_path.clone(),
        inline_threshold: config.jam.inline_threshold,
        // Static hosting materializes each URL path component as a name;
        // hash-qualified lists continue over components under the usual
        // 255-byte filesystem limit.
        url_component_limit: Some(255),
    };
    // The base path is written into HTML and split off URL strings; an
    // invalid one fails the build even for a site with no directives.
    jam::validate_options(&opts)?;

    // URL path -> the template that first referenced it, so a combo shared
    // by several pages is resolved and written once.
    let mut referenced: BTreeMap<String, PathBuf> = BTreeMap::new();

    for entry in WalkDir::new(&templates.source).sort_by_file_name() {
        let entry = entry.with_context(|| format!("cannot walk {}", templates.source.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let (rendered, urls) = render_file(path, manifest, &dirs, &opts)?;
        for url in urls {
            referenced.entry(url).or_insert_with(|| path.to_path_buf());
        }
        let rel = path.strip_prefix(&templates.source)?;
        let dest = templates.output.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, rendered).with_context(|| format!("cannot write {}", dest.display()))?;
    }

    // Static combo files: a response the combo cannot produce would ship a
    // broken link, so anything but 200 fails the build.
    for (url, template) in &referenced {
        let response = jam::combo(manifest, &dirs, &opts.base_path, url, None);
        if response.status != 200 {
            bail!(
                "{}: referenced combo {} returned status {}",
                template.display(),
                url,
                response.status
            );
        }
        write_output(&config.output.base, url, response.body.as_bytes())?;
    }

    // Loader files: paste.js fetches {packagesUrl}{module}.js itself, so
    // every module with a JS asset needs its compiled output under
    // {base_path}/{packages segment}/.
    let version = jam::packages_segment(manifest);
    let mut modules: Vec<&String> = manifest.modules.keys().collect();
    modules.sort();
    for module in modules {
        let Some(key) = manifest.asset_key_for_module(module, "js") else {
            continue;
        };
        let entry = &manifest.assets[&key];
        let source = dirs
            .compiled_file(&entry.output, AssetType::Js)
            .ok_or_else(|| anyhow::anyhow!("invalid compiled output for module {}", module))?;
        let body = fs::read(&source).with_context(|| {
            format!(
                "cannot read compiled output for module {} ({})",
                module,
                source.display()
            )
        })?;
        let rel = format!("{}/{}/{}.js", opts.base_path, version, module);
        write_output(&config.output.base, &rel, &body)?;
    }

    Ok(())
}

/// Canonical form of `path` even when its tail does not exist yet: the
/// nearest existing ancestor is canonicalized and the missing components
/// re-attached, so a not-yet-created output directory still compares
/// correctly against the source tree.
fn canonical_or_pending(path: &Path) -> Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut pending = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => pending.push(name.to_os_string()),
            None => break,
        }
        if !existing.pop() {
            break;
        }
    }
    let mut resolved = existing
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", existing.display()))?;
    for part in pending.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

/// Write `body` to `{base}{url_path}` — the URL's leading `/` is dropped so
/// the path nests under base, and intermediate directories are created.
/// The path must be a plain relative path: `..`, `.`, an empty segment, a
/// backslash or a `:` (a Windows drive marker even when produced on Unix)
/// is refused rather than written outside `base`.
fn write_output(base: &Path, url_path: &str, body: &[u8]) -> Result<()> {
    let trimmed = url_path.trim_start_matches('/');
    if trimmed.is_empty()
        || trimmed
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || trimmed.contains(['\\', ':'])
        || Path::new(trimmed)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("output path {:?} is not a safe relative path", url_path);
    }
    let dest = base.join(trimmed);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dest, body).with_context(|| format!("cannot write {}", dest.display()))
}

/// One `paste:` directive in a template: the comment's byte span, its line,
/// and the event it declares.
#[derive(Debug)]
struct Directive {
    start: usize,
    end: usize,
    line: usize,
    event: Event,
}

/// Render one template: jam directives are replaced by nothing and each
/// dump by its `Rendered::html`; every other byte is copied unchanged.
/// Returns the rendered content and the URL paths its external tags
/// reference.
fn render_file(
    path: &Path,
    manifest: &Manifest,
    dirs: &OutputDirs,
    opts: &RenderOptions,
) -> Result<(String, Vec<String>)> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("cannot read template {}", path.display()))?;
    let directives = parse_directives(&content, path)?;
    if directives.is_empty() {
        return Ok((content, Vec::new()));
    }
    let last = directives.last().unwrap();
    if matches!(last.event, Event::Jam(_)) {
        bail!(
            "{}:{}: jam after the last dump is never emitted",
            path.display(),
            last.line
        );
    }

    let mut events: Vec<Event> = directives.iter().map(|d| d.event.clone()).collect();
    // EOF seals the page: finish validates that every jammed module was
    // dumped, so a jam queued after its type's last dump fails the build
    // instead of silently dropping the modules.
    events.push(Event::Finish);
    let rendered = jam::render_all(manifest, dirs, opts, &events)
        .with_context(|| format!("{}", path.display()))?;

    let mut out = String::with_capacity(content.len());
    let mut cursor = 0;
    let mut dumps = rendered.iter();
    for directive in &directives {
        out.push_str(&content[cursor..directive.start]);
        if matches!(directive.event, Event::Dump(_)) {
            out.push_str(&dumps.next().unwrap().html);
        }
        cursor = directive.end;
    }
    out.push_str(&content[cursor..]);

    let urls = rendered
        .iter()
        .flat_map(|r| r.urls.iter().cloned())
        .collect();
    Ok((out, urls))
}

/// Find every `<!-- paste:… -->` comment in `content`, in document order.
/// Comments that do not start with `paste:` are ordinary markup and pass
/// through untouched.
fn parse_directives(content: &str, path: &Path) -> Result<Vec<Directive>> {
    let mut directives = Vec::new();
    let mut offset = 0;
    while let Some(found) = content[offset..].find("<!--") {
        let start = offset + found;
        let Some(close) = content[start + 4..].find("-->") else {
            break;
        };
        let end = start + 4 + close + 3;
        if let Some(rest) = content[start + 4..end - 3].trim().strip_prefix("paste:") {
            let line = content[..start].matches('\n').count() + 1;
            let event = parse_event(rest, path, line)?;
            directives.push(Directive {
                start,
                end,
                line,
                event,
            });
        }
        offset = end;
    }
    Ok(directives)
}

/// Parse the body after `paste:` — `jam` with its attributes, or `dump`
/// with an asset type — bare `dump` emits both queues.
fn parse_event(rest: &str, path: &Path, line: usize) -> Result<Event> {
    let rest = rest.trim();
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim_start()),
        None => (rest, ""),
    };
    match name {
        "jam" => parse_jam(args, path, line).map(Event::Jam),
        "dump" => match args.trim() {
            "" => Ok(Event::Dump(DumpScope::All)),
            "css" => Ok(Event::Dump(DumpScope::Css)),
            "js" => Ok(Event::Dump(DumpScope::Js)),
            _ => bail!(
                "{}:{}: paste:dump takes css, js, or nothing",
                path.display(),
                line
            ),
        },
        _ => bail!(
            "{}:{}: unknown paste directive {:?}",
            path.display(),
            line,
            name
        ),
    }
}

/// Parse jam attributes: `css`/`js` take a double-quoted list (commas
/// and/or whitespace), `inline` and `sync` are bare flags, `media` takes a
/// double-quoted value.
fn parse_jam(args: &str, path: &Path, line: usize) -> Result<Jam> {
    let mut jam = Jam::default();
    let mut rest = args;
    while !rest.is_empty() {
        let name_end = rest
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(rest.len());
        let name = &rest[..name_end];
        rest = rest[name_end..].trim_start();

        let value = if let Some(after_eq) = rest.strip_prefix('=') {
            let after_eq = after_eq.trim_start();
            let Some(unquoted) = after_eq.strip_prefix('"') else {
                bail!(
                    "{}:{}: attribute {:?} requires a double-quoted value",
                    path.display(),
                    line,
                    name
                );
            };
            let Some(close) = unquoted.find('"') else {
                bail!(
                    "{}:{}: unterminated value for attribute {:?}",
                    path.display(),
                    line,
                    name
                );
            };
            rest = unquoted[close + 1..].trim_start();
            Some(&unquoted[..close])
        } else {
            None
        };

        match (name, value) {
            ("inline", None) => jam.inline = true,
            ("sync", None) => jam.sync = true,
            ("css", Some(v)) => jam.css = split_list(v),
            ("js", Some(v)) => jam.js = split_list(v),
            ("media", Some(v)) => jam.media = Some(v.to_string()),
            ("css" | "js" | "media", None) => bail!(
                "{}:{}: attribute {:?} requires a double-quoted value",
                path.display(),
                line,
                name
            ),
            ("inline" | "sync", Some(_)) => bail!(
                "{}:{}: attribute {:?} takes no value",
                path.display(),
                line,
                name
            ),
            _ => bail!(
                "{}:{}: unknown attribute {:?} on paste:jam",
                path.display(),
                line,
                name
            ),
        }
    }
    Ok(jam)
}

/// A jam attribute value: names separated by commas and/or whitespace,
/// empties dropped.
fn split_list(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TemplatesConfig;
    use crate::manifest::AssetEntry;
    use tempfile::tempdir;

    /// The stamp every fixture entry carries — its epoch seconds are the
    /// URL segment rendered URLs take (1705312800).
    const STAMP: &str = "2024-01-15T10:00:00+00:00";
    const STAMP_EPOCH: &str = "1705312800";

    /// The recorded hash is a placeholder — an MD5 of the output path
    /// string, not of any bytes — which suffices only because every fixture
    /// here opts out of hash-qualified URLs (`hash_urls = false`). Fixtures
    /// exercising the qualified form must record the MD5 of the bytes they
    /// write instead; see `segmented_manifest`.
    fn asset(output: &str, version: u32, deps: &[&str]) -> AssetEntry {
        AssetEntry {
            source: "src/x".to_string(),
            output: output.to_string(),
            hash: format!("{:x}", md5::compute(output)),
            version,
            timestamp: STAMP.to_string(),
            dependencies: deps.iter().map(|d| d.to_string()).collect(),
            history: Vec::new(),
            retired: false,
        }
    }

    /// Manifest with a paste runtime, a js module and a css module; every
    /// entry carries STAMP, so rendered URLs carry its epoch seconds.
    fn test_manifest() -> Manifest {
        let mut m = Manifest::new();
        m.hash_urls = false;
        m.version = "42".to_string();
        m.modules
            .insert("paste".to_string(), "js/paste.js".to_string());
        m.modules.insert("app".to_string(), "js/app.js".to_string());
        m.modules
            .insert("site".to_string(), "css/site.css".to_string());
        m.assets.insert(
            "js/paste.js".to_string(),
            asset("/js/paste.h.v1.min.js", 1, &[]),
        );
        m.assets.insert(
            "js/app.js".to_string(),
            asset("/js/app.h.v2.min.js", 2, &[]),
        );
        m.assets.insert(
            "css/site.css".to_string(),
            asset("/css/site.h.v1.min.css", 1, &[]),
        );
        m
    }

    /// A config whose templates render into the same public dir as output.
    /// Threshold 0 keeps jams external so the rendered pages exercise the
    /// combo-file writes.
    fn test_config(dir: &Path) -> Config {
        let mut config = Config::default_for(dir);
        config.jam.inline_threshold = 0;
        config.templates = Some(TemplatesConfig {
            source: dir.join("templates"),
            output: config.output.base.clone(),
        });
        config
    }

    fn write_compiled(config: &Config, output: &str, content: &str) {
        let trimmed = output.trim_start_matches('/');
        let path = if let Some(rest) = trimmed.strip_prefix("js/") {
            config.output.js.join(rest)
        } else if let Some(rest) = trimmed.strip_prefix("css/") {
            config.output.css.join(rest)
        } else {
            config.output.base.join(trimmed)
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn write_template(config: &Config, rel: &str, content: &str) {
        let path = config.templates.as_ref().unwrap().source.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn test_directive_parsing() {
        let path = Path::new("t.html");

        let d = parse_directives(
            "<!-- paste:jam css=\"a, b\" js=\"c d\" inline sync media=\"print\" -->",
            path,
        )
        .unwrap();
        assert_eq!(d.len(), 1);
        match &d[0].event {
            Event::Jam(j) => {
                assert_eq!(j.css, vec!["a".to_string(), "b".to_string()]);
                assert_eq!(j.js, vec!["c".to_string(), "d".to_string()]);
                assert!(j.inline);
                assert!(j.sync);
                assert_eq!(j.media.as_deref(), Some("print"));
            }
            _ => panic!("expected a jam event"),
        }

        // Whitespace is free around the name and attributes.
        let d = parse_directives("<!--   paste:dump   js  -->", path).unwrap();
        assert!(matches!(d[0].event, Event::Dump(DumpScope::Js)));

        // A bare dump emits both queues.
        let d = parse_directives("<!-- paste:dump -->", path).unwrap();
        assert!(matches!(d[0].event, Event::Dump(DumpScope::All)));

        // Ordinary comments are not directives.
        assert!(parse_directives("<!-- hello --><!-- paste2:x -->", path)
            .unwrap()
            .is_empty());
        assert!(parse_directives("<!-- paste :jam -->", path)
            .unwrap()
            .is_empty());

        // Errors name the file and the directive's line.
        for (src, needle) in [
            ("<!-- paste:bogus -->", "unknown paste directive"),
            ("<!-- paste:jam bogus -->", "unknown attribute"),
            ("<!-- paste:jam css=a -->", "double-quoted value"),
            ("<!-- paste:jam css -->", "double-quoted value"),
            ("<!-- paste:jam inline=\"yes\" -->", "takes no value"),
            ("<!-- paste:jam css=\"a -->", "unterminated"),
            ("<!-- paste:dump css js -->", "takes css, js, or nothing"),
            ("<!-- paste:dump style -->", "takes css, js, or nothing"),
            ("<!-- paste:dump all -->", "takes css, js, or nothing"),
        ] {
            let err = parse_directives(&format!("line1\nline2\n{src}"), path).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("t.html:3:"), "{src} → {msg}");
            assert!(msg.contains(needle), "{src} → {msg}");
        }
    }

    #[test]
    fn test_render_templates_end_to_end() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let manifest = test_manifest();
        write_compiled(&config, "/js/paste.h.v1.min.js", "PASTE_RT");
        write_compiled(&config, "/js/app.h.v2.min.js", "APP");
        write_compiled(&config, "/css/site.h.v1.min.css", ".site{}");

        let template = concat!(
            "<!doctype html>\n<html>\n<head>\n",
            "<!-- paste:jam css=\"site\" -->\n<!-- paste:dump css -->\n",
            "</head>\n<body>\n",
            "<!-- paste:jam js=\"app\" -->\n<!-- paste:dump js -->\n",
            "</body>\n</html>\n"
        );
        write_template(&config, "index.html", template);

        render_templates(&config, &manifest).unwrap();

        let out = fs::read_to_string(config.templates.as_ref().unwrap().output.join("index.html"))
            .unwrap();
        assert!(
            out.starts_with("<!doctype html>\n<html>\n<head>\n"),
            "{out}"
        );
        assert!(out.ends_with("</body>\n</html>\n"), "{out}");
        // The css dump sits where the directive was, inside head.
        let link =
            format!("<link rel=\"stylesheet\" href=\"/paste/{STAMP_EPOCH}/site+v1.0.min.css\"/>");
        assert!(out.contains(&format!("<head>\n\n{link}\n</head>")), "{out}");
        // The js dump ends the page: inline runtime with the config, then
        // the external module tag.
        assert!(
            out.contains(&format!(
                "PASTE_RT{}paste.config({{packagesUrl:'/paste/{}/'}});",
                crate::graph::JS_CONCAT_SEPARATOR,
                jam::packages_segment(&manifest)
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "d('app').isLoading=!0;r('/paste/{STAMP_EPOCH}/app+v2.0.min.js')"
            )),
            "{out}"
        );
        // No directive survives.
        assert!(!out.contains("paste:jam"), "{out}");
        assert!(!out.contains("paste:dump"), "{out}");
    }

    #[test]
    fn test_render_templates_bare_dump_emits_both() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let manifest = test_manifest();
        write_compiled(&config, "/js/paste.h.v1.min.js", "PASTE_RT");
        write_compiled(&config, "/js/app.h.v2.min.js", "APP");
        write_compiled(&config, "/css/site.h.v1.min.css", ".site{}");

        // One bare dump stands in for a css dump followed by a js dump.
        write_template(
            &config,
            "index.html",
            concat!(
                "<!doctype html>\n<html>\n<head>\n",
                "<!-- paste:jam css=\"site\" js=\"app\" -->\n<!-- paste:dump -->\n",
                "</head>\n<body>\n</body>\n</html>\n"
            ),
        );
        render_templates(&config, &manifest).unwrap();

        let out = fs::read_to_string(config.templates.as_ref().unwrap().output.join("index.html"))
            .unwrap();
        let link =
            format!("<link rel=\"stylesheet\" href=\"/paste/{STAMP_EPOCH}/site+v1.0.min.css\"/>");
        assert!(out.contains(&link), "{out}");
        assert!(
            out.contains(&format!(
                "d('app').isLoading=!0;r('/paste/{STAMP_EPOCH}/app+v2.0.min.js')"
            )),
            "{out}"
        );
        assert!(
            out.find("<link").unwrap() < out.find("<script").unwrap(),
            "css tags lead the combined dump: {out}"
        );
        assert!(!out.contains("paste:dump"), "{out}");
    }

    #[test]
    fn test_render_templates_errors_when_a_late_jam_is_never_dumped() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let manifest = test_manifest();
        write_compiled(&config, "/js/paste.h.v1.min.js", "PASTE_RT");
        write_compiled(&config, "/js/app.h.v2.min.js", "APP");
        write_compiled(&config, "/css/site.h.v1.min.css", ".site{}");

        // The css dump already ran when this jam lands: site is queued for
        // a css dump that never comes, and ending on a js dump must not
        // lose it silently.
        write_template(
            &config,
            "index.html",
            "<!-- paste:dump css -->\n<!-- paste:jam css=\"site\" js=\"app\" -->\n<!-- paste:dump js -->",
        );

        let err = render_templates(&config, &manifest).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("css"), "{msg}");
        assert!(msg.contains("site"), "{msg}");
    }

    #[test]
    fn test_render_templates_errors_when_a_late_js_jam_is_never_dumped() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let manifest = test_manifest();
        write_compiled(&config, "/js/paste.h.v1.min.js", "PASTE_RT");
        write_compiled(&config, "/js/app.h.v2.min.js", "APP");
        write_compiled(&config, "/css/site.h.v1.min.css", ".site{}");

        // The symmetric case: app is jammed after the js dump and the page
        // ends on a css dump — the pending js must still fail the build.
        write_template(
            &config,
            "index.html",
            "<!-- paste:dump js -->\n<!-- paste:jam css=\"site\" js=\"app\" -->\n<!-- paste:dump css -->",
        );

        let err = render_templates(&config, &manifest).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("js"), "{msg}");
        assert!(msg.contains("app"), "{msg}");
    }

    #[test]
    fn test_combo_and_loader_files_are_written() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let manifest = test_manifest();
        write_compiled(&config, "/js/paste.h.v1.min.js", "PASTE_RT");
        write_compiled(&config, "/js/app.h.v2.min.js", "APP");
        write_compiled(&config, "/css/site.h.v1.min.css", ".site{}");
        write_template(
            &config,
            "index.html",
            "<!-- paste:jam css=\"site\" js=\"app\" -->\n<!-- paste:dump css -->\n<!-- paste:dump js -->",
        );

        render_templates(&config, &manifest).unwrap();
        let public = &config.output.base;

        // Combo bodies land at {output.base}{url}.
        let combo_css = public.join(format!("paste/{STAMP_EPOCH}/site+v1.0.min.css"));
        assert_eq!(fs::read_to_string(&combo_css).unwrap(), ".site{}");
        let combo_js = public.join(format!("paste/{STAMP_EPOCH}/app+v2.0.min.js"));
        assert_eq!(fs::read_to_string(&combo_js).unwrap(), "APP");

        // Every module with a js asset gets a loader file under the
        // packages segment; a css-only module gets none.
        let packages = jam::packages_segment(&manifest);
        assert_eq!(
            fs::read_to_string(public.join(format!("paste/{packages}/paste.js"))).unwrap(),
            "PASTE_RT"
        );
        assert_eq!(
            fs::read_to_string(public.join(format!("paste/{packages}/app.js"))).unwrap(),
            "APP"
        );
        assert!(!public.join(format!("paste/{packages}/site.js")).exists());
    }

    #[test]
    fn test_referenced_combo_that_cannot_be_served_fails_the_build() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let manifest = test_manifest();
        write_compiled(&config, "/js/paste.h.v1.min.js", "PASTE_RT");
        write_compiled(&config, "/js/app.h.v2.min.js", "APP");
        // site.css has no compiled output — its external URL 404s.
        write_template(
            &config,
            "index.html",
            "<!-- paste:jam css=\"site\" -->\n<!-- paste:dump css -->",
        );

        let err = render_templates(&config, &manifest).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("index.html"), "{msg}");
        assert!(msg.contains("/paste/"), "{msg}");
    }

    #[test]
    fn test_template_jamming_an_unknown_module_fails_the_build() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let manifest = test_manifest();
        write_compiled(&config, "/js/paste.h.v1.min.js", "PASTE_RT");
        write_compiled(&config, "/js/app.h.v2.min.js", "APP");
        write_compiled(&config, "/css/site.h.v1.min.css", ".site{}");
        write_template(
            &config,
            "index.html",
            "<!-- paste:jam js=\"ghost\" -->\n<!-- paste:dump js -->",
        );

        let err = render_templates(&config, &manifest).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("index.html"), "{msg}");
        assert!(msg.contains("ghost"), "{msg}");
    }

    #[test]
    fn test_output_inside_or_equal_to_source_fails() {
        // Rendering into the source tree would overwrite templates or
        // re-read its own output.
        for output in ["templates", "templates/public", "templates/deep/nested"] {
            let dir = tempdir().unwrap();
            fs::create_dir_all(dir.path().join("templates")).unwrap();
            let mut config = test_config(dir.path());
            config.templates = Some(TemplatesConfig {
                source: dir.path().join("templates"),
                output: dir.path().join(output),
            });
            let err = render_templates(&config, &Manifest::new()).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("must not equal or nest inside"),
                "{output} → {msg}"
            );
        }

        // A sibling directory renders fine.
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("templates")).unwrap();
        let mut config = test_config(dir.path());
        config.templates = Some(TemplatesConfig {
            source: dir.path().join("templates"),
            output: dir.path().join("public"),
        });
        render_templates(&config, &Manifest::new()).unwrap();
    }

    #[test]
    fn test_trailing_jam_is_a_build_error_and_plain_files_pass_through() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        // No modules — nothing to materialize beyond the pages themselves.
        let manifest = Manifest::new();

        // A file without directives renders unchanged at the same nested
        // path; non-html files are left alone entirely.
        write_template(
            &config,
            "sub/deep/plain.html",
            "<p>unchanged <!-- note --></p>",
        );
        write_template(&config, "data.json", "{\"not\":\"html\"}");
        render_templates(&config, &manifest).unwrap();
        let output = &config.templates.as_ref().unwrap().output;
        assert_eq!(
            fs::read_to_string(output.join("sub/deep/plain.html")).unwrap(),
            "<p>unchanged <!-- note --></p>"
        );
        assert!(!output.join("data.json").exists());

        // A trailing jam is never emitted, so it fails the build.
        write_template(
            &config,
            "index.html",
            "<!-- paste:jam js=\"app\" -->\n<p>hi</p>",
        );
        let err = render_templates(&config, &manifest).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("index.html:1: jam after the last dump is never emitted"),
            "{msg}"
        );
    }

    #[test]
    fn test_invalid_base_path_fails_even_without_directives() {
        // base_path lands in emitted HTML attributes — an invalid one fails
        // the build even when every template is plain markup.
        for bad in ["/paste/../escape", "/paste?x", "//host", "/a/./b", ""] {
            let dir = tempdir().unwrap();
            let mut config = test_config(dir.path());
            config.jam.base_path = bad.to_string();
            write_template(&config, "index.html", "<p>plain</p>");
            assert!(
                render_templates(&config, &Manifest::new()).is_err(),
                "base_path {bad:?} must fail the build"
            );
            // Nothing may be written outside the output tree.
            assert!(!dir.path().join("escape").exists(), "{bad}");
        }
    }

    #[test]
    fn test_write_output_rejects_unsafe_paths() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("public");
        fs::create_dir_all(&base).unwrap();
        for bad in [
            "/../escape.txt",
            "..\\escape.txt",
            "C:/x.txt",
            "/a//b.txt",
            "",
            "/",
        ] {
            assert!(write_output(&base, bad, b"x").is_err(), "{bad}");
        }
        write_output(&base, "/paste/42/x.css", b"ok").unwrap();
        assert_eq!(
            fs::read_to_string(base.join("paste/42/x.css")).unwrap(),
            "ok"
        );
    }

    #[test]
    fn test_beside_source_writes_combo_and_loader_files_under_output_base() {
        // In beside-source layout the resolver's base is the project root
        // (that is where compiled reads resolve), but the combo and loader
        // files the pages reference are hosted files: they always land
        // under output.base.
        let dir = tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.output.layout = "beside-source".into();

        // Beside-source outputs are project-relative without a type prefix;
        // reads resolve them against base_dir.
        let mut manifest = test_manifest();
        manifest.assets.get_mut("js/paste.js").unwrap().output =
            "/assets/_paste_build/paste.h.v1.min.js".into();
        manifest.assets.get_mut("js/app.js").unwrap().output =
            "/assets/_paste_build/app.h.v2.min.js".into();
        manifest.assets.get_mut("css/site.css").unwrap().output =
            "/assets/_paste_build/site.h.v1.min.css".into();
        for (output, body) in [
            ("/assets/_paste_build/paste.h.v1.min.js", "PASTE_RT"),
            ("/assets/_paste_build/app.h.v2.min.js", "APP"),
            ("/assets/_paste_build/site.h.v1.min.css", ".site{}"),
        ] {
            let path = dir.path().join(output.trim_start_matches('/'));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        write_template(
            &config,
            "index.html",
            "<!-- paste:jam css=\"site\" js=\"app\" -->\n<!-- paste:dump css -->\n<!-- paste:dump js -->",
        );

        render_templates(&config, &manifest).unwrap();
        let public = &config.output.base;

        assert_eq!(
            fs::read_to_string(public.join(format!("paste/{STAMP_EPOCH}/site+v1.0.min.css")))
                .unwrap(),
            ".site{}"
        );
        assert_eq!(
            fs::read_to_string(public.join(format!("paste/{STAMP_EPOCH}/app+v2.0.min.js")))
                .unwrap(),
            "APP"
        );
        let packages = jam::packages_segment(&manifest);
        assert_eq!(
            fs::read_to_string(public.join(format!("paste/{packages}/app.js"))).unwrap(),
            "APP"
        );
        assert!(
            !dir.path().join("paste").exists(),
            "combo files must not be written at the project root"
        );
    }

    /// Build a manifest + compiled files for the given modules, in sorted
    /// order. Entries record the MD5 of the bytes actually written, since
    /// the qualified URL pins those hashes.
    fn segmented_manifest(config: &Config, modules: &[&str], hash_urls: bool) -> Manifest {
        let mut manifest = Manifest::new();
        manifest.hash_urls = hash_urls;
        manifest.version = "1".to_string();
        manifest
            .modules
            .insert("paste".to_string(), "js/paste.js".to_string());
        let mut paste_entry = asset("/js/paste.h.v1.min.js", 1, &[]);
        paste_entry.hash = format!("{:x}", md5::compute(b"PASTE_RT"));
        manifest
            .assets
            .insert("js/paste.js".to_string(), paste_entry);
        let mut sorted = Vec::new();
        for (i, module) in modules.iter().enumerate() {
            let version = (i + 1) as u32;
            let key = format!("js/{}.js", module.replace('.', "/"));
            let output = format!("/js/{}.h.v{}.min.js", module.replace('.', "/"), version);
            manifest.modules.insert(module.to_string(), key.clone());
            let mut entry = asset(&output, version, &[]);
            entry.hash = format!("{:x}", md5::compute(format!("M{i}").as_bytes()));
            manifest.assets.insert(key, entry);
            sorted.push((module.to_string(), output.clone(), version));
            write_compiled(config, &output, &format!("M{i}"));
        }
        manifest.sorted_deps.insert("js".to_string(), sorted);
        write_compiled(config, "/js/paste.h.v1.min.js", "PASTE_RT");
        manifest
    }

    #[test]
    fn test_long_qualified_combo_continues_over_url_components() {
        // Nine hash-qualified modules — the last a Unicode name — push the
        // list well past the 255-byte component limit: the rendered URL
        // continues over `/`-separated components, every materialized name
        // fits the limit, and the joined combo serves the ordered body.
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let modules = [
            "mod.alpha",
            "mod.bravo",
            "mod.charlie",
            "mod.delta",
            "mod.echo",
            "mod.foxtrot",
            "mod.golf",
            "mod.hotel",
            "site.café",
        ];
        let manifest = segmented_manifest(&config, &modules, true);
        assert!(manifest.hash_urls);

        write_template(
            &config,
            "index.html",
            &format!(
                "<!-- paste:jam js=\"{}\" -->\n<!-- paste:dump js -->",
                modules.join(" ")
            ),
        );
        render_templates(&config, &manifest).unwrap();

        let refs: Vec<&str> = modules.to_vec();
        let url = manifest
            .jam_url_with_limit(&refs, "js", "/paste", false, Some(255))
            .unwrap();
        // The full token list does not fit one component — the URL splits
        // at a `~` seam, never inside a token.
        let after_stamp = url.splitn(4, '/').nth(3).unwrap();
        assert!(
            after_stamp.contains('/'),
            "{url} should continue over components"
        );
        for component in url.split('/').skip(1) {
            assert!(
                component.len() <= 255,
                "component {component:?} exceeds 255 bytes in {url}"
            );
            assert!(!component.is_empty(), "{url}");
        }

        // The tag carried it and the combo file exists on disk.
        let out = fs::read_to_string(config.templates.as_ref().unwrap().output.join("index.html"))
            .unwrap();
        assert!(out.contains(&url), "{out}");
        let file = config.output.base.join(url.trim_start_matches('/'));
        assert!(file.is_file(), "missing combo file for {url}");
        for component in file.components() {
            if let std::path::Component::Normal(name) = component {
                assert!(name.len() <= 255, "{name:?} too long");
            }
        }

        // The materialized body is the ordered concat — and the combo
        // endpoint resolves the same segmented URL, percent-encoded
        // Unicode included.
        let expected = (0..modules.len())
            .map(|i| format!("M{i}"))
            .collect::<Vec<_>>()
            .join(crate::graph::JS_CONCAT_SEPARATOR);
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);

        let response = jam::combo(&manifest, &config.output_dirs(), "/paste", &url, None);
        assert_eq!(response.status, 200, "{url}");
        assert_eq!(response.body, expected);

        let encoded = url.replace("café", "caf%C3%A9");
        assert_ne!(encoded, url, "the fixture should exercise encoding");
        let response = jam::combo(&manifest, &config.output_dirs(), "/paste", &encoded, None);
        assert_eq!(response.status, 200, "{encoded}");
        assert_eq!(response.body, expected);
    }

    #[test]
    fn test_optout_long_combo_materializes_the_legacy_form() {
        // With hash_urls off the emitted tokens are short enough to fit one
        // component — legacy shape, same materialization path.
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let modules = [
            "mod.alpha",
            "mod.bravo",
            "mod.charlie",
            "mod.delta",
            "mod.echo",
            "mod.foxtrot",
            "mod.golf",
            "mod.hotel",
            "site.café",
        ];
        let manifest = segmented_manifest(&config, &modules, false);

        write_template(
            &config,
            "index.html",
            &format!(
                "<!-- paste:jam js=\"{}\" -->\n<!-- paste:dump js -->",
                modules.join(" ")
            ),
        );
        render_templates(&config, &manifest).unwrap();

        let refs: Vec<&str> = modules.to_vec();
        let url = manifest.jam_url(&refs, "js", "/paste", false).unwrap();
        assert!(!url.contains("-h"), "{url}");
        assert!(url.contains("mod.alpha+v1.0"), "{url}");

        let file = config.output.base.join(url.trim_start_matches('/'));
        assert!(file.is_file(), "missing combo file for {url}");
        let response = jam::combo(&manifest, &config.output_dirs(), "/paste", &url, None);
        assert_eq!(response.status, 200, "{url}");
    }
}
