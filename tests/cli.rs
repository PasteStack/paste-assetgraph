//! Integration tests for the `render` and `combo` commands — the contract
//! thin runtimes call. `render` reads `{"events":[...]}` on stdin and prints
//! the last dump's HTML; `combo` prints the ComboResponse JSON.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_paste-assetgraph"))
}

fn write_out(dir: &Path, output: &str, content: &str) {
    let path = dir.join(output.trim_start_matches('/'));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn write_manifest(dir: &Path) -> std::path::PathBuf {
    let manifest = serde_json::json!({
        "version": "42",
        "generated": "",
        "assets": {
            "js/paste.js": {
                "source": "src/paste.js",
                "output": "/js/paste.h.v1.min.js",
                "hash": "7e9e92b0857c477a6e8aebb0ab2bd924",
                "version": 1,
                "timestamp": "2024-01-15T10:00:00+00:00"
            },
            "js/a.js": {
                "source": "src/a.js",
                "output": "/js/a.h.v1.min.js",
                "hash": "7fc56270e7a70fa81a5935b72eacbe29",
                "version": 1,
                "timestamp": "2024-01-15T10:00:00+00:00"
            }
        }
    });
    let path = dir.join("manifest.json");
    std::fs::write(&path, serde_json::to_string(&manifest).unwrap()).unwrap();
    path
}

fn render(dir: &Path, manifest: &Path, stdin: &str) -> std::process::Output {
    let mut child = bin()
        .arg("render")
        .arg("--manifest")
        .arg(manifest)
        .arg("--output-base")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn cli_render_prints_exactly_the_dump_html() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path());
    write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
    write_out(dir.path(), "/js/a.h.v1.min.js", "A");

    let out = render(
        dir.path(),
        &manifest,
        r#"{"events":[{"jam":{"js":["a"]}},{"dump":"js"}]}"#,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Exactly the dump's tags — runtime+config inline (the config starts on
    // its own line behind the JS concat separator and carries the loader's
    // packages segment for the js assets), then the module — joined by a
    // newline and with no trailing newline.
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout,
        "<script type=\"text/javascript\">PASTE_RT\n;\npaste.config({packagesUrl:'/paste/471326ca0716/'});</script>\n<script type=\"text/javascript\">A</script>"
    );
}

#[test]
fn cli_render_errors_when_last_event_is_not_a_dump() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path());
    write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");

    let out = render(
        dir.path(),
        &manifest,
        r#"{"events":[{"jam":{"js":["a"]}}]}"#,
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("dump"));
}

#[test]
fn cli_render_rejects_misspelled_jam_options() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path());
    write_out(dir.path(), "/js/paste.h.v1.min.js", "PASTE_RT");
    write_out(dir.path(), "/js/a.h.v1.min.js", "A");

    // A misspelled option at the raw CLI boundary fails the render —
    // it must never silently select the default.
    for stdin in [
        r#"{"events":[{"jam":{"js":["a"],"per_module":true}},{"dump":"js"}]}"#,
        r#"{"events":[{"jam":{"js":["a"],"sycn":true}},{"dump":"js"}]}"#,
        r#"{"events":[{"dump":"js"}],"eventz":[]}"#,
    ] {
        let out = render(dir.path(), &manifest, stdin);
        assert!(!out.status.success(), "{stdin}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("unknown field"),
            "{stdin}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn cli_render_errors_on_missing_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let out = render(
        dir.path(),
        &dir.path().join("absent.json"),
        r#"{"events":[{"dump":"js"}]}"#,
    );
    assert!(!out.status.success());
}

#[test]
fn cli_combo_prints_response_json() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path());
    write_out(dir.path(), "/js/a.h.v1.min.js", "A");

    let out = bin()
        .arg("combo")
        .arg("--manifest")
        .arg(&manifest)
        .arg("--output-base")
        .arg(dir.path())
        // 1705312800 is the manifest fixture's stamp segment.
        .arg("/paste/1705312800/a+v1.0.js")
        .output()
        .unwrap();
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["status"], 200);
    assert_eq!(json["contentType"], "application/javascript; charset=utf-8");
    assert_eq!(json["cacheControl"], "public, max-age=31536000");
    assert_eq!(json["cacheable"], true);
    assert_eq!(json["location"], serde_json::Value::Null);
    assert_eq!(json["body"], "A");
}

#[test]
fn cli_combo_stale_stamp_redirects_to_the_canonical_url() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path());
    write_out(dir.path(), "/js/a.h.v1.min.js", "A");

    // A stamp from an older build sends the client to today's canonical
    // URL — status and Location travel in the JSON.
    let out = bin()
        .arg("combo")
        .arg("--manifest")
        .arg(&manifest)
        .arg("--output-base")
        .arg(dir.path())
        .arg("/paste/1705312799/a+v1.0.min.js")
        .output()
        .unwrap();
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["status"], 302);
    assert_eq!(json["cacheControl"], "no-cache");
    assert_eq!(json["location"], "/paste/1705312800/a+v1.0.min.js");
}

/// The shared fixture: a runtime module, an app module, one scss module,
/// and a config with no publication keys.
fn write_basic_project(root: &Path, extra_config: &str) {
    std::fs::create_dir_all(root.join("assets/js")).unwrap();
    std::fs::create_dir_all(root.join("assets/scss")).unwrap();
    std::fs::write(
        root.join("assets/js/paste.js"),
        "// @module paste\nwindow.paste = { define: function(){}, require: function(){}, config: function(){} };\n",
    )
    .unwrap();
    std::fs::write(
        root.join("assets/js/app.js"),
        "// @module app\nvar app = 1;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("assets/scss/site.scss"),
        "// @module site\n.site { color: red; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("paste-assetgraph.conf"),
        format!(
            concat!(
                "source.js = \"assets/js\"\n",
                "source.scss = \"assets/scss\"\n",
                "output.base = \"public\"\n",
                "output.js = \"public/js\"\n",
                "output.css = \"public/css\"\n",
                "output.images = \"public/images\"\n",
                "manifest = \"public/manifest.json\"\n",
                "jam.enabled = true\n",
                "jam.base-path = \"/paste\"\n",
                "{}",
            ),
            extra_config
        ),
    )
    .unwrap();
}

#[test]
fn cli_build_explicit_source_maps_notice_appears_once() {
    // `source-maps = true` parses for compatibility but emits nothing;
    // the build must say so exactly once — validate() runs at config
    // load AND at build, so the notice lives in emit_compat_notes() which
    // build() calls once.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_basic_project(root, "source-maps = true\n");

    let out = build_at(root);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let occurrences = stderr.matches("source-maps is not implemented").count();
    assert_eq!(
        occurrences, 1,
        "the inert-key notice appears exactly once per build: {stderr}"
    );
}

fn build_at(root: &Path) -> std::process::Output {
    bin()
        .arg("build")
        .arg("--config")
        .arg("paste-assetgraph.conf")
        .arg("--cwd")
        .arg(root)
        .output()
        .unwrap()
}

/// Every file under `root` mapped to its exact bytes — the byte-level
/// comparison `file_set`'s name list cannot make.
fn file_contents(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    files
}

/// Every file under `root` as a forward-slash relative path.
fn file_set(root: &Path) -> std::collections::BTreeSet<String> {
    let mut set = std::collections::BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                set.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    set
}

#[test]
fn cli_build_rejects_templates_configuration() {
    // Both retired keys, and each one alone, fail at config load — a
    // half-removed publication setting must not silently become an
    // assets-only build. No output directory is created on failure.
    for extra in [
        "templates.source = \"templates\"\ntemplates.output = \"public\"\n",
        "templates.source = \"templates\"\n",
        "templates.output = \"public\"\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_basic_project(root, extra);

        let out = build_at(root);
        assert!(
            !out.status.success(),
            "expected a migration error for {extra:?}"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("templates.source/templates.output"),
            "stderr: {stderr}"
        );
        assert!(
            stderr.contains("individual versioned assets"),
            "stderr: {stderr}"
        );
        assert!(
            !root.join("public").exists(),
            "a rejected config must not publish anything"
        );
    }
}

#[test]
fn cli_build_writes_only_versioned_assets_and_metadata() {
    // The whole build contract: individual versioned outputs plus
    // metadata. No rollups, no materialized combos, no per-module loader
    // copies, no generated pages — and request-time serving writes none
    // either.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_basic_project(root, "");

    let out = build_at(root);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let public = root.join("public");
    let files = file_set(&public);

    // Metadata lives in the manifest and its private load cache — the
    // local backend writes both; nothing else may carry content.
    assert!(files.contains("manifest.json"), "files: {files:?}");
    let metadata = ["manifest.json", "manifest.bin"];
    let content_files: Vec<&String> = files
        .iter()
        .filter(|f| !metadata.contains(&f.as_str()))
        .collect();
    assert!(!content_files.is_empty(), "files: {files:?}");

    // Every remaining file is an individual versioned output the manifest
    // itself points at — current or history — under its type root.
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(public.join("manifest.json")).unwrap())
            .unwrap();
    let mut referenced = std::collections::BTreeSet::new();
    for asset in manifest["assets"].as_object().unwrap().values() {
        // Each output is already the path under output.base — `/js/...`
        // and `/css/...` carry their type root.
        for output in std::iter::once(asset["output"].as_str().unwrap()).chain(
            asset["history"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|h| h["output"].as_str().unwrap()),
        ) {
            referenced.insert(output.trim_start_matches('/').to_string());
        }
    }
    for file in &content_files {
        assert!(
            file.starts_with("js/") || file.starts_with("css/"),
            "unexpected generated file: {file}"
        );
        assert!(
            referenced.contains(file.as_str()),
            "{file} is not a manifest-referenced versioned output"
        );
    }

    // The forbidden artifacts by name: no rollup, no combo URL tree, no
    // packages-segment loader copies, no rendered pages.
    assert!(!public.join("paste").exists(), "files: {files:?}");
    for file in &files {
        assert!(
            !file.ends_with(".html"),
            "rendered page must not be generated: {file}"
        );
        assert_ne!(file, "js/assetgraph.js", "files: {files:?}");
        assert_ne!(file, "css/assetgraph.css", "files: {files:?}");
        assert!(
            !file.contains('+'),
            "a combo URL path is never a file: {file}"
        );
    }

    // Request-time resolution reads the individual files only: the combo
    // command answers the canonical URL with a 200 carrying the compiled
    // bytes, and the output tree is byte-identical afterward. Serving a
    // real response — not merely a redirect — is what proves the boundary.
    let mut child = bin()
        .arg("render")
        .arg("--manifest")
        .arg(public.join("manifest.json"))
        .arg("--output-base")
        .arg(&public)
        .arg("--inline-threshold")
        .arg("0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"events":[{"jam":{"css":["site"]}},{"dump":"css"}]}"#.as_slice())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let url = stdout
        .split("href=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("no stylesheet href in {stdout}"));

    let before = file_contents(&public);
    let out = bin()
        .arg("combo")
        .arg("--manifest")
        .arg(public.join("manifest.json"))
        .arg("--output-base")
        .arg(&public)
        .arg(url)
        .output()
        .unwrap();
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["status"], 200, "canonical URL must serve: {json}");
    let expected = std::fs::read_to_string(
        public.join(
            manifest["assets"]["css/site.css"]["output"]
                .as_str()
                .unwrap()
                .trim_start_matches('/'),
        ),
    )
    .unwrap();
    assert_eq!(json["body"].as_str().unwrap(), expected);
    assert_eq!(file_contents(&public), before, "serving must write nothing");
}

#[test]
fn cli_build_hash_qualified_combo_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Nine SCSS modules in a dependency chain — the last carries a Unicode
    // name — so the rendered combo list runs well past the 255-byte path
    // component limit. The page jams only the tail; expansion orders the
    // whole chain into the URL.
    let modules = [
        "site.alpha",
        "site.bravo",
        "site.charlie",
        "site.delta",
        "site.echo",
        "site.foxtrot",
        "site.golf",
        "site.hotel",
        "site.café",
    ];
    std::fs::create_dir_all(root.join("assets/scss")).unwrap();
    for (i, module) in modules.iter().enumerate() {
        let requires = if i == 0 {
            String::new()
        } else {
            format!("// @requires {}\n", modules[i - 1])
        };
        std::fs::write(
            root.join(format!("assets/scss/m{i}.scss")),
            format!("{requires}// @module {module}\n.site {{ z-index: {i}; }}\n"),
        )
        .unwrap();
    }
    let write_config = |hash_urls: Option<bool>| {
        let hash_line = match hash_urls {
            Some(v) => format!("jam.hash-urls = {v}\n"),
            None => String::new(),
        };
        std::fs::write(
            root.join("paste-assetgraph.conf"),
            format!(
                concat!(
                    "source.scss = \"assets/scss\"\n",
                    "output.base = \"public\"\n",
                    "output.js = \"public/js\"\n",
                    "output.css = \"public/css\"\n",
                    "output.images = \"public/images\"\n",
                    "manifest = \"public/manifest.json\"\n",
                    "jam.enabled = true\n",
                    "jam.base-path = \"/paste\"\n",
                    "{}",
                ),
                hash_line
            ),
        )
        .unwrap();
    };
    // The combo href a page would carry — produced by the request-time
    // `render` command on stdout, extracted from the markup, never
    // rederived through jam_url. inline-threshold 0 keeps the group
    // external so the URL appears.
    let rendered_href = |root: &Path| -> String {
        let mut child = bin()
            .arg("render")
            .arg("--manifest")
            .arg(root.join("public/manifest.json"))
            .arg("--output-base")
            .arg(root.join("public"))
            .arg("--inline-threshold")
            .arg("0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                br#"{"events":[{"jam":{"css":["site.caf\u00e9"]}},{"dump":"css"}]}"#.as_slice(),
            )
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
            .split("href=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("no stylesheet href in {stdout}"))
            .to_string()
    };
    let manifest_json = |root: &Path| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(root.join("public/manifest.json")).unwrap())
            .unwrap()
    };

    // jam.hash-urls is absent — the build exercises the default.
    write_config(None);
    let out = build_at(root);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let manifest = manifest_json(root);
    assert_eq!(manifest["hash_urls"], true);
    let url = rendered_href(root);
    assert!(url.starts_with("/paste/"), "{url}");
    assert!(url.contains("-h"), "{url} should carry hash-qualified pins");
    assert!(url.contains("site.café"), "Unicode module in {url}");

    // Request-time emission is not filesystem-constrained: the stamp plus
    // one list component, the whole qualified list in a single component
    // past the 255-byte file-name limit.
    let after_base = url.trim_start_matches("/paste/");
    let components: Vec<&str> = after_base.split('/').collect();
    assert_eq!(
        components.len(),
        2,
        "{url} should be the stamp plus one list component"
    );
    let list_component = components.last().unwrap();
    assert!(list_component.len() > 255, "{url}");
    assert!(list_component.ends_with(".min.css"), "{url}");

    // The combo URL is a request-time address, not a file — the build
    // must not have materialized it under output.base.
    assert!(
        !root
            .join("public")
            .join(url.trim_start_matches('/'))
            .exists(),
        "a combo URL is resolved per request, never written: {url}"
    );

    // The combo response's bytes are the ordered compiled outputs — each
    // asset's output file under output.base — joined with the CSS newline
    // separator, in dependency order (alpha first, café last).
    let assets = manifest["assets"].as_object().unwrap();
    let module_asset = |name: &str| {
        let key = manifest["modules"][name].as_str().unwrap();
        assets[key].clone()
    };
    let expected = modules
        .iter()
        .map(|m| {
            let output = module_asset(m)["output"].as_str().unwrap().to_string();
            std::fs::read_to_string(root.join("public").join(output.trim_start_matches('/')))
                .unwrap_or_else(|e| panic!("missing compiled {output}: {e}"))
        })
        .collect::<Vec<_>>()
        .join("\n");

    // The combo CLI resolves the same URL percent-encoded, as an HTTP
    // request would carry it — é as %C3%A9.
    let encoded = url.replace('é', "%C3%A9");
    let out = bin()
        .arg("combo")
        .arg("--manifest")
        .arg(root.join("public/manifest.json"))
        .arg("--output-base")
        .arg(root.join("public"))
        .arg(&encoded)
        .output()
        .unwrap();
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["status"], 200, "{url}");
    assert_eq!(json["body"], expected);
    assert_eq!(json["cacheable"], true);

    // The segmented spelling — the list carried over '/'-separated path
    // components, each within a filesystem component limit — is how builds
    // that materialized pages once wrote this URL. Those spellings are in
    // the wild; the endpoint must still join them into the same group.
    let tokens: Vec<&str> = list_component
        .trim_end_matches(".min.css")
        .split('~')
        .collect();
    let segmented = format!(
        "/paste/{}/{}/{}.min.css",
        components[0],
        tokens[..4].join("~"),
        tokens[4..].join("~"),
    )
    .replace('é', "%C3%A9");
    let out = bin()
        .arg("combo")
        .arg("--manifest")
        .arg(root.join("public/manifest.json"))
        .arg("--output-base")
        .arg(root.join("public"))
        .arg(&segmented)
        .output()
        .unwrap();
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["status"], 200, "{segmented}");
    assert_eq!(json["body"], expected);

    // Opt out: rebuild with jam.hash-urls = false — same content, legacy
    // URL tokens, and no asset record changes (version/hash/stamp/history).
    write_config(Some(false));
    let out = build_at(root);
    assert!(out.status.success());
    let opted = manifest_json(root);
    assert_eq!(opted["hash_urls"], false);
    assert_eq!(opted["assets"], manifest["assets"]);
    let legacy_url = rendered_href(root);
    assert!(!legacy_url.contains("-h"), "{legacy_url}");
    assert!(legacy_url.contains("+v"), "{legacy_url}");

    // Re-enable: the original qualified URL comes back unchanged.
    write_config(Some(true));
    let out = build_at(root);
    assert!(out.status.success());
    let reenabled = manifest_json(root);
    assert_eq!(reenabled["hash_urls"], true);
    assert_eq!(reenabled["assets"], manifest["assets"]);
    assert_eq!(rendered_href(root), url);
}

#[test]
fn cli_build_warns_on_an_unowned_historical_rollup() {
    // A manifest without output_roots (written before the field existed)
    // cannot establish the moved-from root: the stale aggregate there is
    // reported on stderr and kept, while the new configured root's own
    // copy is still cleaned.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("assets/js")).unwrap();
    std::fs::create_dir_all(root.join("assets/scss")).unwrap();
    std::fs::write(root.join("assets/js/app.js"), "var app = 1;\n").unwrap();
    let write_config = |root: &Path, js_dir: &str| {
        std::fs::write(
            root.join("paste-assetgraph.conf"),
            format!(
                concat!(
                    "source.js = \"assets/js\"\n",
                    "source.scss = \"assets/scss\"\n",
                    "output.base = \"public\"\n",
                    "output.js = \"{}\"\n",
                    "output.css = \"public/css\"\n",
                    "output.images = \"public/images\"\n",
                    "manifest = \"public/manifest.json\"\n",
                    "storage.backend = \"dist\"\n",
                ),
                js_dir
            ),
        )
        .unwrap();
    };
    let build = |root: &Path| {
        let out = bin()
            .arg("build")
            .arg("--config")
            .arg("paste-assetgraph.conf")
            .arg("--cwd")
            .arg(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };

    write_config(root, "public/js");
    build(root);

    // The manifest a pre-output_roots build would have written, then the
    // output-root move and the stale aggregate at the old root.
    let manifest_path = root.join("public/manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    value.as_object_mut().unwrap().remove("output_roots");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
    std::fs::write(root.join("public/js/assetgraph.js"), "var stale = 1;").unwrap();
    write_config(root, "public/js2");
    let out = build(root);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("assetgraph.js") && stderr.contains("not an established js output root"),
        "stderr: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("public/js/assetgraph.js")).unwrap(),
        "var stale = 1;",
        "an unowned location is reported, not deleted"
    );
}

#[test]
fn cli_render_per_module_emits_per_module_tags() {
    // --per-module names the versioned files directly: one ordered
    // loader block per async js group, one link per css module — and
    // every referenced file is on disk for a static host to serve.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_basic_project(root, "");
    let out = build_at(root);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let public = root.join("public");

    let mut child = bin()
        .arg("render")
        .arg("--manifest")
        .arg(public.join("manifest.json"))
        .arg("--output-base")
        .arg(&public)
        .arg("--inline-threshold")
        .arg("0")
        .arg("--per-module")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"{"events":[{"jam":{"css":["site"],"js":["app"]}},{"dump":"all"}]}"#.as_slice(),
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();

    // One link for site, one loader statement for app — the module name
    // in each mark matches what the jam asked for.
    assert_eq!(stdout.matches("<link").count(), 1, "{stdout}");
    assert_eq!(stdout.matches("paste.require").count(), 1, "{stdout}");
    assert!(stdout.contains("d('app').isLoading=!0"), "{stdout}");
    assert!(!stdout.contains("+v"), "{stdout}");

    // Every external URL lands on a real compiled file.
    let urls: Vec<&str> = stdout
        .split('\'')
        .chain(stdout.split("href=\""))
        .filter(|part| part.starts_with("/js/") || part.starts_with("/css/"))
        .map(|part| part.split(['\'', '"']).next().unwrap())
        .collect();
    assert_eq!(urls.len(), 2, "{stdout}");
    for url in urls {
        assert!(
            public.join(url.trim_start_matches('/')).is_file(),
            "{url} must exist under the output directory"
        );
    }
}

#[test]
fn cli_render_cdn_base_overrides_the_manifest_default() {
    // A build under cdn.enabled writes the origin into the manifest;
    // render inherits it, and --cdn-base overrides it. An empty
    // --cdn-base still inherits — the manifest is the fallback, not a
    // second default.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_basic_project(
        root,
        "cdn.enabled = true\ncdn.url = \"https://cdn.example\"\n",
    );
    let out = build_at(root);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let public = root.join("public");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(public.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["cdn_base"], "https://cdn.example");

    let render_with = |args: &[&str]| {
        let mut command = bin();
        command
            .arg("render")
            .arg("--manifest")
            .arg(public.join("manifest.json"))
            .arg("--output-base")
            .arg(&public)
            .arg("--inline-threshold")
            .arg("0")
            .args(args);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"}]}"#.as_slice())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };

    assert!(
        render_with(&[]).contains("r('https://cdn.example/paste/"),
        "manifest default applies"
    );
    assert!(
        render_with(&["--cdn-base", "https://other.example"])
            .contains("r('https://other.example/paste/"),
        "the flag overrides the manifest"
    );
}

#[test]
fn cli_build_resolves_the_configuration_filename() {
    // Without --config the new default name wins, the former name still
    // loads with a notice, both files prefer the new name with a notice,
    // and neither errors naming the new name.
    let project_config = "source.js = \"assets/js\"\nsource.scss = \"assets/scss\"\noutput.base = \"public\"\nmanifest = \"public/manifest.json\"\n";
    let scaffold = |root: &Path| {
        std::fs::create_dir_all(root.join("assets/js")).unwrap();
        std::fs::create_dir_all(root.join("assets/scss")).unwrap();
        std::fs::write(root.join("assets/js/app.js"), "var app = 1;\n").unwrap();
    };
    let build_default = |root: &Path| bin().arg("build").arg("--cwd").arg(root).output().unwrap();

    // New name only — silent success.
    let dir = tempfile::tempdir().unwrap();
    scaffold(dir.path());
    std::fs::write(dir.path().join("paste-assetgraph.conf"), project_config).unwrap();
    let out = build_default(dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("public/manifest.json").exists());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("paste-surface"));

    // Former name only — used, with the migration notice.
    let dir = tempfile::tempdir().unwrap();
    scaffold(dir.path());
    std::fs::write(dir.path().join("paste-surface.conf"), project_config).unwrap();
    let out = build_default(dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("paste-surface.conf is the former default name"),
        "stderr: {stderr}"
    );
    assert!(dir.path().join("public/manifest.json").exists());

    // Both — the new name wins and the notice names the ignored file.
    let dir = tempfile::tempdir().unwrap();
    scaffold(dir.path());
    std::fs::write(dir.path().join("paste-assetgraph.conf"), project_config).unwrap();
    std::fs::write(
        dir.path().join("paste-surface.conf"),
        "manifest = \"nope.json\"\n",
    )
    .unwrap();
    let out = build_default(dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("using paste-assetgraph.conf; ignoring paste-surface.conf"),
        "stderr: {stderr}"
    );
    assert!(dir.path().join("public/manifest.json").exists());
    assert!(!dir.path().join("nope.json").exists());

    // Neither — the load error names the new default.
    let dir = tempfile::tempdir().unwrap();
    scaffold(dir.path());
    let out = build_default(dir.path());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("paste-assetgraph.conf"), "stderr: {stderr}");

    // --config with the former name still works, without a notice.
    let dir = tempfile::tempdir().unwrap();
    scaffold(dir.path());
    std::fs::write(dir.path().join("paste-surface.conf"), project_config).unwrap();
    let out = bin()
        .arg("build")
        .arg("--config")
        .arg("paste-surface.conf")
        .arg("--cwd")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("public/manifest.json").exists());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("former default"));
}

#[test]
fn cli_init_writes_paste_assetgraph_conf() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin()
        .arg("init")
        .arg("--cwd")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("paste-assetgraph.conf").is_file());
    assert!(!dir.path().join("paste-surface.conf").exists());
}

#[test]
fn cli_combo_exits_zero_on_404() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path());

    // The status travels in the JSON — a 404 is still a successful run.
    let out = bin()
        .arg("combo")
        .arg("--manifest")
        .arg(&manifest)
        .arg("--output-base")
        .arg(dir.path())
        .arg("/paste/1/missing+v1.0.js")
        .output()
        .unwrap();
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["status"], 404);
    assert_eq!(json["cacheable"], false);
    assert_eq!(json["body"], "/* modules not found */");
}

/// Watch startup contract: a populated source root missing when `watch`
/// starts is a transient condition, not a fatal one — the ancestor watch
/// registered before the first build keeps the process alive and
/// announces the root's return, and the rebuild happens on that event.
/// While the root is gone the previously published manifest is untouched.
#[test]
fn watch_recovers_a_source_root_missing_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_basic_project(root, "");

    // A populated project publishes first.
    let out = build_at(root);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest_bytes = std::fs::read(root.join("public/manifest.json")).unwrap();

    // The js root leaves before watch starts — the classic transient:
    // a not-yet-mounted or renamed source tree.
    let parked = root.join("assets/js-parked");
    std::fs::rename(root.join("assets/js"), &parked).unwrap();

    let mut child = bin()
        .arg("watch")
        .arg("--config")
        .arg("paste-assetgraph.conf")
        .arg("--cwd")
        .arg(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Drain both pipes on reader threads so the child never blocks on a
    // full buffer and the test can poll the transcript.
    let transcript = drain_child(&mut child);

    // Startup: the build fails — but the process stays alive behind the
    // ancestor watch, and the published manifest is untouched. The
    // recoverable-failure message also certifies the error was classified
    // as source-repairable rather than structural.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if transcript
            .lock()
            .unwrap()
            .contains("Initial build waiting for a watched input repair")
        {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "watch exited ({status}) instead of waiting on the ancestor watch; transcript: {}",
                transcript.lock().unwrap()
            );
        }
        assert!(
            Instant::now() < deadline,
            "no initial-build failure logged; transcript: {}",
            transcript.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        child.try_wait().unwrap().is_none(),
        "watch must stay alive behind the missing root's ancestor watch"
    );
    assert_eq!(
        std::fs::read(root.join("public/manifest.json")).unwrap(),
        manifest_bytes,
        "the prior manifest is untouched while the root is gone"
    );

    // The root returns: its ancestor watch announces the create event,
    // the debounced batch rebuilds — no restart needed.
    std::fs::rename(&parked, root.join("assets/js")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if transcript.lock().unwrap().contains("Rebuild complete") {
            break;
        }
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "watch never rebuilt after the root returned; transcript: {}",
            transcript.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    child.kill().unwrap();
    let _ = child.wait();
}

/// `watch` builds in the same mode as `build`: both write into one retained
/// version history, so starting a watch over sources `build` just published
/// rebuilds to the same bytes and mints no version. Only an explicit
/// `watch --debug` publishes unminified output.
#[test]
fn watch_builds_in_the_build_mode_and_mints_no_version_over_unchanged_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_basic_project(root, "");
    let public = root.join("public");
    let assets = |root: &Path| -> serde_json::Value {
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("public/manifest.json")).unwrap(),
        )
        .unwrap();
        manifest["assets"].clone()
    };
    let without_manifest = |files: std::collections::BTreeMap<String, Vec<u8>>| {
        files
            .into_iter()
            .filter(|(name, _)| name != "manifest.json")
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let watch_once = |extra: &[&str]| {
        let mut child = bin()
            .arg("watch")
            .args(extra)
            .arg("--config")
            .arg("paste-assetgraph.conf")
            .arg("--cwd")
            .arg(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let transcript = drain_child(&mut child);
        await_log(&mut child, &transcript, "Initial build complete");
        child.kill().unwrap();
        let _ = child.wait();
    };

    let out = build_at(root);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let built_assets = assets(root);
    let built_files = without_manifest(file_contents(&public));

    // build -> watch -> build over unchanged sources: every published
    // entry and every output file stays exactly as the first build left it.
    watch_once(&[]);
    assert_eq!(
        assets(root),
        built_assets,
        "watch over unchanged sources must not mint a version"
    );
    assert_eq!(without_manifest(file_contents(&public)), built_files);
    let out = build_at(root);
    assert!(out.status.success());
    assert_eq!(
        assets(root),
        built_assets,
        "build after watch must not mint a version"
    );
    assert_eq!(without_manifest(file_contents(&public)), built_files);

    // An explicit --debug is the one way watch switches to unminified
    // output, and it publishes that as a new current version.
    watch_once(&["--debug"]);
    let debug_assets = assets(root);
    let built_output = built_assets["js/app.js"]["output"].as_str().unwrap();
    let debug_output = debug_assets["js/app.js"]["output"].as_str().unwrap();
    assert_ne!(
        debug_output, built_output,
        "watch --debug publishes its own version"
    );
    let debug_body =
        std::fs::read_to_string(public.join(debug_output.trim_start_matches('/'))).unwrap();
    assert!(
        debug_body.contains("var app = 1;"),
        "watch --debug output is unminified: {debug_body:?}"
    );
    let built_body =
        std::fs::read_to_string(public.join(built_output.trim_start_matches('/'))).unwrap();
    assert!(
        !built_body.contains("var app = 1;"),
        "the default build output is minified: {built_body:?}"
    );
}

/// Spawn `watch` for the project and drain both output pipes into one
/// transcript on reader threads — a full pipe would otherwise block the
/// child mid-test.
#[cfg(unix)]
fn spawn_watch(
    root: &Path,
) -> (
    std::process::Child,
    std::sync::Arc<std::sync::Mutex<String>>,
) {
    let mut child = bin()
        .arg("watch")
        .arg("--config")
        .arg("paste-assetgraph.conf")
        .arg("--cwd")
        .arg(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let transcript = drain_child(&mut child);
    (child, transcript)
}

fn drain_child(child: &mut std::process::Child) -> std::sync::Arc<std::sync::Mutex<String>> {
    let transcript = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let pipes: Vec<Box<dyn Read + Send>> = vec![
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    for pipe in pipes {
        let transcript = transcript.clone();
        std::thread::spawn(move || {
            let mut line = String::new();
            let mut reader = BufReader::new(pipe);
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                transcript.lock().unwrap().push_str(&line);
                line.clear();
            }
        });
    }
    transcript
}

/// Wait until the transcript mentions `needle`, the watch exits early, or
/// twenty seconds pass — the failure transcript is the useful diagnostic.
fn await_log(
    child: &mut std::process::Child,
    transcript: &std::sync::Arc<std::sync::Mutex<String>>,
    needle: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if transcript.lock().unwrap().contains(needle) {
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "watch exited ({status}) waiting for {needle:?}; transcript: {}",
                transcript.lock().unwrap()
            );
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {needle:?}; transcript: {}",
            transcript.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn rebuild_count(transcript: &std::sync::Arc<std::sync::Mutex<String>>) -> usize {
    transcript
        .lock()
        .unwrap()
        .matches("Rebuild complete")
        .count()
}

/// The css output the current manifest points at — the freshly built
/// file, not an older version kept beside it.
#[cfg(unix)]
fn current_css_body(root: &Path) -> String {
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("public/manifest.json")).unwrap())
            .unwrap();
    let output = manifest["assets"]["css/site.css"]["output"]
        .as_str()
        .expect("manifest carries css/site.css output");
    std::fs::read_to_string(root.join("public").join(output.trim_start_matches('/'))).unwrap()
}

/// An imported file symlink's watch follows the entry, not the bytes it
/// resolved to: retargeting the link to an identical-content file must
/// still move the observation, so later edits to the new target rebuild —
/// and edits to the old target no longer do.
#[cfg(unix)]
#[test]
fn watch_follows_sass_import_symlink_retargets() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_basic_project(root, "");

    // site.scss imports outside its root: shared/_theme.scss is a link
    // into a sibling directory — an external input watched through its
    // parent entry plus the resolved file.
    std::fs::write(
        root.join("assets/scss/site.scss"),
        "// @module site\n@use '../shared/theme' as theme; .site { color: theme.$accent; }\n",
    )
    .unwrap();
    let shared = root.join("assets/shared");
    let targets = root.join("assets/targets");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::create_dir_all(&targets).unwrap();
    std::fs::write(targets.join("one.scss"), "$accent: #123456;\n").unwrap();
    symlink(targets.join("one.scss"), shared.join("_theme.scss")).unwrap();

    let (mut child, transcript) = spawn_watch(root);
    await_log(&mut child, &transcript, "Initial build complete");

    // An edit to the resolved import rebuilds through the file watch.
    std::fs::write(targets.join("one.scss"), "$accent: #654321;\n").unwrap();
    await_log(&mut child, &transcript, "Rebuild complete");
    assert_eq!(rebuild_count(&transcript), 1);
    assert!(
        current_css_body(root).contains("#654321"),
        "the rebuild carried the edited import"
    );

    // A sibling file the build never consumed must not rebuild.
    std::fs::write(shared.join("unrelated.scss"), "$other: 1;\n").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        rebuild_count(&transcript),
        1,
        "an unrelated sibling write caused a rebuild; transcript: {}",
        transcript.lock().unwrap()
    );

    // Replace the link with an identical-content target: the bytes are
    // the same, but the dependency identity moved.
    std::fs::write(targets.join("two.scss"), "$accent: #654321;\n").unwrap();
    symlink(targets.join("two.scss"), shared.join("replacement")).unwrap();
    std::fs::rename(shared.join("replacement"), shared.join("_theme.scss")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while rebuild_count(&transcript) < 2 {
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "watch never rebuilt after the equal-content retarget; transcript: {}",
            transcript.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The observation moved: the new target rebuilds, the old one does
    // not. If the watch still held the first target this times out.
    std::fs::write(targets.join("two.scss"), "$accent: #789abc;\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while rebuild_count(&transcript) < 3 {
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "watch kept the old target after the retarget; transcript: {}",
            transcript.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(current_css_body(root).contains("#789abc"));

    std::fs::write(targets.join("one.scss"), "$accent: #000000;\n").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        rebuild_count(&transcript),
        3,
        "a write to the superseded target rebuilt; transcript: {}",
        transcript.lock().unwrap()
    );

    child.kill().unwrap();
    let _ = child.wait();
}

/// A source root reached through a symlink keeps working when the link
/// retargets: the link's own entry is a watched input, so replacing it
/// rebuilds from the new tree and re-arms the root's recursive watch.
#[cfg(unix)]
#[test]
fn watch_survives_a_source_root_symlink_retarget() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_basic_project(root, "");

    // The configured scss root becomes a link: first at real-scss-a,
    // then at real-scss-b with different content.
    let scss = root.join("assets/scss");
    let first = root.join("real-scss-a");
    let second = root.join("real-scss-b");
    std::fs::rename(&scss, &first).unwrap();
    std::fs::write(
        first.join("site.scss"),
        "// @module site\n.site { color: #123456; }\n",
    )
    .unwrap();
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(
        second.join("site.scss"),
        "// @module site\n.site { color: #654321; }\n",
    )
    .unwrap();
    symlink(&first, &scss).unwrap();

    let (mut child, transcript) = spawn_watch(root);
    await_log(&mut child, &transcript, "Initial build complete");
    assert!(current_css_body(root).contains("#123456"));

    // Retarget the root link — the rebuild must read the new tree even
    // though every configured path spells the same.
    let replacement = root.join("assets/scss-next");
    symlink(&second, &replacement).unwrap();
    std::fs::rename(&replacement, &scss).unwrap();
    await_log(&mut child, &transcript, "Rebuild complete");
    assert!(
        current_css_body(root).contains("#654321"),
        "the rebuild read the retargeted root; transcript: {}",
        transcript.lock().unwrap()
    );

    // The re-armed root watch now observes the new target's contents.
    std::fs::write(
        second.join("site.scss"),
        "// @module site\n.site { color: #789abc; }\n",
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while rebuild_count(&transcript) < 2 {
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "watch never rebuilt under the retargeted root; transcript: {}",
            transcript.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(current_css_body(root).contains("#789abc"));

    // An equal-content retarget emits no visible CSS change, but the
    // registration must still move: a later in-place edit inside the
    // new target has to rebuild.
    let third = root.join("real-scss-c");
    std::fs::create_dir_all(&third).unwrap();
    std::fs::write(
        third.join("site.scss"),
        "// @module site\n.site { color: #789abc; }\n",
    )
    .unwrap();
    let replacement = root.join("assets/scss-next");
    symlink(&third, &replacement).unwrap();
    std::fs::rename(&replacement, &scss).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while rebuild_count(&transcript) < 3 {
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "watch never rebuilt on the equal-content retarget; transcript: {}",
            transcript.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // The retarget rebuild settled and re-armed the registration —
    // writing now must be observed, not folded into the swap window.
    std::thread::sleep(Duration::from_millis(400));
    std::fs::write(
        third.join("site.scss"),
        "// @module site\n.site { color: #0f0f0f; }\n",
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while rebuild_count(&transcript) < 4 {
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "watch never rebuilt inside the equal-content retarget; transcript: {}",
            transcript.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(current_css_body(root).contains("#0f0f0f"));

    child.kill().unwrap();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn cli_build_through_a_symlinked_project_root() {
    // A deploy-style `current -> release` link names the project; the
    // build anchors at the real directory instead of refusing the link.
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("release");
    write_basic_project(&real, "");
    let link = dir.path().join("current");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let out = build_at(&link);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(real.join("public/manifest.json").is_file());
}

#[test]
fn cli_build_publishes_a_manifest_configured_outside_the_project() {
    // A manifest outside both the project and every output root is its
    // own configured location — publication anchors at its directory.
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    write_basic_project(&project, "");
    let elsewhere = dir.path().join("elsewhere");
    let conf = std::fs::read_to_string(project.join("paste-assetgraph.conf"))
        .unwrap()
        .replace(
            "manifest = \"public/manifest.json\"",
            &format!(
                "manifest = \"{}\"",
                elsewhere.join("manifest.json").display()
            ),
        );
    std::fs::write(project.join("paste-assetgraph.conf"), conf).unwrap();
    let out = build_at(&project);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(elsewhere.join("manifest.json").is_file());
}

#[cfg(unix)]
#[test]
fn cli_build_accepts_absolute_paths_spelled_through_the_project_link() {
    // `--cwd link` with absolute config paths spelled through the same
    // link: the paths stay inside the project as written.
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("release");
    std::fs::create_dir_all(real.join("src")).unwrap();
    std::fs::write(real.join("src/app.js"), "var app = 1;\n").unwrap();
    let link = dir.path().join("current");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    std::fs::write(
        real.join("paste-assetgraph.conf"),
        format!(
            "source.dirs = [\"src\"]\noutput.layout = \"beside-source\"\noutput.base = \"{}\"\nmanifest = \"dist/manifest.json\"\n",
            link.join("dist").display()
        ),
    )
    .unwrap();
    let out = build_at(&link);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(real.join("dist/manifest.json").is_file());
}
