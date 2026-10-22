"""Build one distribution for the Node, Python, and JVM contract tests.

Run once with the CLI being tested, then give every runner the same directory
through PASTE_ASSETGRAPH_TEST_FIXTURE. The reference records contain response
digests, not copies of module bodies. CLI comparisons establish adapter
consistency; they are not an independent old-Paste compatibility reference.
"""

import argparse
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


STAMP = 1700000000
JS_SEPARATOR = "\n;\n"
PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXECUTABLE = "paste-assetgraph.exe" if os.name == "nt" else "paste-assetgraph"
BUILD_BINARIES = {
    "debug": PROJECT_ROOT / "target" / "debug" / EXECUTABLE,
    "release": PROJECT_ROOT / "target" / "release" / EXECUTABLE,
    "linux-musl-release": (
        PROJECT_ROOT / "target/x86_64-unknown-linux-musl/release/paste-assetgraph"
    ),
}


def digest(value):
    if isinstance(value, str):
        value = value.encode("utf-8")
    return hashlib.sha256(value).hexdigest()


def file_digests(root):
    return {
        path.relative_to(root).as_posix(): digest(path.read_bytes())
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def write_source(path, body):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    os.utime(path, (STAMP, STAMP))


# The header every Paste module carries (the committed fixtures in
# features/ use the same lines).
PASTE_HEADER = "/*jslint browser:true */\n/*globals paste */\n"


def module(name, version):
    return (
        PASTE_HEADER
        + f"// @module {name}\n// @requires helper\n"
        f'paste.define("{name}", ["helper"], function ({name}, helper) {{\n'
        '  "use strict";\n'
        f'  return function () {{ return "{version}:" + helper(); }};\n'
        "});\n"
    )


def build_case(binary, root, name, minify, layout="flat"):
    project = root / name
    source = project / "src"
    public = project / "dist"
    served = project if layout == "beside-source" else public
    manifest_path = public / "manifest.json"
    write_source(source / "paste.js",
                 (Path(__file__).resolve().parents[1] / "features/paste.js").read_text(
                     encoding="utf-8"))
    write_source(source / "helper.js",
                 PASTE_HEADER
                 + '// @module helper\npaste.define("helper", [], function (helper) {'
                 ' "use strict"; return function () { return "helper"; }; });\n')
    write_source(source / "app.js", module("app", "v1"))
    write_source(source / "later.js", module("later", "v1"))
    write_source(source / "unused.js",
                 PASTE_HEADER
                 + '// @module unused\npaste.define("unused", [], function (unused) {'
                 ' "use strict"; return "unused"; });\n')
    write_source(source / "site.scss",
                 "// @module site\n.site { color: red; }\n")
    config = {
        "source": {"dirs": ["src"]},
        "output": {
            "base": "dist", "js": "dist/js",
            "css": "dist/css", "images": "dist/images",
            "layout": layout,
        },
        "manifest_path": "dist/manifest.json",
        "storage": {"backend": "dist", "keep_versions": "all"},
        "minify": minify,
        "source_maps": False,
    }
    (project / "config.json").write_text(json.dumps(config), encoding="utf-8")
    env = {**os.environ, "SOURCE_DATE_EPOCH": str(STAMP)}

    def invoke(*args, events=None):
        return subprocess.run(
            [str(binary), *map(str, args)],
            input=None if events is None else json.dumps({"events": events}),
            text=True, encoding="utf-8", capture_output=True, env=env,
        )

    def build():
        result = invoke("build", "--cwd", project, "--config", "config.json")
        if result.returncode:
            raise RuntimeError(result.stderr)
        return json.loads(manifest_path.read_text(encoding="utf-8"))

    def render(events, settings=None):
        settings = settings or {}
        return invoke(
            "render", "--manifest", manifest_path, "--output-base", served,
            "--inline-threshold", str(settings.get("inlineThreshold", 0)),
            "--cdn-base", settings.get("cdnBase", ""),
            *(["--per-module"] if settings.get("perModule", False) else []),
            events=events,
        )

    def output(entry):
        return served / entry["output"].lstrip("/")

    def pin(entry, module_name, ext="js"):
        stamp = int(datetime.fromisoformat(entry["timestamp"]).timestamp())
        return (f"/paste/{stamp}/{module_name}+v{entry['version']}.0"
                f"-h{entry['hash']}.min.{ext}")

    first = build()
    first_html = render([{"jam": {"js": ["helper"]}}, {"dump": "js"}])
    if first_html.returncode:
        raise RuntimeError(first_html.stderr)
    old_packages = re.search(r"packagesUrl:'([^']+)'", first_html.stdout).group(1)
    old_app = first["assets"]["js/app.js"]
    old_later = first["assets"]["js/later.js"]
    old_unused = first["assets"]["js/unused.js"]
    app_v1_bytes = output(old_app).read_text(encoding="utf-8")
    later_v1_bytes = output(old_later).read_text(encoding="utf-8")
    unused_bytes = output(old_unused).read_text(encoding="utf-8")
    if not minify:
        assert app_v1_bytes == module("app", "v1")
        assert later_v1_bytes == module("later", "v1")

    write_source(source / "app.js", module("app", "v2"))
    write_source(source / "later.js", module("later", "v2"))
    (source / "unused.js").unlink()
    manifest = build()
    app = manifest["assets"]["js/app.js"]
    helper = manifest["assets"]["js/helper.js"]
    app_bytes = output(app).read_text(encoding="utf-8")
    helper_bytes = output(helper).read_text(encoding="utf-8")
    if not minify:
        assert app_bytes == module("app", "v2")
    assert app["version"] == 2
    assert manifest["assets"]["js/unused.js"]["retired"]
    assert output(old_app).read_text(encoding="utf-8") == app_v1_bytes
    current_html = render([{"jam": {"js": ["helper"]}}, {"dump": "js"}])
    if current_html.returncode:
        raise RuntimeError(current_html.stderr)
    current_packages = re.search(r"packagesUrl:'([^']+)'", current_html.stdout).group(1)
    assert current_packages != old_packages
    assert current_html.stdout.replace(current_packages, old_packages) == first_html.stdout

    # Every content file in the build output must be an individual retained
    # module version. Reference metadata lives outside the distribution.
    expected_paths = {manifest_path.relative_to(served).as_posix()}
    if layout == "beside-source":
        expected_paths.add("config.json")
        # The build's write lock lives at the project root — which
        # beside-source itself serves, so it lands inside `served` here.
        expected_paths.add("manifest.lock")
        expected_paths.update(
            path.relative_to(served).as_posix()
            for path in source.iterdir() if path.is_file()
        )
    for entry in manifest["assets"].values():
        for version in [entry, *entry["history"]]:
            expected_paths.add(version["output"].lstrip("/"))
    before = file_digests(served)
    assert set(before) == expected_paths

    responses = []

    def response(name, path, status, body=None, location=None):
        result = invoke("combo", "--manifest", manifest_path,
                        "--output-base", served, path)
        if result.returncode:
            raise RuntimeError(result.stderr)
        actual = json.loads(result.stdout)
        assert actual["status"] == status, (name, actual)
        if body is not None:
            assert actual["body"] == body, (name, actual)
        if status == 404:
            assert actual["body"] == "/* modules not found */"
            assert actual["cacheControl"] == "no-cache"
            assert not actual["cacheable"]
        if status == 302:
            assert actual["location"] == location
            assert actual["cacheControl"] == "no-cache"
        actual["bodySha256"] = digest(actual.pop("body"))
        responses.append({"name": name, "path": path, "expected": actual})

    current_pin = pin(app, "app")
    response("current", current_pin, 200, app_bytes)
    response("historical", pin(old_app, "app"), 200, app_v1_bytes)
    response("historical loader", old_packages + "app.js", 200, app_v1_bytes)
    response("retired loader", old_packages + "unused.js", 200, unused_bytes)
    response("historical list order", old_packages + "app~helper.js", 200,
             app_v1_bytes + JS_SEPARATOR + helper_bytes)
    response("historical filter", old_packages + "app~helper.js?filter=helper",
             200, app_v1_bytes)
    response("dependency expansion", "/paste/require/app.js", 200,
             helper_bytes + JS_SEPARATOR + app_bytes)
    response("dependency filter", "/paste/require/app.js?filter=helper",
             200, app_bytes)
    response("retired fresh name", "/paste/unused.js", 404)
    response("missing module", "/paste/ghost.js", 404)
    response("wrong hash", current_pin.replace(app["hash"], "0" * 32), 404)
    response("conflicting pins", f"/paste/{STAMP}/app+v1.0~app+v2.0.min.js", 404)
    response("canonical redirect", current_pin.replace(f"/{STAMP}/", "/1/"),
             302, location=current_pin)

    # The runner executes these operations through its public page wrapper.
    # Candidate events enter the reference history only after a successful
    # CLI render. Manifest hiding checks failed-dump recovery.
    pages = []

    def page(name, operations, settings=None, manifest_cdn_base=None):
        events = []
        steps = []
        finished = False
        original_manifest_bytes = manifest_path.read_bytes()
        manifest_bytes = original_manifest_bytes
        if manifest_cdn_base is not None:
            # The scenario's manifest carries this origin — renders inherit
            # it unless the settings override.
            manifest_json = json.loads(original_manifest_bytes)
            manifest_json["cdn_base"] = manifest_cdn_base
            manifest_bytes = json.dumps(
                manifest_json, sort_keys=True
            ).encode("utf-8")
        manifest_path.write_bytes(manifest_bytes)
        try:
            for operation in operations:
                step = dict(operation)
                if "hideManifest" in step:
                    manifest_path.unlink()
                elif "restoreManifest" in step:
                    manifest_path.write_bytes(manifest_bytes)
                elif finished and "finish" not in step:
                    step["errorContains"] = "finished"
                elif "jam" in step:
                    events.append({"jam": step["jam"]})
                    step["valueSha256"] = digest("")
                elif "finish" in step and finished:
                    step["valueSha256"] = digest("")
                else:
                    event = "finish" if "finish" in step else {"dump": step["dump"]}
                    candidate = [*events, event]
                    result = render(candidate, settings)
                    if step.get("error") or "errorContains" in step:
                        assert result.returncode != 0
                        if "errorContains" in step:
                            assert step["errorContains"] in result.stderr
                    else:
                        if result.returncode:
                            raise RuntimeError(result.stderr)
                        step["valueSha256"] = digest(result.stdout)
                        events = candidate
                        finished = "finish" in step
                steps.append(step)
        finally:
            manifest_path.write_bytes(original_manifest_bytes)
        pages.append({
            "name": name,
            "steps": steps,
            "settings": settings or {},
            "manifestCdnBase": manifest_cdn_base,
        })

    page("groups and completion", [
        {"jam": {"css": ["site"], "js": ["app"]}},
        {"dump": "css"}, {"dump": "js"}, {"dump": "js"},
        {"finish": True}, {"finish": True},
        {"jam": {"js": ["app"]}}, {"dump": "js"},
    ])
    page("media groups", [
        {"jam": {"css": ["site"], "media": "print"}}, {"dump": "css"},
        {"jam": {"css": ["site"], "media": "screen"}}, {"dump": "css"},
        {"finish": True},
    ])
    page("finish recovery", [
        {"jam": {"js": ["app"]}}, {"finish": True, "errorContains": "app"},
        {"dump": "js"}, {"finish": True},
    ])
    page("dump recovery", [
        {"hideManifest": True}, {"jam": {"js": ["app"]}},
        {"dump": "js", "error": True},
        {"restoreManifest": True}, {"dump": "js"}, {"finish": True},
    ])
    page("unknown module", [
        {"jam": {"js": ["ghost"]}}, {"dump": "js", "errorContains": "ghost"},
        {"finish": True, "errorContains": "ghost"},
    ])
    page("combined dump", [
        {"jam": {"css": ["site"], "js": ["app"]}},
        {"dump": "all"}, {"finish": True},
    ])
    # Conditional groups wrap their tags; the conditional js group does
    # not mark loaded (or carry the bootstrap) while the conditional css
    # group does mark loaded — the recorded asymmetry.
    page("conditional groups", [
        {"jam": {"css": ["site"], "js": ["app"], "condition": "lt IE 9"}},
        {"dump": "all"},
        {"jam": {"css": ["site"], "js": ["app"]}},
        {"dump": "all"},
        {"finish": True},
    ])
    page("per-module settings", [
        {"jam": {"css": ["site"], "js": ["app"], "media": "print"}},
        {"dump": "all"},
        {"jam": {"css": ["site"], "js": ["app", "later"], "media": "screen"}},
        {"dump": "all"},
        {"dump": "all"},
        {"finish": True},
    ], settings={"perModule": True})
    page("per-module event overrides", [
        {"jam": {"js": ["app"], "perModule": False}},
        {"dump": "js"},
        {"jam": {"css": ["site"], "js": ["later"], "perModule": True}},
        {"dump": "all"},
        {"finish": True},
    ], settings={"perModule": True})
    page("per-module event enables", [
        {"jam": {"css": ["site"], "js": ["app"], "perModule": True}},
        {"dump": "all"},
        {"finish": True},
    ])
    page("per-module synchronous", [
        {"jam": {"css": ["site"], "js": ["app"], "sync": True, "media": "print"}},
        {"dump": "all"},
        {"finish": True},
    ], settings={"perModule": True})
    # The manifest-recorded origin is the default; a runtime cdnBase
    # overrides it — crossed with both emission modes.
    for mode, per_module in [("combo", False), ("per-module", True)]:
        for variant, runtime_base in [
            ("inherit", ""),
            ("override", "https://runtime.example/assets"),
        ]:
            page(f"{mode} cdn {variant}", [
                {"jam": {"css": ["site"], "js": ["app"]}},
                {"dump": "all"},
                {"finish": True},
            ], settings={"perModule": per_module, "cdnBase": runtime_base},
               manifest_cdn_base="https://build.example/assets")
    assert file_digests(served) == before
    return {
        "name": name,
        "manifest": manifest_path.relative_to(root).as_posix(),
        "outputBase": served.relative_to(root).as_posix(),
        "files": before,
        "responses": responses,
        "pages": pages,
        "browser": {
            "oldPackages": old_packages,
            "currentPackages": current_packages,
            "oldMarkupSha256": digest(first_html.stdout),
            "dynamicModule": "later",
            "dynamicValue": "v1:helper",
        },
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", choices=tuple(BUILD_BINARIES), default="debug")
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    root = args.out.resolve()
    root.mkdir(parents=True, exist_ok=True)
    if any(root.iterdir()):
        parser.error("--out must be an empty fixture directory")
    if args.build == "debug":
        binary = BUILD_BINARIES["debug"].resolve(strict=True)
    elif args.build == "release":
        binary = BUILD_BINARIES["release"].resolve(strict=True)
    else:
        binary = BUILD_BINARIES["linux-musl-release"].resolve(strict=True)
    cases = [
        build_case(binary, root, "minified", True),
        build_case(binary, root, "unminified", False),
        build_case(binary, root, "beside-source", True, "beside-source"),
    ]
    (root / "reference.json").write_text(
        json.dumps({"schema": 1, "cases": cases}, sort_keys=True, indent=2) + "\n",
        encoding="utf-8",
    )
    print(f"Shared runtime fixture: {root}")


if __name__ == "__main__":
    main()
