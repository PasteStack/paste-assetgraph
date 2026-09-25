//! The C ABI the language runtimes call. Two entry points —
//! `paste_render` and `paste_combo` — take UTF-8 JSON strings and return a
//! heap C string holding a JSON envelope: `{"ok": ...}` on success or
//! `{"error": "<message>"}` on any failure. Every returned pointer must be
//! released with `paste_free`.

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use paste_assetgraph::api::{self, Settings};

/// One FFI call's outcome: `{"ok": <json>}` or `{"error": "<message>"}`.
fn envelope(result: Result<serde_json::Value, String>) -> *mut c_char {
    let body = match result {
        Ok(value) => serde_json::json!({ "ok": value }),
        Err(message) => serde_json::json!({ "error": message }),
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
fn guarded<F>(f: F) -> Result<serde_json::Value, String>
where
    F: FnOnce() -> Result<serde_json::Value, String>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => Err(format!(
            "paste-assetgraph panicked: {}",
            panic_message(payload)
        )),
    }
}

fn parse_settings(json: &str) -> Result<Settings, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid settings JSON: {e}"))
}

/// Render the event list `events_json` against `settings_json`; the
/// envelope's `ok` is the last dump's HTML string.
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
        serde_json::to_value(&response).map_err(|e| format!("{e:#}"))
    }))
}

/// Release a string returned by `paste_render` or `paste_combo`.
///
/// # Safety
/// `ptr` must be null (a no-op) or a pointer previously returned by
/// `paste_render`/`paste_combo`, freed at most once.
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
                        "hash": "h",
                        "version": 1,
                        "timestamp": "2026-01-01T00:00:00Z",
                        "dependencies": [],
                        "history": []
                    },
                    "js/app.js": {
                        "source": "src/app.js",
                        "output": "/js/app.h.v1.min.js",
                        "hash": "h",
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
    fn freeing_null_is_a_no_op() {
        unsafe { paste_free(ptr::null_mut()) };
    }
}
