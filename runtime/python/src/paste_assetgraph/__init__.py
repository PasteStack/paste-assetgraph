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
import sysconfig
from dataclasses import dataclass
from typing import Optional, Sequence


class PasteError(RuntimeError):
    """A paste-assetgraph library call failed; the message is the library's.

    `kind` carries the library's machine classification when one failure
    must be told apart from the rest — currently "manifest-generation",
    a manifest rewrite that retired a page mid-render.
    """

    kind = None


def _platform_dir() -> str:
    if sys.platform == "darwin":
        os_name = "darwin"
    elif sys.platform.startswith("linux"):
        os_name = "linux"
    elif os.name == "nt":
        os_name = "windows"
    else:
        os_name = "unknown"
    arch = _interpreter_arch()
    # Linux bundles are per-libc: musl (Alpine) cannot load a glibc build.
    # glibc exports gnu_get_libc_version; musl does not.
    libc = ""
    if os_name == "linux" and not hasattr(
        ctypes.CDLL(None), "gnu_get_libc_version"
    ):
        libc = "-musl"
    return f"{os_name}-{arch}{libc}"


def _interpreter_arch() -> str:
    """The running interpreter's architecture — what the library must match.

    `platform.machine()` reports the hardware: x64 Python on Windows ARM64
    answers "ARM64", and a 32-bit interpreter on a 64-bit kernel answers the
    kernel's arch, so neither can pick the loadable build.
    """
    if sys.maxsize <= 2**32:
        return "unknown"
    tail = sysconfig.get_platform().rsplit("-", 1)[-1].lower()
    if tail in ("universal2", "intel", "fat", "fat3", "fat64"):
        tail = platform.machine().lower()
    return {"arm64": "aarch64", "aarch64": "aarch64",
            "x86_64": "amd64", "amd64": "amd64"}.get(tail, "unknown")


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
        # Absolute before both the check and the load: ctypes/dlopen searches
        # the library path for a bare name, so the file checked relative to
        # the working directory need not be the one that loads.
        if override:
            override = os.path.abspath(override)
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
    last_modified: Optional[str]
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
        per_module: bool = False,
        library: Optional[str] = None,
    ):
        # Primitive types are checked before the library is loaded: a
        # mistyped flag or threshold ("false", -1) would otherwise
        # surface mid-render at the native deserializer instead of at
        # the call that supplied it. What makes a base_path or cdn_base
        # *valid* — the URL grammar — stays with the library; only the
        # type is checked here.
        if not isinstance(per_module, bool):
            raise PasteError(
                f"Paste per_module must be a bool — got {per_module!r}"
            )
        if (
            not isinstance(inline_threshold, int)
            or isinstance(inline_threshold, bool)
            or inline_threshold < 0
        ):
            raise PasteError(
                "Paste inline_threshold must be a non-negative integer "
                f"— got {inline_threshold!r}"
            )
        for name, value in (("base_path", base_path), ("cdn_base", cdn_base)):
            if not isinstance(value, str):
                raise PasteError(
                    f"Paste {name} must be a str — got {value!r}"
                )

        try:
            manifest_path = os.fspath(manifest)
            output_base_path = os.fspath(output_base)
        except TypeError:
            raise PasteError(
                "Paste manifest and output_base must be filesystem paths — "
                f"got {manifest!r}, {output_base!r}"
            ) from None
        if (
            not isinstance(manifest_path, str)
            or manifest_path.strip() == ""
            or "\0" in manifest_path
        ):
            raise PasteError(
                f"Paste manifest must be a non-empty path — got {manifest!r}"
            )
        if (
            not isinstance(output_base_path, str)
            or output_base_path.strip() == ""
            or "\0" in output_base_path
        ):
            raise PasteError(
                f"Paste output_base must be a non-empty path — got {output_base!r}"
            )
        settings = {
            "manifest": manifest_path,
            "outputBase": output_base_path,
            "cdnBase": cdn_base,
            "basePath": base_path,
            "inlineThreshold": inline_threshold,
            "perModule": per_module,
        }
        # output_js/output_css distinguish None — inherit
        # <output_base>/{js,css} — from an explicitly unusable override:
        # an env-var expansion producing "" must fail loudly, never
        # silently select the default directory.
        for name, value in (("outputJs", output_js), ("outputCss", output_css)):
            if value is None:
                continue
            try:
                rendered = os.fspath(value)
            except TypeError:
                rendered = value
            if (
                not isinstance(rendered, str)
                or rendered.strip() == ""
                or "\0" in rendered
            ):
                raise PasteError(
                    f"Paste {name} must be a non-empty directory path or None "
                    f"— got {value!r}"
                )
            settings[name] = rendered
        self._settings = json.dumps(settings).encode("utf-8")
        self._base_path = base_path

        lib = ctypes.CDLL(resolve_library() if library is None else library)
        lib.paste_render_page.argtypes = [
            ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p
        ]
        lib.paste_render_page.restype = ctypes.c_void_p
        lib.paste_combo.argtypes = [ctypes.c_char_p, ctypes.c_char_p]
        lib.paste_combo.restype = ctypes.c_void_p
        lib.paste_combo_conditional.argtypes = [
            ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p
        ]
        lib.paste_combo_conditional.restype = ctypes.c_void_p
        lib.paste_free.argtypes = [ctypes.c_void_p]
        lib.paste_free.restype = None
        self._lib = lib

    @property
    def base_path(self) -> str:
        return self._base_path

    def page(self) -> "PastePage":
        """Start a fresh per-render event list — see PastePage."""
        return PastePage(self)

    def _call(self, fn, *arguments: bytes) -> str:
        """Call an entry point, read its envelope string, free the buffer."""
        pointer = fn(self._settings, *arguments)
        if not pointer:
            raise PasteError("paste-assetgraph returned no response")
        try:
            return ctypes.string_at(pointer).decode("utf-8")
        finally:
            self._lib.paste_free(pointer)

    @staticmethod
    def _unwrap(envelope: str):
        """The envelope's `ok` payload, or PasteError carrying `error` —
        plus the machine `kind` when the library marks one."""
        parsed = json.loads(envelope)
        if parsed.get("error") is not None:
            error = PasteError(parsed["error"])
            if parsed.get("kind") is not None:
                error.kind = parsed["kind"]
            raise error
        return parsed["ok"]

    def _render_page(self, events_json: str, generation: str):
        """One page step: posts the page's generation token, returns the
        step's (html, generation) — the answer's token replaces the
        page's stored one."""
        data = self._unwrap(
            self._call(
                self._lib.paste_render_page,
                events_json.encode("utf-8"),
                generation.encode("utf-8"),
            )
        )
        return data["html"], data["generation"]

    def combo(
        self, path_and_query: str, if_modified_since: Optional[str] = None
    ) -> ComboResponse:
        """Resolve a combo path plus optional query through the library.

        `if_modified_since` hands the request's validator to the same
        Rust decision — a 304 arrives with an empty body."""
        # The path crosses the ABI as a C string: a NUL would silently
        # truncate it. A non-str caller gets a PasteError, not a
        # TypeError — and nothing is coerced into one.
        if not isinstance(path_and_query, str):
            raise PasteError(
                f"combo takes a path str — got {path_and_query!r}"
            )
        if "\0" in path_and_query:
            raise PasteError("combo path must not contain NUL")
        if if_modified_since is None:
            data = self._unwrap(
                self._call(
                    self._lib.paste_combo, path_and_query.encode("utf-8")
                )
            )
        else:
            if not isinstance(if_modified_since, str):
                raise PasteError(
                    "combo if_modified_since takes a header str — "
                    f"got {if_modified_since!r}"
                )
            if "\0" in if_modified_since:
                raise PasteError(
                    "combo if_modified_since must not contain NUL"
                )
            data = self._unwrap(
                self._call(
                    self._lib.paste_combo_conditional,
                    path_and_query.encode("utf-8"),
                    if_modified_since.encode("utf-8"),
                )
            )
        return ComboResponse(
            status=data["status"],
            content_type=data["contentType"],
            cache_control=data["cacheControl"],
            cacheable=data["cacheable"],
            location=data.get("location"),
            last_modified=data.get("lastModified"),
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
    condition: Optional[str]
    per_module: Optional[bool]


@dataclass(frozen=True)
class _DumpEvent:
    """One dump event, frozen at record time."""

    asset_type: str


@dataclass(frozen=True)
class _FinishEvent:
    """The terminal finish marker, frozen at record time."""


def _encode_events(events: Sequence) -> str:
    """Serialize a page's event records to the library's render JSON."""
    def encode(e):
        if isinstance(e, _JamEvent):
            jam = {
                "css": e.css,
                "js": e.js,
                "inline": e.inline,
                "sync": e.sync,
                "media": e.media,
            }
            # condition and perModule are posted only when set — absent
            # means an unconditional group / inherit the render setting.
            if e.condition is not None:
                jam["condition"] = e.condition
            if e.per_module is not None:
                jam["perModule"] = e.per_module
            return {"jam": jam}
        if isinstance(e, _DumpEvent):
            return {"dump": e.asset_type}
        return "finish"  # _FinishEvent encodes as the bare string

    return json.dumps({"events": [encode(e) for e in events]})


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

    `finish` seals the page after the last dump: the library verifies every
    jammed module was emitted — a jam queued after its type's final dump is
    a PasteError, not a silently dropped tag — and further jams/dumps
    raise. A failed `finish` leaves the page open so the caller can add the
    missing dump and retry — unless the library reported a manifest
    generation change, which retires the page permanently: every later
    call raises the recorded error.
    """

    def __init__(self, paste: Paste):
        self._paste = paste
        # Private append-only event log — a list so recording stays O(1)
        # per event; snapshots are taken by the render calls themselves.
        self._events: list = []
        # The manifest generation the render calls are pinned to: "" asks
        # for whatever the manifest is; every answer carries the token it
        # was planned against and each later call posts it back — a build
        # landing mid-page can never mix two manifests' module maps.
        self._generation = ""
        # A manifest-generation error retires the page permanently —
        # earlier dumps were planned against a manifest that no longer
        # exists, so every later call fails with it.
        self._invalid: Optional[PasteError] = None

    @property
    def _finished(self) -> bool:
        return bool(self._events) and isinstance(self._events[-1], _FinishEvent)

    def _require_open(self) -> None:
        if self._invalid is not None:
            raise self._invalid
        if self._finished:
            raise PasteError("page is finished — no more jams or dumps")

    def _render(self, events_json: str) -> str:
        try:
            html, generation = self._paste._render_page(
                events_json, self._generation
            )
        except PasteError as error:
            if error.kind == "manifest-generation":
                self._invalid = error
            raise
        self._generation = generation
        return html

    def jam(
        self,
        css=(),
        js=(),
        inline=False,
        sync=False,
        media=None,
        condition=None,
        per_module=None,
    ) -> str:
        self._require_open()
        # Booleans are required, not coerced — bool("false") is True, so a
        # string passed by mistake must fail rather than flip the flag.
        # Every check runs before the event is appended: a rejected jam
        # changes no page state, so a later valid jam on the same page is
        # the straightforward recovery.
        for name, value in (("inline", inline), ("sync", sync)):
            if not isinstance(value, bool):
                raise PasteError(
                    f"jam {name} must be a bool — got {value!r}"
                )
        if per_module is not None and not isinstance(per_module, bool):
            raise PasteError(
                f"jam per_module must be a bool or None — got {per_module!r}"
            )
        if media is not None and not isinstance(media, str):
            raise PasteError(
                f"jam media must be a str or None — got {media!r}"
            )
        if condition is not None and not isinstance(condition, str):
            raise PasteError(
                f"jam condition must be a str or None — got {condition!r}"
            )

        def module_list(value, name):
            # A scalar name is not a collection: tuple("app") would record
            # the three letters as three modules. Strings and bytes are
            # iterable in exactly that wrong way, so they fail by type.
            if isinstance(value, (str, bytes, bytearray)):
                raise PasteError(
                    f"jam {name} must be a collection of module names — got {value!r}"
                )
            try:
                entries = tuple(value)
            except TypeError:
                raise PasteError(
                    f"jam {name} must be a collection of module names — got {value!r}"
                ) from None
            for entry in entries:
                if not isinstance(entry, str):
                    raise PasteError(
                        f"jam {name} entries must be strings — got {entry!r}"
                    )
            return entries

        self._events.append(_JamEvent(
            css=module_list(css, "css"),
            js=module_list(js, "js"),
            inline=inline,
            sync=sync,
            media=media,
            condition=condition,
            per_module=per_module,
        ))
        return ""

    def dump(self, asset_type: Optional[str] = None) -> str:
        """Dump pending modules: one type for "css"/"js", every type for
        no argument. Anything else raises PasteError before any native call
        or event mutation."""
        if asset_type is None:
            scope = "all"
        elif asset_type in ("css", "js"):
            scope = asset_type
        else:
            raise PasteError(
                f"dump takes 'css', 'js', or no argument — got {asset_type!r}"
            )
        return self._dump(scope)

    def finish(self) -> str:
        """Seal the page: every jammed module must have been dumped.

        Returns "" — usable at the end of a template the way dumps are —
        and is a no-op on an already-finished page. A failed finish leaves
        the page open so the missing dump can still be added.
        """
        if self._invalid is not None:
            raise self._invalid
        if self._finished:
            return ""
        self._events.append(_FinishEvent())
        try:
            self._render(_encode_events(self._events))
        except Exception:
            # A failed finish leaves the page open — retract the event.
            self._events.pop()
            raise
        return ""

    def _dump(self, asset_type: str) -> str:
        self._require_open()
        # Commit the dump event only when the render succeeds — a failed
        # dump must not count as delivery.
        self._events.append(_DumpEvent(asset_type))
        try:
            return self._render(_encode_events(self._events))
        except Exception:
            self._events.pop()
            raise


__all__ = [
    "ComboResponse",
    "Paste",
    "PasteError",
    "PastePage",
    "resolve_library",
]
