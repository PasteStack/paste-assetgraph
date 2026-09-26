import ctypes
import json
import os
import platform
import sys
import tempfile
import unittest

from paste_assetgraph import (
    ComboResponse,
    Paste,
    PasteError,
    resolve_library,
)

def library():
    """The library resolves like at run time: PASTE_ASSETGRAPH_LIB when set,
    else the bundled native-lib/ copy the package ships."""
    return resolve_library()


MANIFEST = json.dumps({
    "version": "42",
    "generated": "2026-01-01T00:00:00Z",
    "modules": {"paste": "js/paste.js", "app": "js/app.js", "site.theme": "css/theme.css"},
    "assets": {
        "js/paste.js": {
            "source": "src/paste.js", "output": "/js/paste.h.v1.min.js", "hash": "h",
            "version": 1, "timestamp": "2026-01-01T00:00:00Z",
            "dependencies": [], "history": [],
        },
        "js/app.js": {
            "source": "src/app.js", "output": "/js/app.h.v1.min.js", "hash": "h",
            "version": 1, "timestamp": "2026-01-01T00:00:00Z",
            "dependencies": [], "history": [],
        },
        "css/theme.css": {
            "source": "src/theme.scss", "output": "/css/theme.h.v1.min.css", "hash": "h",
            "version": 1, "timestamp": "2026-01-01T00:00:00Z",
            "dependencies": [], "history": [],
        },
    },
    "sorted_deps": {},
})


def write(path, content):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(content)
    return path


def e2e_fixture(root):
    """A manifest file plus its compiled outputs, in the layout the build writes."""
    out = os.path.join(root, "public")
    write(os.path.join(out, "js", "paste.h.v1.min.js"), "PASTE_RT")
    write(os.path.join(out, "js", "app.h.v1.min.js"), "APP")
    write(os.path.join(out, "css", "theme.h.v1.min.css"), "CSS")
    manifest = write(os.path.join(root, "manifest.json"), MANIFEST)
    return manifest, out


class PasteEndToEndTest(unittest.TestCase):
    def test_page_renders_end_to_end_through_the_library(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()

            self.assertEqual(page.jam(css=("site.theme",), js=("app",)), "")

            css_tag = page.dump_css()
            self.assertIn("stylesheet", css_tag)
            self.assertIn("site.theme+v1.0", css_tag)

            js_tags = page.dump_js()
            self.assertTrue(js_tags.startswith('<script type="text/javascript">PASTE_RT'))
            self.assertIn("(function(d,r){", js_tags)
            self.assertIn("app+v1.0", js_tags)

            # A complete page seals cleanly.
            self.assertEqual(page.finish(), "")
            self.assertEqual(page.finish(), "")

    def test_finish_seals_the_page_and_a_late_jam_fails(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()

            # The css dump already ran when the jam lands: site.theme is
            # queued for a dump that never comes, and finishing on the js
            # dump must refuse — not drop the stylesheet silently.
            page.dump_css()
            page.jam(css=("site.theme",), js=("app",))
            page.dump_js()
            with self.assertRaises(PasteError) as raised:
                page.finish()
            self.assertIn("site.theme", str(raised.exception))

            # A failed finish leaves the page open: deliver the missing
            # dump, then finish — and finishing again is a no-op.
            page.dump_css()
            self.assertEqual(page.finish(), "")
            self.assertEqual(page.finish(), "")

            # A sealed page accepts no more events.
            with self.assertRaises(PasteError) as raised:
                page.jam(js=("app",))
            self.assertIn("finished", str(raised.exception))
            with self.assertRaises(PasteError) as raised:
                page.dump_js()
            self.assertIn("finished", str(raised.exception))

    def test_combo_passes_through_the_library_response(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, library=library())

            # 1767225600 is the fixture stamp (2026-01-01T00:00:00Z).
            versioned = paste.combo("/paste/1767225600/app+v1.0.min.js")
            self.assertEqual(versioned.status, 200)
            self.assertTrue(versioned.cacheable)
            self.assertEqual(versioned.cache_control, "public, max-age=31536000")
            self.assertIsNone(versioned.location)
            self.assertEqual(versioned.body, "APP")
            self.assertEqual(versioned.content_type, "application/javascript; charset=utf-8")

            unversioned = paste.combo("/paste/app.js")
            self.assertEqual(unversioned.status, 200)
            self.assertFalse(unversioned.cacheable)
            self.assertEqual(unversioned.cache_control, "no-cache")

            missing = paste.combo("/paste/1767225600/ghost+v1.0.min.js")
            self.assertEqual(missing.status, 404)
            self.assertFalse(missing.cacheable)

    def test_combo_stale_stamp_carries_location(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, library=library())

            # A stale stamp canonicalizes: `location` names the URL the
            # host's HTTP layer should redirect to.
            redirected = paste.combo("/paste/1767225599/app+v1.0.min.js")
            self.assertEqual(redirected.status, 302)
            self.assertEqual(
                redirected.location, "/paste/1767225600/app+v1.0.min.js"
            )
            self.assertEqual(redirected.cache_control, "no-cache")
            self.assertEqual(redirected.body, "")

    def test_hash_qualified_urls_render_serve_redirect_and_validate(self):
        # The hash-qualified contract lives in the library — the wrapper
        # only forwards paths. The fixture pins the content hashes the
        # contract records for these bytes (golden vectors — hash
        # calculation is the compiler's, tested in Rust).
        APP = "0bdbb2c325525e986d55ef50ca66d056"
        PASTE_RT = "7e9e92b0857c477a6e8aebb0ab2bd924"
        CSS = "2c56c360580420d293172f42d85dfbed"
        BOGUS = "90affbd9a1954ec9ff029b7ad7183a16"

        with tempfile.TemporaryDirectory(prefix="paste-py-hash-") as root:
            app_hash = APP
            hashed = json.loads(MANIFEST)
            hashed["hash_urls"] = True
            hashed["assets"]["js/paste.js"]["hash"] = PASTE_RT
            hashed["assets"]["js/app.js"]["hash"] = app_hash
            hashed["assets"]["css/theme.css"]["hash"] = CSS

            out = os.path.join(root, "public")
            write(os.path.join(out, "js", "paste.h.v1.min.js"), "PASTE_RT")
            write(os.path.join(out, "js", "app.h.v1.min.js"), "APP")
            write(os.path.join(out, "css", "theme.h.v1.min.css"), "CSS")
            manifest = write(
                os.path.join(root, "manifest.json"), json.dumps(hashed)
            )
            paste = Paste(manifest, out, inline_threshold=0, library=library())

            page = paste.page()
            page.jam(js=("app",))
            self.assertIn(f"app+v1.0-h{app_hash}", page.dump_js())

            qualified = paste.combo(
                f"/paste/1767225600/app+v1.0-h{app_hash}.min.js"
            )
            self.assertEqual(qualified.status, 200)
            self.assertEqual(qualified.body, "APP")

            legacy = paste.combo("/paste/1767225600/app+v1.0.min.js")
            self.assertEqual(legacy.status, 302)
            self.assertEqual(
                legacy.location,
                f"/paste/1767225600/app+v1.0-h{app_hash}.min.js",
            )

            wrong = paste.combo(
                f"/paste/1767225600/app+v1.0-h{BOGUS}.min.js"
            )
            self.assertEqual(wrong.status, 404)
            self.assertIsNone(wrong.location)

            # Opted out, the same manifest serves and renders the legacy
            # form — the version unchanged — while a correctly qualified
            # URL still resolves.
            hashed["hash_urls"] = False
            opted_out = Paste(
                write(os.path.join(root, "manifest-legacy.json"),
                      json.dumps(hashed)),
                out,
                inline_threshold=0,
                library=library(),
            )
            self.assertEqual(
                opted_out.combo("/paste/1767225600/app+v1.0.min.js").status, 200
            )
            self.assertEqual(
                opted_out.combo(
                    f"/paste/1767225600/app+v1.0-h{app_hash}.min.js"
                ).status,
                200,
            )
            page = opted_out.page()
            page.jam(js=("app",))
            tags = page.dump_js()
            self.assertIn("app+v1.0.min.js", tags)
            self.assertNotIn("-h", tags)

    def test_render_naming_an_unknown_module_raises_paste_error_naming_it(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()
            page.jam(js=("ghost",))
            with self.assertRaises(PasteError) as ctx:
                page.dump_js()
            self.assertIn("ghost", str(ctx.exception))


class PastePageTest(unittest.TestCase):
    def test_jam_does_not_alias_caller_lists(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-alias-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()
            modules = ["app"]
            page.jam(js=modules)
            # Mutating the caller's list must not rewrite the recorded event:
            # an aliased list would render 'ghost', which is not in the
            # manifest and would raise.
            modules.clear()
            modules.append("ghost")

            js_tags = page.dump_js()
            self.assertIn("app+v1.0", js_tags)
            self.assertNotIn("ghost", js_tags)


class ResolveLibraryTest(unittest.TestCase):
    def test_env_override_wins(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            fake = write(os.path.join(root, "fake-lib.so"), "x")
            self.assertEqual(
                resolve_library({"PASTE_ASSETGRAPH_LIB": fake}, package_dir=root),
                fake,
            )

    def test_set_but_invalid_env_fails_loudly_naming_the_value(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            missing = os.path.join(root, "missing")
            with self.assertRaises(PasteError) as ctx:
                resolve_library(
                    {"PASTE_ASSETGRAPH_LIB": missing}, package_dir=root
                )
            self.assertIn(missing, str(ctx.exception))

            directory = os.path.join(root, "dir")
            os.makedirs(directory)
            with self.assertRaises(PasteError) as ctx:
                resolve_library(
                    {"PASTE_ASSETGRAPH_LIB": directory}, package_dir=root
                )
            self.assertIn(directory, str(ctx.exception))

    def test_empty_env_override_fails_rather_than_falling_through(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            with self.assertRaises(PasteError) as ctx:
                resolve_library({"PASTE_ASSETGRAPH_LIB": ""}, package_dir=root)
            self.assertIn("PASTE_ASSETGRAPH_LIB", str(ctx.exception))

    def test_bundled_copy_under_an_injected_package_dir(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            # The platform segment mirrors the package's _platform_dir —
            # Linux bundles split by libc.
            if sys.platform == "darwin":
                os_name, file_name = "darwin", "libpaste_assetgraph_ffi.dylib"
            elif os.name == "nt":
                os_name, file_name = "windows", "paste_assetgraph_ffi.dll"
            else:
                os_name, file_name = "linux", "libpaste_assetgraph_ffi.so"
            arch = {"arm64": "aarch64", "aarch64": "aarch64",
                    "x86_64": "amd64", "amd64": "amd64"}.get(
                platform.machine().lower()
            )
            libc = ""
            if os_name == "linux" and not hasattr(
                ctypes.CDLL(None), "gnu_get_libc_version"
            ):
                libc = "-musl"
            bundled = write(
                os.path.join(
                    root, "native-lib", f"{os_name}-{arch}{libc}", file_name
                ),
                "x",
            )
            self.assertEqual(resolve_library({}, package_dir=root), bundled)

    def test_nothing_found_names_both_places_and_the_platform(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            with self.assertRaises(PasteError) as ctx:
                resolve_library({}, package_dir=root)
            message = str(ctx.exception)
            self.assertIn("PASTE_ASSETGRAPH_LIB", message)
            self.assertIn("native-lib", message)
            self.assertIn("platform", message)


class ComboContractTest(unittest.TestCase):
    def test_combo_path_with_nul_is_rejected_not_truncated(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, library=library())
            with self.assertRaises(PasteError) as ctx:
                paste.combo("/paste/app.js\0/paste/1770000000/app+v1.0.min.js")
            self.assertEqual("combo path must not contain NUL", str(ctx.exception))

    def test_repeated_paste_construction_does_not_exhaust_loads(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            for _ in range(100):
                paste = Paste(manifest, out, library=library())
                self.assertEqual(paste.base_path, "/paste")


if __name__ == "__main__":
    unittest.main()
