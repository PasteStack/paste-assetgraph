# paste-assetgraph Roadmap

Deferred items from code reviews. Organized by priority.

## High Priority

### Server manifest caching (server.rs)
`load_manifest()` is called in both `serve_js` and `serve_css`, performing disk I/O + JSON/bincode deserialization on every HTTP request. Under load this will be a significant bottleneck.

**Approach:** Implement a caching strategy informed by the original Python system's [`speed.py`](https://github.com/PasteStack/_attic-paste-service/blob/master/py/source/speed.py). Options include:
- TTL-based cache with configurable refresh interval
- File-watch (notify crate) to reload only on manifest change
- `Arc<RwLock<Manifest>>` shared state with periodic refresh

## Medium Priority

### SCSS without `@module` excluded from `sorted_deps` (graph.rs)
When an SCSS file has no `@module` comment, `process_scss` returns `None`, so the asset never appears in `sorted_deps`. The asset is still in `manifest.assets` and written to disk. This is by design (sorted_deps is a named-module index), but consumers relying on `sorted_deps` for a complete CSS asset list will miss unnamed assets.

**Approach:** Document the behavior clearly. If a complete asset list is needed, consumers should iterate `manifest.assets` filtered by key prefix (`css/`).

### HOCON unquoted values with `#` or `//` (config.rs)
`strip_inline_comment` treats `#` and `//` as comment delimiters in unquoted values. A value like `cdn.url = https://example.com/path#anchor` would be truncated. All generated configs use quoted values, but hand-edited configs could hit this.

**Approach:** Document that values containing `#` or `//` must be quoted. Alternatively, only strip comments after whitespace in unquoted contexts.

### `jamInlineBootstrap` packagesUrl timestamp (Manifest.scala)
`packagesUrl` uses `manifest.version` (global build ID) while the combo URL uses per-group max timestamp. This is intentional — packagesUrl is a generic base for dynamic module loading where any module could be requested. The two timestamp strategies serve different cache invalidation purposes. Document this design decision.

### Cross-type module resolution inflates CSS cache timestamps (manifest.rs / Manifest.scala)
`jam_url` / `jamUrlForType` intentionally uses unfiltered cross-type module lookup for version/timestamp retrieval — this ensures cache busting covers all requested modules. However, it means a CSS combo URL's group timestamp can change when only JS files change (e.g., `paste.ui.heroscroll` maps to JS, its JS timestamp gets picked up for the CSS URL). This causes unnecessary CSS cache invalidation for modules that only have JS assets.

**Approach:** Consider separating the group timestamp computation from version lookup: use the unfiltered lookup for version strings but only include timestamps from entries matching the requested asset type.

### O(n²) dependency expansion via `Vec::contains` (graph.rs)
`expand_all_dependencies` uses `result.contains(&t)` which is O(n) per call. For large dependency trees this results in O(n²) behavior. With typical paste module counts (<100) this is negligible, but a `HashSet` alongside the `Vec` would fix it for larger projects.

**Approach:** Maintain a `HashSet` parallel to the result `Vec` for O(1) dedup checks.

### Output path resolution fragility (server.rs / storage.rs)
The manifest stores output paths with a hardcoded `/{type}/` prefix (e.g., `/js/paste/dom.abc123.v1.min.js`). The server trims this prefix to resolve the filesystem path via `resolve_dir`. This works because `graph.rs` always produces the `/{type}/` prefix, but the coupling is implicit. If the output format changes, both graph and server must be updated in sync.

**Approach:** Consider storing the asset key (without prefix) in the manifest and having the server/storage resolve the full path, or add an assertion that validates the prefix on save.

## Low Priority

### Fragile source dir matching in sbt plugin (AssetGraphPlugin.scala)
`sourceDirs.find(_.contains("js"))` and `.contains("scss")` would match paths like `"src/json-schemas"` or `"src/scss-backups"`. The setting accepts `Seq[String]` (multiple dirs), but only one JS and one SCSS dir is actually used — the rest are silently dropped.

**Approach:** Use `.endsWith("/js")` or a more specific path suffix check instead of `.contains`. Consider supporting multiple source dirs or validating the setting.

### Unnecessary clone of paths on every build cycle (graph.rs)
`self.config.source.js.clone()` and `self.config.include_paths.clone()` are cloned to work around the borrow checker (can't borrow `self` immutably and mutably at the same time). These clones happen on every build/rebuild in watch mode.

**Approach:** Extract paths into local variables before the mutable borrow begins.

### Include path module name fallback (graph.rs)
When `scan_js_dir` processes files from `include_paths` that lack `@module` comments, the module name is derived relative to the include path root. For example, `../paste-elements/modules/heroscroll/heroscroll.js` becomes `modules.heroscroll.heroscroll` instead of `paste.ui.heroscroll`. Files in include paths should always have `@module` comments; the silent fallback to path-derived names is a footgun.

**Approach:** Log a warning when a file in an include path falls back to path-derived module naming.

### `strip_js_comments` nested template literal edge case (graph.rs)
When exiting a `${}` interpolation in a template literal (depth goes to 0, frame popped), the function does `continue` which skips the rest of the loop body. For deeply nested template literals like `` `a ${`b ${`c`}`}` ``, the template stack may become empty while `template_nesting > 0`, causing the re-entry path to not be reached. Practical impact depends on whether JS sources use nested template literals.

**Approach:** Add test cases for nested template literals and fix the state machine to correctly re-enter the template body after interpolation exit.

### Redundant byte comparison for versioned filenames (storage.rs)
The `Dist` backend's `write_asset` reads the entire existing file to compare bytes before writing. With versioned filenames (containing the hash), the filename itself already encodes content identity — if the file exists, the content is correct by construction. The byte comparison adds unnecessary I/O.

**Approach:** Skip the byte comparison for versioned filenames where the hash is embedded in the filename. Only compare for non-versioned files.

### `strip_js_comments` regex flag consumption too greedy (graph.rs)
After the closing `/` of a regex literal, the parser consumes all ASCII-alphabetic characters as flags. Valid JS regex flags are only `d`, `g`, `i`, `m`, `s`, `u`, `v`, `y`. If a regex is immediately followed by a keyword without whitespace (e.g., `/re/instanceof ...`), the keyword would be consumed as flags. Practical impact is minimal since this is only used for hash computation and such patterns are extremely rare.

**Approach:** Limit flag consumption to the set of valid regex flags (`[dgimsuy]`).

### `source.images` hardcoded in sbt plugin generated config (AssetGraphPlugin.scala)
The `source.images` path is hardcoded to `"src/main/assets/images"` and not derived from any sbt setting. If a user configures custom source directories, images will still point to the default. There is no `pasteAssetGraphImageDir` setting.

**Approach:** Add a `pasteAssetGraphImageDir` setting key and use it in `generateConfig`, or document the hardcoded default.

### Non-local `return` inside `foreach` lambda (PasteComboHandler.scala)
`getDependencies` uses `return` inside a `foreach` closure (line 163–167), which is a Scala anti-pattern — it works via exception throwing, is fragile across async boundaries, and can surprise future maintainers. The current usage is correct (returns from the enclosing method), but should be refactored.

**Approach:** Replace `manifest.foreach { m => ... return ... }` with `manifest.flatMap { m => ... }` pattern matching, avoiding non-local return.

### `jamInlineBootstrap` expands dependencies into combo URL unnecessarily (Manifest.scala)
`jamInlineBootstrap` calls `expandDependencies(modules)` before generating the combo URL, listing all transitive dependencies in the URL. `jamBootstrap` correctly uses the original `modules` list (the server handles expansion). This produces unnecessarily long URLs and is inconsistent, though not functionally broken since the server deduplicates.

**Approach:** Use `modules` directly instead of `allModules` for the combo URL in `jamInlineBootstrap`, matching `jamBootstrap` behavior.

### `jam_url` doesn't sanitize null bytes independently of `Manifest::load` (manifest.rs)
`sanitize_strings` is only called during `Manifest::load`. If a `Manifest` is constructed programmatically (e.g., in tests or future code paths) without calling `sanitize_strings`, null bytes in the `version` field could leak into JAM URLs. The Scala runtime sanitizes inline in `jamUrlForType`.

**Approach:** Either sanitize in `jam_url` directly, or make `sanitize_strings` pub and document that it must be called after programmatic construction.

### HOCON array parser missing test for elements containing `=` (config.rs)
The `get_array` closure correctly uses `splitn(2, '=')` to avoid splitting on `=` inside values, but there's no test verifying that array elements like `["a=b", "c=d"]` parse correctly. The scalar value test (`test_config_value_with_equals_sign`) exists but doesn't cover arrays.

**Approach:** Add a test case for `include-paths = ["path=with=equals"]`.

### `strip_js_comments` / `strip_css_comments` whitespace normalization destroys string literal whitespace (graph.rs)
The final `result.split_whitespace().join(" ")` collapses all whitespace including inside string literals. Since this is hash-only (not output), the impact is that whitespace-only changes inside strings won't trigger version bumps. Low probability but semantically incorrect.

**Approach:** Either remove the normalization entirely, or only normalize whitespace outside of string contexts by tracking quote state during the normalization pass.

### sass CLI paths from environment variables should be documented as trusted (PasteComboHandler.scala)
The `compileScssToCss` method passes `pasteElementsPath` and `siteCssPath` to the `sass` CLI as `--load-path` arguments. These paths originate from constructor parameters which may come from environment variables. While `Seq(...)` avoids shell interpolation, the trust boundary should be documented.

**Approach:** Document that `PASTE_ELEMENTS_PATH` and `SITE_CSS_PATH` environment variables are trusted inputs that must not contain adversarial values.

### Manifest path may be absolute in generated sbt config (AssetGraphPlugin.scala)
If `resourceManagedDir` is not under `baseDirectory` (unusual but possible with custom sbt settings), `relativeTo(base)` returns `None` and the fallback produces an absolute path in the generated config file. The Rust config loader handles absolute paths correctly, but the config becomes machine-specific and non-portable.

**Approach:** Always resolve relative to `baseDirectory` and warn if the path cannot be made relative.

### Binary.extractBundled race condition (Binary.scala)
If two parallel sbt builds check `Files.isExecutable(target)` simultaneously, both may attempt extraction. The atomic rename via temp file + `REPLACE_EXISTING` mitigates most risk, but there's a brief window where the target could be replaced while another process executes it.

**Approach:** Use a lock file (`target.lock`) with `FileLock` to serialize extraction across concurrent sbt processes.
