"""Thin loader for the paste-assetgraph native library.

All tag, URL and combo logic lives in the library; this package only calls
it through its C ABI (ctypes — no third-party dependency). Usable from
FastAPI, Jinja, Flask or plain string templates alike.
"""

from __future__ import annotations

import ctypes
import json
import os
import platform
import sys
from dataclasses import dataclass
from typing import Optional


class PasteError(RuntimeError):
    """A paste-assetgraph library call failed; the message is the library's."""


def _platform_dir() -> str:
    if sys.platform == "darwin":
        os_name = "darwin"
    elif sys.platform.startswith("linux"):
        os_name = "linux"
    elif os.name == "nt":
        os_name = "windows"
    else:
        os_name = "unknown"
    arch = {"arm64": "aarch64", "aarch64": "aarch64",
            "x86_64": "amd64", "amd64": "amd64"}.get(
        platform.machine().lower(), "unknown"
    )
    # Linux bundles are per-libc: musl (Alpine) cannot load a glibc build.
    # glibc exports gnu_get_libc_version; musl does not.
    libc = ""
    if os_name == "linux" and not hasattr(
        ctypes.CDLL(None), "gnu_get_libc_version"
    ):
        libc = "-musl"
    return f"{os_name}-{arch}{libc}"


def _library_file_name() -> str:
    if os.name == "nt":
        return "paste_assetgraph_ffi.dll"
    if sys.platform == "darwin":
        return "libpaste_assetgraph_ffi.dylib"
    return "libpaste_assetgraph_ffi.so"


def resolve_library(env=os.environ, package_dir: Optional[str] = None) -> str:
    """Locate the paste-assetgraph native library.

    `PASTE_ASSETGRAPH_LIB`, then the bundled `native-lib/<platform>/<file>`
    under `package_dir` (this package's directory by default), where
    <platform> is `{os}-{arch}` plus `-musl` on musl Linux (a musl process
    cannot load a glibc build). An override that is set — including an
    empty value — but not a regular file raises PasteError naming the
    value; an explicit override never falls through silently. A missing
    library raises PasteError naming both places searched and the platform.
    """
    if package_dir is None:
        package_dir = os.path.dirname(os.path.abspath(__file__))
    override = env.get("PASTE_ASSETGRAPH_LIB")
    if override is not None:
        if os.path.isfile(override):
            return override
        raise PasteError(
            f"PASTE_ASSETGRAPH_LIB is set to {override} but that is not a file"
        )
    platform_dir = _platform_dir()
    bundled = os.path.join(package_dir, "native-lib", platform_dir, _library_file_name())
    if os.path.isfile(bundled):
        return bundled
    raise PasteError(
        f"paste-assetgraph native library not found for platform {platform_dir}. "
        f"Searched: the PASTE_ASSETGRAPH_LIB environment variable and {bundled}"
    )


@dataclass(frozen=True)
class ComboResponse:
    """The library's combo answer, parsed from its camelCase JSON.

    `location` is the canonical URL a non-canonical versioned request
    redirects to — set only on a 302, else None. The host framework writes it
    as the Location header.
    """

    status: int
    content_type: str
    cache_control: str
    cacheable: bool
    location: Optional[str]
    body: str


class Paste:
    """A wrapper around the library for one manifest/output pair.

    Safe to share across request threads: the library holds no per-caller
    state and each `page()` carries its own event state. `library` is an
    explicit path to the shared library; None resolves through
    `resolve_library`.
    """

    def __init__(
        self,
        manifest,
        output_base,
        *,
        output_js=None,
        output_css=None,
        cdn_base: str = "",
        base_path: str = "/paste",
        inline_threshold: int = 1024,
        library: Optional[str] = None,
    ):
        settings = {
            "manifest": os.fspath(manifest),
            "outputBase": os.fspath(output_base),
            "cdnBase": cdn_base,
            "basePath": base_path,
            "inlineThreshold": inline_threshold,
        }
        if output_js:
            settings["outputJs"] = os.fspath(output_js)
        if output_css:
            settings["outputCss"] = os.fspath(output_css)
        self._settings = json.dumps(settings).encode("utf-8")
        self._base_path = base_path

        lib = ctypes.CDLL(resolve_library() if library is None else library)
        lib.paste_render.argtypes = [ctypes.c_char_p, ctypes.c_char_p]
        lib.paste_render.restype = ctypes.c_void_p
        lib.paste_combo.argtypes = [ctypes.c_char_p, ctypes.c_char_p]
        lib.paste_combo.restype = ctypes.c_void_p
        lib.paste_free.argtypes = [ctypes.c_void_p]
        lib.paste_free.restype = None
        self._lib = lib

    @property
    def base_path(self) -> str:
        return self._base_path

    def page(self) -> "PastePage":
        """Start a fresh per-render event list — see PastePage."""
        return PastePage(self)

    def _call(self, fn, argument: bytes) -> str:
        """Call an entry point, read its envelope string, free the buffer."""
        pointer = fn(self._settings, argument)
        if not pointer:
            raise PasteError("paste-assetgraph returned no response")
        try:
            return ctypes.string_at(pointer).decode("utf-8")
        finally:
            self._lib.paste_free(pointer)

    @staticmethod
    def _unwrap(envelope: str):
        """The envelope's `ok` payload, or PasteError carrying `error`."""
        parsed = json.loads(envelope)
        if parsed.get("error") is not None:
            raise PasteError(parsed["error"])
        return parsed["ok"]

    def _render(self, events_json: str) -> str:
        return self._unwrap(
            self._call(self._lib.paste_render, events_json.encode("utf-8"))
        )

    def combo(self, path_and_query: str) -> ComboResponse:
        """Resolve a combo path plus optional query through the library."""
        # The path crosses the ABI as a C string: a NUL would silently
        # truncate it.
        if "\0" in path_and_query:
            raise PasteError("combo path must not contain NUL")
        data = self._unwrap(
            self._call(self._lib.paste_combo, path_and_query.encode("utf-8"))
        )
        return ComboResponse(
            status=data["status"],
            content_type=data["contentType"],
            cache_control=data["cacheControl"],
            cacheable=data["cacheable"],
            location=data.get("location"),
            body=data["body"],
        )


@dataclass(frozen=True)
class _JamEvent:
    """One jam event, frozen at record time."""

    css: tuple
    js: tuple
    inline: bool
    sync: bool
    media: Optional[str]


@dataclass(frozen=True)
class _DumpEvent:
    """One dump event, frozen at record time."""

    asset_type: str


def _encode_events(events: tuple) -> str:
    """Serialize a page's event tuple to the library's render JSON."""
    return json.dumps({"events": [
        {
            "jam": {
                "css": e.css,
                "js": e.js,
                "inline": e.inline,
                "sync": e.sync,
                "media": e.media,
            }
        }
        if isinstance(e, _JamEvent)
        else {"dump": e.asset_type}
        for e in events
    ]})


class PastePage:
    """A single page render's event list.

    Jams arrive from anywhere in a render — nested templates and components
    each add their own — while dumps land where the layout emits them, so the
    page accumulates the render's events. NOT thread-safe: create one per
    render — `paste.page()` — and keep it on the request's thread. `jam` only
    records an event (and returns "" so a template can emit it in place);
    each `dump` renders the full event list so far through the library,
    which is what lets the renderer skip modules an earlier dump already
    delivered.
    """

    def __init__(self, paste: Paste):
        self._paste = paste
        # The page's single mutable cell: an immutable tuple replaced per event.
        self._events: tuple = ()

    def jam(self, css=(), js=(), inline=False, sync=False, media=None) -> str:
        self._events = (*self._events, _JamEvent(
            css=tuple(css),
            js=tuple(js),
            inline=bool(inline),
            sync=bool(sync),
            media=media,
        ))
        return ""

    def dump_css(self) -> str:
        return self._dump("css")

    def dump_js(self) -> str:
        return self._dump("js")

    def _dump(self, asset_type: str) -> str:
        self._events = (*self._events, _DumpEvent(asset_type))
        return self._paste._render(_encode_events(self._events))


__all__ = [
    "ComboResponse",
    "Paste",
    "PasteError",
    "PastePage",
    "resolve_library",
]
