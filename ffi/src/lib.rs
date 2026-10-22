//! The C ABI the language runtimes call. Four entry points take UTF-8
//! JSON strings and return a heap C string holding a JSON envelope —
//! `{"ok": ...}` on success or `{"error": "<message>"}` on any failure,
//! with an optional `"kind"` marking errors an adapter must distinguish:
//!
//! - `paste_render` — the classic per-jam markup for an event list
//! - `paste_render_page` — one page's complete markup state
//! - `paste_combo` — a combo response for a URL
//! - `paste_combo_conditional` — `paste_combo` plus the request's
//!   `If-Modified-Since`; the library decides 200 vs 304
//!
//! Every pointer these four return is heap memory owned by this library:
//! it must be released exactly once with `paste_free` — a null pointer is
//! a no-op — and never freed through the caller's own allocator.
//! Returned strings stay valid until that call; copy out before freeing.

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use paste_assetgraph::api::{self, Settings};

/// The error half of an envelope: the message, plus the machine `kind`
/// an adapter matches on when one failure must be told apart from the
/// rest.
struct CallFailure {
    message: String,
    kind: Option<&'static str>,
}

impl From<String> for CallFailure {
    fn from(message: String) -> Self {
        CallFailure {
            message,
            kind: None,
        }
    }
}

/// One FFI call's outcome: `{"ok": <json>}` or `{"error": "<message>"}`
/// — plus `"kind"` when the failure carries one.
fn envelope(result: Result<serde_json::Value, CallFailure>) -> *mut c_char {
    let body = match result {
        Ok(value) => serde_json::json!({ "ok": value }),
        Err(failure) => {
            let mut body = serde_json::json!({ "error": failure.message });
            if let Some(kind) = failure.kind {
                body["kind"] = serde_json::json!(kind);
            }
            body
        }
    };
    let text = serde_json::to_string(&body)
        .unwrap_or_else(|_| "{\"error\":\"envelope serialization failed\"}".to_string());
    // serde_json output can never contain a NUL byte, so the only failure
    // is unreachable; still, a null return is an error a caller can show.
    match CString::new(text) {
        Ok(text) => text.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

/// Read a caller's C string as UTF-8. Null and malformed input become
/// envelope errors rather than undefined behaviour.
fn read_str<'a>(ptr: *const c_char, what: &str) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err(format!("{what} is a null pointer"));
    }
    // Safety: non-null checked above; the caller's contract supplies a
    // NUL-terminated string.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| format!("{what} is not valid UTF-8"))
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Run `f` never letting a panic cross the FFI boundary: a Rust panic
/// unwinding into a C caller is undefined behaviour, so every call is
/// caught and reported in the envelope instead.
fn guarded<F>(f: F) -> Result<serde_json::Value, CallFailure>
where
    F: FnOnce() -> Result<serde_json::Value, CallFailure>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => Err(CallFailure::from(format!(
            "paste-assetgraph panicked: {}",
            panic_message(payload)
        ))),
    }
}

fn parse_settings(json: &str) -> Result<Settings, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid settings JSON: {e}"))
}

/// Render the event list `events_json` against `settings_json`; the
/// envelope's `ok` is the last dump's HTML string. An event list ending in
/// `"finish"` is a completion check instead — it validates that every
/// jammed module was dumped and `ok` is an empty string.
///
/// # Safety
/// `settings_json` and `events_json` must be null or point to valid
/// NUL-terminated UTF-8 strings that outlive the call. The returned pointer
/// is heap-allocated and must be freed exactly once with `paste_free`.
#[no_mangle]
pub unsafe extern "C" fn paste_render(
    settings_json: *const c_char,
    events_json: *const c_char,
) -> *mut c_char {
    envelope(guarded(|| {
        let settings = parse_settings(read_str(settings_json, "settings")?)?;
        let events = read_str(events_json, "events")?;
        let html = api::render(&settings, events).map_err(|e| format!("{e:#}"))?;
        Ok(serde_json::Value::String(html))
    }))
}

/// Render one page step against `settings_json`, pinned to the
/// `generation` token an earlier `paste_render_page` call issued — an
/// empty string asks for whatever generation the manifest currently is.
/// The envelope's `ok` is `{"html": "...", "generation": "<token>"}`;
/// a generation mismatch answers
/// `{"error": "...", "kind": "manifest-generation"}` so an adapter can
/// retire the page rather than serve markup mixed across two builds.
///
/// # Safety
/// `settings_json`, `events_json` and `generation` must be null or point
/// to valid NUL-terminated UTF-8 strings that outlive the call. The
/// returned pointer is heap-allocated and must be freed exactly once with
/// `paste_free`.
#[no_mangle]
pub unsafe extern "C" fn paste_render_page(
    settings_json: *const c_char,
    events_json: *const c_char,
    generation: *const c_char,
) -> *mut c_char {
    envelope(guarded(|| {
        let settings = parse_settings(read_str(settings_json, "settings")?)?;
        let events = read_str(events_json, "events")?;
        // NULL means "no token yet", exactly like the empty string — the
        // same convention `paste_combo_conditional` applies to its validator.
        let expected = if generation.is_null() {
            None
        } else {
            let generation = read_str(generation, "generation")?;
            (!generation.is_empty()).then_some(generation)
        };
        match api::render_page(&settings, events, expected) {
            Ok(page) => Ok(serde_json::json!({
                "html": page.html,
                "generation": page.generation,
            })),
            Err(e) => Err(CallFailure {
                message: format!("{e:#}"),
                kind: e
                    .downcast_ref::<api::GenerationMismatch>()
                    .map(|_| "manifest-generation"),
            }),
        }
    }))
}

/// Resolve the combo URL `path_and_query` against `settings_json`; the
/// envelope's `ok` is the `ComboResponse` object.
///
/// # Safety
/// `settings_json` and `path_and_query` must be null or point to valid
/// NUL-terminated UTF-8 strings that outlive the call. The returned pointer
/// is heap-allocated and must be freed exactly once with `paste_free`.
#[no_mangle]
pub unsafe extern "C" fn paste_combo(
    settings_json: *const c_char,
    path_and_query: *const c_char,
) -> *mut c_char {
    envelope(guarded(|| {
        let settings = parse_settings(read_str(settings_json, "settings")?)?;
        let path_and_query = read_str(path_and_query, "pathAndQuery")?;
        let response = api::combo(&settings, path_and_query).map_err(|e| format!("{e:#}"))?;
        Ok(serde_json::to_value(&response).map_err(|e| format!("{e:#}"))?)
    }))
}

/// `paste_combo` with the request's `If-Modified-Since` value — a NULL or
/// empty `if_modified_since` behaves exactly like `paste_combo`. The
/// response is the same `ComboResponse` shape plus its `lastModified`
/// field; a 304 carries an empty body.
///
/// # Safety
/// `settings_json`, `path_and_query`, and `if_modified_since` must be null
/// or point to valid NUL-terminated UTF-8 strings that outlive the call.
/// The returned pointer is heap-allocated and must be freed exactly once
/// with `paste_free`.
#[no_mangle]
pub unsafe extern "C" fn paste_combo_conditional(
    settings_json: *const c_char,
    path_and_query: *const c_char,
    if_modified_since: *const c_char,
) -> *mut c_char {
    envelope(guarded(|| {
        let settings = parse_settings(read_str(settings_json, "settings")?)?;
        let path_and_query = read_str(path_and_query, "pathAndQuery")?;
        let validator = if if_modified_since.is_null() {
            None
        } else {
            read_str(if_modified_since, "ifModifiedSince").map(|h| (!h.is_empty()).then_some(h))?
        };
        let response = api::combo_conditional(&settings, path_and_query, validator)
            .map_err(|e| format!("{e:#}"))?;
        Ok(serde_json::to_value(&response).map_err(|e| format!("{e:#}"))?)
    }))
}

/// Release a heap string this library allocated.
///
/// # Safety
/// `ptr` must be null (a no-op) or a pointer previously returned by one
/// of the four allocating calls — `paste_render`, `paste_render_page`,
/// `paste_combo`, or `paste_combo_conditional` — freed at most once and
/// never through the caller's own allocator (`free`/`kfree`/etc.). The
/// string stays valid until this call; copy its contents out first.
#[no_mangle]
pub unsafe extern "C" fn paste_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        // Safety: per the caller's contract this pointer came from
        // CString::into_raw above and has not been freed before.
        drop(unsafe { CString::from_raw(ptr) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cstring(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    /// Read and free an FFI return value.
    fn take(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        let text = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
        unsafe { paste_free(ptr) };
        text
    }

    fn fixture(root: &std::path::Path) -> String {
        std::fs::create_dir_all(root.join("public/js")).unwrap();
        std::fs::write(root.join("public/js/paste.h.v1.min.js"), "PASTE_RT").unwrap();
        std::fs::write(root.join("public/js/app.h.v1.min.js"), "APP").unwrap();
        let manifest = root.join("manifest.json");
        std::fs::write(
            &manifest,
            r#"{
                "version": "42",
                "generated": "2026-01-01T00:00:00Z",
                "modules": { "paste": "js/paste.js", "app": "js/app.js" },
                "assets": {
                    "js/paste.js": {
                        "source": "src/paste.js",
                        "output": "/js/paste.h.v1.min.js",
                        "hash": "7e9e92b0857c477a6e8aebb0ab2bd924",
                        "version": 1,
                        "timestamp": "2026-01-01T00:00:00Z",
                        "dependencies": [],
                        "history": []
                    },
                    "js/app.js": {
                        "source": "src/app.js",
                        "output": "/js/app.h.v1.min.js",
                        "hash": "0bdbb2c325525e986d55ef50ca66d056",
                        "version": 1,
                        "timestamp": "2026-01-01T00:00:00Z",
                        "dependencies": [],
                        "history": []
                    }
                },
                "sorted_deps": {}
            }"#,
        )
        .unwrap();
        serde_json::json!({
            "manifest": manifest.to_str().unwrap(),
            "outputBase": root.join("public").to_str().unwrap(),
            "inlineThreshold": 0
        })
        .to_string()
    }

    #[test]
    fn render_returns_the_ok_envelope() {
        let root = tempfile::tempdir().unwrap();
        let settings = cstring(&fixture(root.path()));
        let events = cstring(r#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"}]}"#);
        let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_render(settings.as_ptr(), events.as_ptr())
        }))
        .unwrap();
        assert!(envelope["ok"].as_str().unwrap().contains("app+v1.0"));
    }

    #[test]
    fn render_rejects_misspelled_jam_options() {
        let root = tempfile::tempdir().unwrap();
        let settings = cstring(&fixture(root.path()));
        // The FFI boundary is raw caller JSON — a misspelled option must
        // come back as an error envelope, never silently default.
        for events in [
            r#"{"events":[{"jam":{"js":["app"],"per_module":true}},{"dump":"js"}]}"#,
            r#"{"events":[{"jam":{"js":["app"],"sycn":true}},{"dump":"js"}]}"#,
        ] {
            let events = cstring(events);
            let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
                paste_render(settings.as_ptr(), events.as_ptr())
            }))
            .unwrap();
            let error = envelope["error"].as_str().unwrap_or("");
            assert!(error.contains("unknown field"), "got: {envelope}");
        }
        // The real spelling still selects per-module delivery.
        let events =
            cstring(r#"{"events":[{"jam":{"js":["app"],"perModule":true}},{"dump":"js"}]}"#);
        let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_render(settings.as_ptr(), events.as_ptr())
        }))
        .unwrap();
        assert!(
            envelope["ok"].as_str().unwrap().contains("app.h.v1.min.js"),
            "got: {envelope}"
        );
    }

    #[test]
    fn finish_validates_the_page_and_returns_empty() {
        let root = tempfile::tempdir().unwrap();
        let settings = cstring(&fixture(root.path()));

        // A complete page renders no markup at finish — ok is "".
        let events = cstring(r#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"},"finish"]}"#);
        let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_render(settings.as_ptr(), events.as_ptr())
        }))
        .unwrap();
        assert_eq!(envelope["ok"], "");

        // A jam no dump emitted fails the finish instead of dropping it.
        let events = cstring(r#"{"events":[{"dump":"js"},{"jam":{"js":["app"]}},"finish"]}"#);
        let text = take(unsafe { paste_render(settings.as_ptr(), events.as_ptr()) });
        let error = serde_json::from_str::<serde_json::Value>(&text).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(error.contains("app"), "{error}");
    }

    #[test]
    fn combo_returns_the_ok_envelope() {
        let root = tempfile::tempdir().unwrap();
        let settings = cstring(&fixture(root.path()));
        // 1767225600 is the fixture stamp (2026-01-01T00:00:00Z).
        let url = cstring("/paste/1767225600/app+v1.0.min.js");
        let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_combo(settings.as_ptr(), url.as_ptr())
        }))
        .unwrap();
        assert_eq!(envelope["ok"]["status"], 200);
        assert_eq!(envelope["ok"]["location"], serde_json::Value::Null);
        assert_eq!(envelope["ok"]["body"], "APP");
    }

    #[test]
    fn combo_redirect_carries_location_in_the_envelope() {
        let root = tempfile::tempdir().unwrap();
        let settings = cstring(&fixture(root.path()));
        // A stale stamp canonicalizes: the envelope's `location` names the
        // URL the host's HTTP layer should redirect to.
        let url = cstring("/paste/1767225599/app+v1.0.min.js");
        let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_combo(settings.as_ptr(), url.as_ptr())
        }))
        .unwrap();
        assert_eq!(envelope["ok"]["status"], 302);
        assert_eq!(
            envelope["ok"]["location"],
            "/paste/1767225600/app+v1.0.min.js"
        );
        assert_eq!(envelope["ok"]["body"], "");
    }

    #[test]
    fn combo_conditional_forwards_the_validator_and_tolerates_null() {
        let root = tempfile::tempdir().unwrap();
        let settings = cstring(&fixture(root.path()));
        let url = cstring("/paste/1767225600/app+v1.0.min.js");
        let stamp = "Thu, 01 Jan 2026 00:00:00 GMT"; // the fixture's

        // A matching validator → the 304 record, stamped and empty.
        let ims = cstring(stamp);
        let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_combo_conditional(settings.as_ptr(), url.as_ptr(), ims.as_ptr())
        }))
        .unwrap();
        assert_eq!(envelope["ok"]["status"], 304);
        assert_eq!(envelope["ok"]["body"], "");
        assert_eq!(envelope["ok"]["lastModified"], stamp);

        // A stale one → the ordinary 200, still stamped.
        let ims = cstring("Wed, 01 Jan 2025 00:00:00 GMT");
        let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_combo_conditional(settings.as_ptr(), url.as_ptr(), ims.as_ptr())
        }))
        .unwrap();
        assert_eq!(envelope["ok"]["status"], 200);
        assert_eq!(envelope["ok"]["body"], "APP");
        assert_eq!(envelope["ok"]["lastModified"], stamp);

        // A NULL or empty validator behaves exactly like paste_combo.
        for ims in [ptr::null(), cstring("").as_ptr()] {
            let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
                paste_combo_conditional(settings.as_ptr(), url.as_ptr(), ims)
            }))
            .unwrap();
            assert_eq!(envelope["ok"]["status"], 200);
            assert_eq!(envelope["ok"]["body"], "APP");
        }
    }

    #[test]
    fn null_arguments_are_an_error_envelope() {
        let settings = cstring("{}");
        let text = take(unsafe { paste_render(ptr::null(), settings.as_ptr()) });
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("null")
        );
    }

    #[test]
    fn non_utf8_input_is_an_error_envelope() {
        let settings = cstring(r#"{"manifest":"m","outputBase":"o"}"#);
        let bad = [0xff_u8, 0xfe, 0];
        let text = take(unsafe { paste_render(settings.as_ptr(), bad.as_ptr() as *const c_char) });
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("UTF-8")
        );
    }

    #[test]
    fn invalid_settings_json_is_an_error_envelope() {
        let settings = cstring("{not json");
        let events = cstring("{}");
        let text = take(unsafe { paste_render(settings.as_ptr(), events.as_ptr()) });
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("settings JSON")
        );
    }

    #[test]
    fn an_unknown_settings_field_is_an_error_envelope() {
        let settings = cstring(r#"{"manifest":"m","outputBase":"o","bogus":1}"#);
        let events = cstring("{}");
        let text = take(unsafe { paste_render(settings.as_ptr(), events.as_ptr()) });
        let error = serde_json::from_str::<serde_json::Value>(&text).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(error.contains("bogus"), "{error}");
    }

    #[test]
    fn an_unknown_module_render_is_an_error_naming_it() {
        let root = tempfile::tempdir().unwrap();
        let settings = cstring(&fixture(root.path()));
        let events = cstring(r#"{"events":[{"jam":{"js":["ghost"]}},{"dump":"js"}]}"#);
        let text = take(unsafe { paste_render(settings.as_ptr(), events.as_ptr()) });
        let error = serde_json::from_str::<serde_json::Value>(&text).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(error.contains("ghost"), "{error}");
    }

    #[test]
    fn render_page_issues_and_validates_a_generation_token() {
        let root = tempfile::tempdir().unwrap();
        let settings = cstring(&fixture(root.path()));
        let events = cstring(r#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"}]}"#);
        let empty = cstring("");

        // An empty generation asks for whatever the manifest is now — the
        // reply issues the token the page stores.
        let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_render_page(settings.as_ptr(), events.as_ptr(), empty.as_ptr())
        }))
        .unwrap();
        assert!(envelope["ok"]["html"]
            .as_str()
            .unwrap()
            .contains("app+v1.0"));
        let generation = envelope["ok"]["generation"].as_str().unwrap().to_string();
        assert!(!generation.is_empty());

        // NULL means the same as the empty string.
        let null_envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_render_page(settings.as_ptr(), events.as_ptr(), std::ptr::null())
        }))
        .unwrap();
        assert_eq!(null_envelope["ok"]["generation"], generation);

        // Posting the issued token back validates.
        let pinned = cstring(&generation);
        let again: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_render_page(settings.as_ptr(), events.as_ptr(), pinned.as_ptr())
        }))
        .unwrap();
        assert_eq!(again["ok"]["generation"], generation);

        // A token that matches nothing is a manifest-generation failure.
        let stale = cstring("deadbeefdeadbeefdeadbeefdeadbeef");
        let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_render_page(settings.as_ptr(), events.as_ptr(), stale.as_ptr())
        }))
        .unwrap();
        assert_eq!(envelope["kind"], "manifest-generation");
        assert!(envelope["error"].as_str().unwrap().contains("generation"));

        // A manifest rewrite mid-page invalidates a previously valid
        // token — bump mtime past filesystem granularity so the cache
        // cannot answer the old bytes.
        std::fs::write(
            root.path().join("manifest.json"),
            std::fs::read_to_string(root.path().join("manifest.json"))
                .unwrap()
                .replace("\"version\": \"42\"", "\"version\": \"43\""),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(root.path().join("manifest.json"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&take(unsafe {
            paste_render_page(settings.as_ptr(), events.as_ptr(), pinned.as_ptr())
        }))
        .unwrap();
        assert_eq!(envelope["kind"], "manifest-generation");
    }

    #[test]
    fn freeing_null_is_a_no_op() {
        unsafe { paste_free(ptr::null_mut()) };
    }
}
