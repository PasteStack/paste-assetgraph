import ctypes
import hashlib
import json
import os
import platform
import sys
import tempfile
import time
import unittest
from pathlib import Path

import paste_assetgraph
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
            "source": "src/paste.js", "output": "/js/paste.h.v1.min.js",
            "hash": "7e9e92b0857c477a6e8aebb0ab2bd924",
            "version": 1, "timestamp": "2026-01-01T00:00:00Z",
            "dependencies": [], "history": [],
        },
        "js/app.js": {
            "source": "src/app.js", "output": "/js/app.h.v1.min.js",
            "hash": "0bdbb2c325525e986d55ef50ca66d056",
            "version": 1, "timestamp": "2026-01-01T00:00:00Z",
            "dependencies": [], "history": [],
        },
        "css/theme.css": {
            "source": "src/theme.scss", "output": "/css/theme.h.v1.min.css",
            "hash": "2c56c360580420d293172f42d85dfbed",
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

            css_tag = page.dump("css")
            self.assertIn("stylesheet", css_tag)
            self.assertIn("site.theme+v1.0", css_tag)

            js_tags = page.dump("js")
            self.assertTrue(js_tags.startswith('<script type="text/javascript">PASTE_RT'))
            self.assertIn("(function(d,r){", js_tags)
            self.assertIn("app+v1.0", js_tags)

            # A complete page seals cleanly.
            self.assertEqual(page.finish(), "")
            self.assertEqual(page.finish(), "")

    def test_a_manifest_rewrite_mid_page_retires_the_page(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()
            page.jam(css=("site.theme",), js=("app",))
            page.dump("css")

            # A build lands between the page's dumps — the manifest's
            # bytes move and so does its generation token (the mtime bump
            # moves the stamp past filesystem granularity — the rewrite
            # is the same length).
            write(manifest, MANIFEST.replace('"version": "42"', '"version": "43"'))
            future = time.time() + 60
            os.utime(manifest, (future, future))

            with self.assertRaises(PasteError) as caught:
                page.dump("js")
            self.assertEqual(caught.exception.kind, "manifest-generation")
            self.assertIn("generation", str(caught.exception))

            # The failure is terminal: every later step answers the
            # recorded error rather than rendering against a manifest
            # the page's earlier dumps were not planned with.
            for step in (
                lambda: page.jam(js=("app",)),
                lambda: page.dump(),
                lambda: page.finish(),
            ):
                with self.assertRaises(PasteError) as again:
                    step()
                self.assertEqual(again.exception.kind, "manifest-generation")

    def test_finish_seals_the_page_and_a_late_jam_fails(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()

            # The css dump already ran when the jam lands: site.theme is
            # queued for a dump that never comes, and finishing on the js
            # dump must refuse — not drop the stylesheet silently.
            page.dump("css")
            page.jam(css=("site.theme",), js=("app",))
            page.dump("js")
            with self.assertRaises(PasteError) as raised:
                page.finish()
            self.assertIn("site.theme", str(raised.exception))

            # A failed finish leaves the page open: deliver the missing
            # dump, then finish — and finishing again is a no-op.
            page.dump("css")
            self.assertEqual(page.finish(), "")
            self.assertEqual(page.finish(), "")

            # A sealed page accepts no more events.
            with self.assertRaises(PasteError) as raised:
                page.jam(js=("app",))
            self.assertIn("finished", str(raised.exception))
            with self.assertRaises(PasteError) as raised:
                page.dump("js")
            self.assertIn("finished", str(raised.exception))

    def test_jam_rejects_scalar_and_non_string_module_names(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()

            # "app" is one name, not three modules 'a','p','p' — a scalar
            # is not a collection, and a collection's members must be
            # strings. Each is a PasteError raised before the event could
            # be recorded.
            for kwargs in (
                {"js": "app"}, {"css": "app"}, {"js": "site.theme"},
                {"js": b"app"}, {"css": bytearray(b"x")},
                {"js": 42}, {"js": ("app", 7)}, {"css": ("app", None)},
            ):
                with self.assertRaises(PasteError, msg=repr(kwargs)):
                    page.jam(**kwargs)

            # Nothing was recorded: a valid jam still dumps and the page
            # finishes — a malformed event cannot strand a usable page.
            page.jam(js=("app",))
            self.assertIn("app+v1.0", page.dump("js"))
            self.assertEqual(page.finish(), "")

    def test_jam_option_types_are_checked_before_recording(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()

            # "false" the string is truthy — coercing it would flip the
            # flag, so the type itself is required. media is a str or
            # None; per_module a bool or None (None means inherit).
            for kwargs, name in (
                ({"js": ("app",), "inline": "false"}, "inline"),
                ({"js": ("app",), "inline": 1}, "inline"),
                ({"js": ("app",), "sync": "yes"}, "sync"),
                ({"js": ("app",), "media": 42}, "media"),
                ({"js": ("app",), "media": {"type": "print"}}, "media"),
                ({"js": ("app",), "per_module": "true"}, "per_module"),
            ):
                with self.assertRaises(PasteError, msg=repr(kwargs)) as caught:
                    page.jam(**kwargs)
                self.assertIn(f"jam {name}", str(caught.exception))

            # Valid spellings — including the None/omitted inherit forms —
            # record and render normally after the rejections: the inline
            # flag inlines the body, so the module lands as its bytes
            # rather than a pinned URL.
            page.jam(js=("app",), inline=True, sync=True, per_module=True)
            page.jam(css=("site.theme",), media="print", per_module=None)
            self.assertIn("APP", page.dump("js"))
            self.assertIn("site.theme+v1.0", page.dump("css"))
            self.assertEqual(page.finish(), "")

    def test_conditional_jam_wraps_tags_and_does_not_mark_js_loaded(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()

            page.jam(js=("app",), condition="lt IE 9")
            first = page.dump("js")
            self.assertIn("<!--[if lt IE 9]>\n", first)
            self.assertIn("\n<![endif]-->", first)
            # A conditional group cannot carry the unconditional bootstrap.
            self.assertNotIn("paste.config", first)

            # js stays unloaded — the unconditional jam serves it again,
            # with the bootstrap this time.
            page.jam(js=("app",))
            second = page.dump("js")
            self.assertIn("paste.config", second)
            self.assertIn("app+v1.0", second)
            self.assertNotIn("<!--[if", second)
            self.assertEqual(page.finish(), "")

    def test_conditional_css_jam_still_marks_loaded(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()

            page.jam(css=("site.theme",), condition="lt IE 9")
            first = page.dump("css")
            self.assertIn("<!--[if lt IE 9]>", first)
            self.assertIn('<link rel="stylesheet"', first)

            # The stylesheet counted as delivered — nothing pending on
            # re-jam.
            page.jam(css=("site.theme",))
            self.assertEqual(page.dump("css"), "")
            self.assertEqual(page.finish(), "")

    def test_jam_condition_type_is_checked_before_recording(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()
            for bad in (42, True, {"if": "IE"}, ["x"]):
                with self.assertRaises(PasteError, msg=repr(bad)) as caught:
                    page.jam(js=("app",), condition=bad)
                self.assertIn("jam condition", str(caught.exception))
            page.jam(js=("app",), condition="IE 8")
            self.assertIn("<!--[if IE 8]>", page.dump("js"))
            self.assertEqual(page.finish(), "")

    def test_jam_records_exactly_the_declared_names_caller_copies_included(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()

            # A list a caller mutates after jam() must not rewrite the
            # recorded event — the page keeps its own materialized copy.
            js = ["app"]
            page.jam(js=js, css=("site.theme",))
            js.append("helper")
            page.dump()
            self.assertEqual(page.finish(), "")

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

    def test_combo_with_a_validator_answers_304_or_200_by_the_recorded_stamp(self):
        # The conditional form is the same library decision with the
        # request's validator forwarded — the fixture group stamps
        # 2026-01-01T00:00:00Z.
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, library=library())
            url = "/paste/1767225600/app+v1.0.min.js"

            # At or after the group stamp → 304, empty body, stamped header.
            for ims in (
                "Thu, 01 Jan 2026 00:00:00 GMT",
                "Thu, 01 Jan 2026 00:00:01 GMT",
            ):
                r = paste.combo(url, if_modified_since=ims)
                self.assertEqual(r.status, 304, ims)
                self.assertEqual(r.body, "")
                self.assertEqual(
                    r.last_modified, "Thu, 01 Jan 2026 00:00:00 GMT"
                )
                self.assertEqual(
                    r.cache_control, "public, max-age=31536000"
                )

            # Before it, absent, or unparsable → the ordinary 200.
            for ims in ("Wed, 01 Jan 2025 00:00:00 GMT", "not a date", None):
                r = paste.combo(url, if_modified_since=ims)
                self.assertEqual(r.status, 200, ims)
                self.assertEqual(r.body, "APP")
                self.assertEqual(
                    r.last_modified, "Thu, 01 Jan 2026 00:00:00 GMT"
                )

            # A non-canonical URL still redirects first — the validator
            # never suppresses canonicalization.
            r = paste.combo(
                "/paste/1767225599/app+v1.0.min.js",
                if_modified_since="Thu, 01 Jan 2026 00:00:01 GMT",
            )
            self.assertEqual(r.status, 302)
            self.assertEqual(r.location, url)

            # A NUL in the validator is rejected at the adapter.
            with self.assertRaises(PasteError):
                paste.combo(url, if_modified_since="Thu\0")

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
            self.assertIn(f"app+v1.0-h{app_hash}", page.dump("js"))

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
            tags = page.dump("js")
            self.assertIn("app+v1.0.min.js", tags)
            self.assertNotIn("-h", tags)

    def test_render_naming_an_unknown_module_raises_paste_error_naming_it(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()
            page.jam(js=("ghost",))
            with self.assertRaises(PasteError) as ctx:
                page.dump("js")
            self.assertIn("ghost", str(ctx.exception))


class PastePageTest(unittest.TestCase):
    def test_dump_with_no_argument_renders_every_type(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())

            reference = paste.page()
            reference.jam(css=("site.theme",), js=("app",))
            expected = reference.dump("css") + "\n" + reference.dump("js")

            page = paste.page()
            page.jam(css=("site.theme",), js=("app",))
            self.assertEqual(page.dump(), expected)
            self.assertEqual(page.finish(), "")

    def test_dump_with_a_bad_type_rejects_before_any_native_call(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()
            page.jam(js=("app",))

            # 'all' is the no-argument spelling; as a value it is invalid.
            for bad in ("all", "css js", "style", 0):
                with self.assertRaises(PasteError) as ctx:
                    page.dump(bad)
                self.assertIn("dump takes", str(ctx.exception))

            # Nothing was recorded: the jam still dumps and finishes.
            self.assertIn("app+v1.0", page.dump("js"))
            self.assertEqual(page.finish(), "")

    def test_per_module_emits_per_module_tags_and_a_jam_overrides_it(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-pm-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(
                manifest, out, inline_threshold=0, per_module=True,
                library=library(),
            )

            # Global on: the dump names each module's versioned file —
            # one ordered loader block per async group, no combo token.
            page = paste.page()
            page.jam(js=("app",))
            tags = page.dump("js")
            self.assertIn("/js/app.h.v1.min.js", tags)
            self.assertIn("paste.require", tags)
            self.assertNotIn("app+v1.0", tags)
            self.assertEqual(page.finish(), "")

            # per_module=False on a jam renders the combo group the
            # setting would suppress — the override goes both ways.
            combo = paste.page()
            combo.jam(js=("app",), per_module=False)
            tags = combo.dump("js")
            self.assertIn("app+v1.0", tags)
            self.assertNotIn("/js/app.h.v1.min.js", tags)
            self.assertEqual(combo.finish(), "")

            # And per_module=True flips a combo-setting page.
            combo_setting = Paste(manifest, out, inline_threshold=0,
                                  library=library())
            flipped = combo_setting.page()
            flipped.jam(js=("app",), per_module=True)
            self.assertIn("/js/app.h.v1.min.js", flipped.dump("js"))
            self.assertEqual(flipped.finish(), "")

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

            js_tags = page.dump("js")
            self.assertIn("app+v1.0", js_tags)
            self.assertNotIn("ghost", js_tags)

    def test_jam_rejects_non_boolean_flags_without_recording(self):
        # bool("false") is True — coercion would silently flip the flag a
        # caller mistyped. jam requires real booleans and a rejected call
        # must leave the page's event list untouched.
        with tempfile.TemporaryDirectory(prefix="paste-py-bool-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, inline_threshold=0, library=library())
            page = paste.page()

            for bad_call in (
                dict(inline="false"),
                dict(inline=1),
                dict(inline="yes"),
                dict(sync="false"),
                dict(sync=0),
                dict(per_module="true"),
                dict(per_module=1),
            ):
                with self.assertRaises(PasteError) as ctx:
                    page.jam(js=("app",), **bad_call)
                self.assertIn("must be a bool", str(ctx.exception))

            # Nothing was recorded: the valid jam still dumps and
            # finishes — the rejected calls changed no page state.
            page.jam(js=("app",))
            self.assertIn("app+v1.0", page.dump("js"))
            self.assertEqual(page.finish(), "")


class PasteConstructionTest(unittest.TestCase):
    def test_primitive_settings_are_validated_before_the_library_loads(self):
        # A mistyped flag or threshold used to sail through __init__ and
        # surface mid-render inside the native deserializer. Every bad
        # value is a PasteError naming its option, and a deliberately
        # absent library path proves the rejection precedes the load.
        nowhere = os.path.join("definitely", "absent", "native.so")
        for kwargs, name in (
            (dict(per_module="false"), "per_module"),
            (dict(per_module=1), "per_module"),
            (dict(per_module=None), "per_module"),
            (dict(inline_threshold=-1), "inline_threshold"),
            (dict(inline_threshold=1.5), "inline_threshold"),
            (dict(inline_threshold="1024"), "inline_threshold"),
            (dict(inline_threshold=True), "inline_threshold"),
            (dict(base_path=None), "base_path"),
            (dict(base_path=42), "base_path"),
            (dict(cdn_base=None), "cdn_base"),
            (dict(cdn_base=False), "cdn_base"),
        ):
            with self.assertRaises(PasteError, msg=repr(kwargs)) as ctx:
                Paste("/x/manifest.json", "/x/public", library=nowhere, **kwargs)
            self.assertIn(name, str(ctx.exception))

    def test_output_overrides_distinguish_absent_from_empty(self):
        # None means "inherit <output_base>/{js,css}"; an explicitly
        # unusable override — "", whitespace, a non-path, a NUL — must
        # fail at the call, never silently select the default. The
        # settings names reported are the wire's camelCase keys.
        nowhere = os.path.join("definitely", "absent", "native.so")
        for key, reported in (
            ("output_js", "outputJs"),
            ("output_css", "outputCss"),
        ):
            for bad in ("", "   ", 42, True, b"/x", "/x\0y"):
                with self.assertRaises(PasteError, msg=f"{key}={bad!r}") as ctx:
                    Paste(
                        "/x/manifest.json", "/x/public",
                        library=nowhere, **{key: bad},
                    )
                self.assertIn(reported, str(ctx.exception))

    def test_output_overrides_accept_pathlike_and_select_the_directory(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            # Each override replaces the whole <output_base>/{js,css}
            # directory. The integrity-checked bytes live only in the
            # custom dirs while a byte trap sits at the defaults —
            # silently resolving a default would read the wrong bytes and
            # fail the recorded hash.
            custom_js = Path(root) / "custom-js"
            custom_css = Path(root) / "custom-css"
            write(str(custom_js / "paste.h.v1.min.js"), "PASTE_RT")
            write(str(custom_js / "app.h.v1.min.js"), "APP")
            write(str(custom_css / "theme.h.v1.min.css"), "CSS")
            write(os.path.join(out, "js", "paste.h.v1.min.js"), "TRAP")
            write(os.path.join(out, "js", "app.h.v1.min.js"), "TRAP")
            write(os.path.join(out, "css", "theme.h.v1.min.css"), "TRAP")

            paste = Paste(
                manifest, out,
                output_js=custom_js, output_css=custom_css,
                library=library(),
            )
            page = paste.page()
            page.jam(js=("app",), css=("site.theme",), inline=True)
            self.assertIn("APP", page.dump("js"))
            self.assertIn("CSS", page.dump("css"))
            self.assertEqual(page.finish(), "")


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

    def test_a_bare_override_resolves_against_the_working_directory(self):
        relative = os.path.relpath(library())
        resolved = resolve_library({"PASTE_ASSETGRAPH_LIB": relative})
        self.assertTrue(os.path.isabs(resolved), resolved)

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
            arch = paste_assetgraph._interpreter_arch()
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

    def test_combo_rejects_a_non_string_path_before_the_native_call(self):
        # A None, number, list, dict or bytes value used to die as a
        # TypeError inside the NUL check — the contract is a PasteError,
        # and nothing is coerced.
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            paste = Paste(manifest, out, library=library())
            for bad in (None, 42, ["/x"], {"path": "/x"}, True, b"/x"):
                with self.assertRaises(PasteError, msg=repr(bad)) as ctx:
                    paste.combo(bad)
                self.assertIn("combo", str(ctx.exception))

    def test_repeated_paste_construction_does_not_exhaust_loads(self):
        with tempfile.TemporaryDirectory(prefix="paste-py-") as root:
            manifest, out = e2e_fixture(root)
            for _ in range(100):
                paste = Paste(manifest, out, library=library())
                self.assertEqual(paste.base_path, "/paste")


class SharedFixtureTest(unittest.TestCase):
    """The shared CLI-built fixture every runner checks: one built
    distribution whose frozen reference.json holds the expected response
    and page-output digests. This proves adapter consistency against the
    same compiled bytes — not legacy parity."""

    def test_shared_cli_distribution(self):
        location = os.environ.get('PASTE_ASSETGRAPH_TEST_FIXTURE')
        self.assertTrue(
            location,
            'Build tests/build_shared_fixture.py once and set '
            'PASTE_ASSETGRAPH_TEST_FIXTURE',
        )
        root = Path(location).resolve(strict=True)

        # Fixture metadata paths must resolve beneath the selected root:
        # absolute members, '..', and symlink escapes are rejected. The
        # operator-chosen root itself is the intended input.
        def fixture_path(relative):
            if Path(relative).is_absolute():
                raise ValueError('fixture paths must be relative')
            candidate = (root / relative).resolve(strict=True)
            candidate.relative_to(root)
            return candidate

        reference = json.loads(
            fixture_path('reference.json').read_text(encoding='utf-8')
        )
        self.assertEqual(reference['schema'], 1)
        with self.assertRaises(ValueError):
            fixture_path('..')

        def sha(value):
            data = value.encode('utf-8') if isinstance(value, str) else value
            return hashlib.sha256(data).hexdigest()

        def tree(directory):
            return {
                p.relative_to(directory).as_posix(): sha(p.read_bytes())
                for p in directory.rglob('*')
                if p.is_file()
            }

        for item in reference['cases']:
            with self.subTest(layout=item['name']):
                output = fixture_path(item['outputBase'])
                self.assertEqual(tree(output), item['files'])
                paste = Paste(
                    fixture_path(item['manifest']), output, inline_threshold=0
                )
                for request in item['responses']:
                    actual = paste.combo(request['path'])
                    self.assertEqual(
                        {
                            'status': actual.status,
                            'contentType': actual.content_type,
                            'cacheControl': actual.cache_control,
                            'cacheable': actual.cacheable,
                            'location': actual.location,
                            'lastModified': actual.last_modified,
                            'bodySha256': sha(actual.body),
                        },
                        request['expected'],
                        request['name'],
                    )
                for scenario in item['pages']:
                    with tempfile.TemporaryDirectory(
                        prefix='paste-shared-'
                    ) as temporary:
                        manifest = Path(temporary) / 'manifest.json'
                        held = Path(temporary) / 'held.json'
                        # The scenario's recorded manifest default, applied
                        # only when it names one — otherwise the fixture
                        # bytes are copied as-is.
                        cdn_base = scenario.get('manifestCdnBase')
                        if cdn_base is not None:
                            manifest_json = json.loads(
                                fixture_path(item['manifest']).read_text(
                                    encoding='utf-8'
                                )
                            )
                            manifest_json['cdn_base'] = cdn_base
                            manifest.write_text(
                                json.dumps(manifest_json), encoding='utf-8'
                            )
                        else:
                            manifest.write_bytes(
                                fixture_path(item['manifest']).read_bytes()
                            )
                        # Only the known render settings are mapped through.
                        settings = scenario.get('settings') or {}
                        page = Paste(
                            manifest,
                            output,
                            inline_threshold=settings.get('inlineThreshold', 0),
                            cdn_base=settings.get('cdnBase', ''),
                            per_module=settings.get('perModule', False),
                        ).page()
                        for step in scenario['steps']:
                            if step.get('hideManifest'):
                                manifest.rename(held)
                                continue
                            if step.get('restoreManifest'):
                                held.rename(manifest)
                                continue

                            def operation():
                                if 'jam' in step:
                                    # The fixture spells the override in
                                    # JSON camelCase; the Python wrapper's
                                    # kwarg is per_module.
                                    jam = {
                                        ('per_module' if k == 'perModule' else k): v
                                        for k, v in step['jam'].items()
                                    }
                                    return page.jam(**jam)
                                if step.get('finish'):
                                    return page.finish()
                                dump = step['dump']
                                return page.dump(None if dump == 'all' else dump)

                            if step.get('error') or step.get('errorContains'):
                                with self.assertRaises(PasteError) as raised:
                                    operation()
                                if step.get('errorContains'):
                                    self.assertIn(
                                        step['errorContains'],
                                        str(raised.exception),
                                    )
                            else:
                                self.assertEqual(
                                    sha(operation()),
                                    step['valueSha256'],
                                    scenario['name'],
                                )
                self.assertEqual(tree(output), item['files'])


if __name__ == "__main__":
    unittest.main()
