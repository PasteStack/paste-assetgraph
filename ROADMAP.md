# paste-assetgraph Roadmap

Deferred items from code reviews. Organized by priority.

## Medium Priority

### SCSS without `@module` excluded from `sorted_deps` (graph.rs)
When an SCSS file has no `@module` comment, `process_scss` returns `None`, so the asset never appears in `sorted_deps`. The asset is still in `manifest.assets` and written to disk. This is by design (sorted_deps is a named-module index), but consumers relying on `sorted_deps` for a complete CSS asset list will miss unnamed assets.

**Approach:** Document the behavior clearly. If a complete asset list is needed, consumers should iterate `manifest.assets` filtered by key prefix (`css/`).

### HOCON unquoted values with `#` or `//` (config.rs)
`strip_inline_comment` treats `#` and `//` as comment delimiters in unquoted values. A value like `cdn.url = https://example.com/path#anchor` would be truncated. All generated configs use quoted values, but hand-edited configs could hit this.

**Approach:** Document that values containing `#` or `//` must be quoted. Alternatively, only strip comments after whitespace in unquoted contexts.

### Output path resolution fragility (jam.rs / storage.rs)
The manifest stores output paths with a hardcoded `/{type}/` prefix (e.g., `/js/paste/dom.abc123.v1.min.js`). `OutputDirs::compiled_file` trims this prefix to resolve the filesystem path under the type's output directory. This works because `graph.rs` always produces the `/{type}/` prefix, but the coupling is implicit. If the output format changes, graph and jam must be updated in sync.

**Approach:** Consider storing the asset key (without prefix) in the manifest and having `compiled_file` resolve the full path, or add an assertion that validates the prefix on save.

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

### Redundant byte comparison for versioned filenames (storage.rs)
The `Dist` backend's `write_asset` reads the entire existing file to compare bytes before writing. With versioned filenames (containing the hash), the filename itself already encodes content identity — if the file exists, the content is correct by construction. The byte comparison adds unnecessary I/O.

**Approach:** Skip the byte comparison for versioned filenames where the hash is embedded in the filename. Only compare for non-versioned files.

### `source.images` hardcoded in sbt plugin generated config (AssetGraphPlugin.scala)
The `source.images` path is hardcoded to `"src/main/assets/images"` and not derived from any sbt setting. If a user configures custom source directories, images will still point to the default. There is no `pasteAssetGraphImageDir` setting.

**Approach:** Add a `pasteAssetGraphImageDir` setting key and use it in `generateConfig`, or document the hardcoded default.

### HOCON array parser missing test for elements containing `=` (config.rs)
The `get_array` closure correctly uses `splitn(2, '=')` to avoid splitting on `=` inside values, but there's no test verifying that array elements like `["a=b", "c=d"]` parse correctly. The scalar value test (`test_config_value_with_equals_sign`) exists but doesn't cover arrays.

**Approach:** Add a test case for `include-paths = ["path=with=equals"]`.

### Manifest path may be absolute in generated sbt config (AssetGraphPlugin.scala)
If `resourceManagedDir` is not under `baseDirectory` (unusual but possible with custom sbt settings), `relativeTo(base)` returns `None` and the fallback produces an absolute path in the generated config file. The Rust config loader handles absolute paths correctly, but the config becomes machine-specific and non-portable.

**Approach:** Always resolve relative to `baseDirectory` and warn if the path cannot be made relative.

### Binary.extractBundled race condition (Binary.scala)
If two parallel sbt builds check `Files.isExecutable(target)` simultaneously, both may attempt extraction. The atomic rename via temp file + `REPLACE_EXISTING` mitigates most risk, but there's a brief window where the target could be replaced while another process executes it.

**Approach:** Use a lock file (`target.lock`) with `FileLock` to serialize extraction across concurrent sbt processes.
