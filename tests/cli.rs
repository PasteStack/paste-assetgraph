//! Integration tests for the `render` and `combo` commands — the contract
//! thin runtimes call. `render` reads `{"events":[...]}` on stdin and prints
//! the last dump's HTML; `combo` prints the ComboResponse JSON.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

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
                "hash": "h",
                "version": 1,
                "timestamp": "2024-01-15T10:00:00+00:00"
            },
            "js/a.js": {
                "source": "src/a.js",
                "output": "/js/a.h.v1.min.js",
                "hash": "h",
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
        "<script type=\"text/javascript\">PASTE_RT\n;\npaste.config({packagesUrl:'/paste/5374b271eac9/'});</script>\n<script type=\"text/javascript\">A</script>"
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

#[test]
fn cli_build_renders_templates_combo_and_loader_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // A minimal project: a runtime module and an app module declared with
    // @module, one scss file, and a templated page.
    std::fs::create_dir_all(root.join("assets/js")).unwrap();
    std::fs::create_dir_all(root.join("assets/scss")).unwrap();
    std::fs::create_dir_all(root.join("templates")).unwrap();
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
        root.join("templates/index.html"),
        concat!(
            "<!doctype html>\n<html>\n<head>\n",
            "<!-- paste:jam css=\"site\" -->\n<!-- paste:dump css -->\n",
            "</head>\n<body>\n",
            "<!-- paste:jam js=\"app\" -->\n<!-- paste:dump js -->\n",
            "</body>\n</html>\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("paste-surface.conf"),
        concat!(
            "source.js = \"assets/js\"\n",
            "source.scss = \"assets/scss\"\n",
            "source.images = \"assets/images\"\n",
            "output.base = \"public\"\n",
            "output.js = \"public/js\"\n",
            "output.css = \"public/css\"\n",
            "output.images = \"public/images\"\n",
            "manifest = \"public/manifest.json\"\n",
            "jam.enabled = true\n",
            "jam.base-path = \"/paste\"\n",
            "jam.inline-threshold = 0\n",
            "templates.source = \"templates\"\n",
            "templates.output = \"public\"\n",
        ),
    )
    .unwrap();

    let out = bin()
        .arg("build")
        .arg("--config")
        .arg("paste-surface.conf")
        .arg("--cwd")
        .arg(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The page rendered with directives gone and a working runtime — the
    // inline bootstrap or an external runtime tag plus the module tag.
    let html = std::fs::read_to_string(root.join("public/index.html")).unwrap();
    assert!(
        html.contains("paste.config({packagesUrl:"),
        "runtime config missing: {html}"
    );
    assert!(
        html.contains("paste.require") || html.contains("src=\"/paste/"),
        "{html}"
    );
    assert!(!html.contains("paste:jam"), "{html}");
    assert!(!html.contains("paste:dump"), "{html}");

    // Every external combo URL the page references exists as a static file
    // — the link's href="..." and the loader's r('...') alike. Directory
    // URLs (the packagesUrl prefix) are not files.
    for url in html
        .split(['\'', '"'])
        .filter(|s| s.starts_with("/paste/") && !s.ends_with('/'))
    {
        let file = root.join("public").join(url.trim_start_matches('/'));
        assert!(file.exists(), "combo file missing for {url}");
        assert!(!std::fs::read_to_string(&file).unwrap().is_empty());
    }

    // Loader files: one per module with a js asset, under the packages
    // segment the config names — the css-only module gets none.
    let mut names = std::collections::BTreeSet::new();
    for version_dir in std::fs::read_dir(root.join("public/paste")).unwrap() {
        for file in std::fs::read_dir(version_dir.unwrap().path()).unwrap() {
            names.insert(file.unwrap().file_name().to_string_lossy().to_string());
        }
    }
    assert!(names.contains("paste.js"), "loader files: {names:?}");
    assert!(names.contains("app.js"), "loader files: {names:?}");
    assert!(!names.contains("site.js"), "loader files: {names:?}");
}

#[test]
fn cli_build_hash_qualified_combo_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Nine SCSS modules in a dependency chain — the last carries a Unicode
    // name — so the rendered combo list runs well past the 255-byte path
    // component limit. The template jams only the tail; expansion orders
    // the whole chain into the URL.
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
    std::fs::create_dir_all(root.join("templates")).unwrap();
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
    std::fs::write(
        root.join("templates/index.html"),
        "<!doctype html>\n<!-- paste:jam css=\"site.café\" -->\n<!-- paste:dump css -->\n",
    )
    .unwrap();
    let write_config = |hash_urls: Option<bool>| {
        let hash_line = match hash_urls {
            Some(v) => format!("jam.hash-urls = {v}\n"),
            None => String::new(),
        };
        std::fs::write(
            root.join("paste-surface.conf"),
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
                    "jam.inline-threshold = 0\n",
                    "{}",
                    "templates.source = \"templates\"\n",
                    "templates.output = \"public\"\n",
                ),
                hash_line
            ),
        )
        .unwrap();
    };
    let build = |root: &Path| {
        let out = bin()
            .arg("build")
            .arg("--config")
            .arg("paste-surface.conf")
            .arg("--cwd")
            .arg(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    // The combo href rendered into the page — extracted from the markup,
    // never rederived through jam_url.
    let rendered_href = |root: &Path| -> String {
        let html = std::fs::read_to_string(root.join("public/index.html")).unwrap();
        html.split("href=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("no stylesheet href in {html}"))
            .to_string()
    };
    let manifest_json = |root: &Path| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(root.join("public/manifest.json")).unwrap())
            .unwrap()
    };

    // jam.hash-urls is absent — the build exercises the default.
    write_config(None);
    build(root);

    let manifest = manifest_json(root);
    assert_eq!(manifest["hash_urls"], true);
    let url = rendered_href(root);
    assert!(url.starts_with("/paste/"), "{url}");
    assert!(url.contains("-h"), "{url} should carry hash-qualified pins");
    assert!(url.contains("site.café"), "Unicode module in {url}");

    // The list continues over path components — each within the filesystem
    // component limit, none empty, suffix only on the last.
    let after_base = url.trim_start_matches("/paste/");
    let components: Vec<&str> = after_base.split('/').collect();
    assert!(
        components.len() > 2,
        "{url} should span the stamp plus multiple list components"
    );
    for component in &components {
        assert!(!component.is_empty() && component.len() <= 255, "{url}");
    }
    assert!(components.last().unwrap().ends_with(".min.css"), "{url}");

    // The materialized combo file exists at the URL path and its bytes are
    // the ordered compiled outputs — each asset's output file under
    // output.base — joined with the CSS newline separator, in dependency
    // order (alpha first, café last).
    let file = root.join("public").join(url.trim_start_matches('/'));
    assert!(file.is_file(), "missing combo file for {url}");
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
    assert_eq!(std::fs::read_to_string(&file).unwrap(), expected);

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

    // Opt out: rebuild with jam.hash-urls = false — same content, legacy
    // URL tokens, and no asset record changes (version/hash/stamp/history).
    write_config(Some(false));
    build(root);
    let opted = manifest_json(root);
    assert_eq!(opted["hash_urls"], false);
    assert_eq!(opted["assets"], manifest["assets"]);
    let legacy_url = rendered_href(root);
    assert!(!legacy_url.contains("-h"), "{legacy_url}");
    assert!(legacy_url.contains("+v"), "{legacy_url}");

    // Re-enable: the original qualified URL comes back unchanged.
    write_config(Some(true));
    build(root);
    let reenabled = manifest_json(root);
    assert_eq!(reenabled["hash_urls"], true);
    assert_eq!(reenabled["assets"], manifest["assets"]);
    assert_eq!(rendered_href(root), url);
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
