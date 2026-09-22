# JAM (JavaScript Asset Manager) Specification

## Overview

JAM is the module loading system for PasteStack. It provides:
- AMD-like module definition and dependency resolution
- Combo URL loading for bundled modules
- Inline bootstrap for paste.js module loader

This spec defines the behavior that ALL runtime implementations must follow.

## URL Format

### Combo URL Structure

```
/paste/{timestamp}/{module+version,...}.{min.}js
```

**Components:**
- `/paste/` - Fixed prefix (configurable via `packagesUrl`)
- `{timestamp}` - Unix timestamp of when the module group last changed
- `{module+version,...}` - Comma-separated list of modules with versions
- `.min.js` or `.js` - Minified vs debug suffix

**Examples:**
```
/paste/1387575212/paste.dom+v1.0,paste.event+v2.0.min.js
/paste/1387575212/paste.ui.heroscroll+v3.0,paste.ui.stickynav+v1.0.min.js
```

### CSS Combo URL

Same format with `.css` extension:
```
/paste/1387575212/paste.base+v1.0,paste.ui.heroscroll+v1.0.min.css
```

## Bootstrap HTML

### Full Bootstrap (with inlined paste.js)

```html
<script type="text/javascript">
{paste.js content inlined here}
</script>
<script type="text/javascript">
(function(d,r){
  d('paste.dom').isLoading=d('paste.event').isLoading=!0;
  r('/paste/1387575212/paste.dom+v1.0,paste.event+v2.0.min.js');
})(paste.define,paste.require);
</script>
```

### Module Loading Bootstrap (paste.js already loaded)

```html
<script type="text/javascript">
(function(d,r){
  d('paste.dom').isLoading=d('paste.event').isLoading=!0;
  r('/paste/1387575212/paste.dom+v1.0,paste.event+v2.0.min.js');
})(paste.define,paste.require);
</script>
```

## Behavior Requirements

### 1. Timestamp Calculation

The timestamp MUST be the maximum timestamp of all modules in the group:

```
timestamp = max(module1.timestamp, module2.timestamp, ...)
```

This ensures cache invalidation when ANY module in the combo changes.

### 2. Module Version Format

Module versions MUST be formatted as `{module}+v{version}.0`:

```
paste.dom+v1.0
paste.ui.heroscroll+v3.0
```

### 3. isLoading Pre-registration

Before loading the combo URL, modules MUST be pre-registered as loading:

```javascript
paste.define('module.name').isLoading = true;
```

This allows paste.js to defer activation until the script loads.

### 4. Combo URL Serving

The server MUST:
1. Parse the combo URL to extract module list
2. Read each module's content from storage
3. Concatenate all module contents
4. Return with appropriate Content-Type (`application/javascript` or `text/css`)
5. Set cache headers based on timestamp

### 5. Debug vs Production Mode

**Production mode** (`.min.js`, `.min.css`):
- Serve pre-compiled, minified content from manifest paths
- CSS served from compiled output (SCSS already processed)

**Debug mode** (`.js`, `.css` without `.min`):
- Serve raw source files
- SCSS compiled on-the-fly to CSS
- No minification
- May include source maps

The URL suffix determines the mode:
```
/paste/123/paste.dom+v1.0.min.js  → Production (minified)
/paste/123/paste.dom+v1.0.js      → Debug (raw source)
```

### 6. SCSS Compilation

For CSS modules:
- **Build time**: SCSS files are compiled to CSS and minified
- **Dev mode**: SCSS compiled on-the-fly when `.css` requested but only `.scss` exists
- Compiled output path: `{checksum}.v{version}.min.css`

## Module Declaration

### @module Comment Format

Modules declare their name explicitly using YUI-doc style comments:

```javascript
/**
 * Description of the module
 * @requires paste/dom
 * @requires paste/event
 * @module paste/ui/heroscroll
 */
```

**Comment format:**
- `@module {namespace}/{name}` - The canonical module name (uses `/` separator)
- `@requires {namespace}/{name}` - Dependencies (optional, for documentation)

**Module name conversion:**
- Comment: `@module paste/ui/heroscroll` 
- Runtime: `paste.ui.heroscroll` (dots for JavaScript)
- URL: `paste.ui.heroscroll+v1.0`

### CSS Module Declaration

SCSS/CSS files use the same format:

```scss
/**
 * Hero scroll component styles
 * @module paste/ui/heroscroll
 */
```

### Module Name Resolution Priority

When resolving a module name to its asset:

1. **Manifest lookup** (preferred) - the `modules` map names the module's key in
   the `assets` map, for the requested asset type
2. **Key convention** (fallback) - `{type}/{module with dots as slashes}.{type}`

### Manifest Module Map

The manifest MUST include a `modules` section mapping each declared name to the
module's key in the `assets` map — never to a source path. A consumer resolves a
module by looking that value up in `assets`, so a value that is not a key of
`assets` is ignored in favour of the convention key:

```json
{
  "modules": {
    "paste.ui.heroscroll": "js/paste/ui/heroscroll.js",
    "paste.ui.stickynav": "js/paste/ui/stickynav.js",
    "site.home": "js/site/home.js",
    "site.theme": "css/theme.css"
  },
  "assets": { ... }
}
```

A declared name need not match the file layout: `site.theme` above is declared by
`theme.scss`, whose asset key stays `css/theme.css`. That is precisely why the
map exists. Each asset entry carries its own `source`, recorded relative to the
project root.

### Module Extraction

Assetgraph MUST:
1. Parse JS/SCSS files for `@module` comments
2. Extract module name (converting `/` to `.`)
3. Include in manifest's `modules` map
4. Support files without `@module` (fallback to filename)

## Manifest Structure

### Module Entry Properties

Each module in the manifest MUST include:

```json
{
  "source_path": "src/js/home.js",
  "source_checksum": "abc123...",
  "name": "site.home",
  "dependencies": ["paste.dom", "paste.event", "paste.util"],
  "last_modified": 1387575212,
  "checksum": "def456...",
  "path": "js/site/home.def456.v1.min.js",
  "byte_size": 1234,
  "version": 1
}
```

**Key property:** `dependencies` MUST contain ALL transitive dependencies, not just direct ones. This is computed during build via topological sort with backfill.

### Dependency Expansion (Build Time)

During manifest build, dependencies MUST be expanded:

1. Parse `@requires` from each module
2. Build dependency graph
3. Topologically sort all modules
4. **Backfill transitive dependencies** - each module gets ALL its transitive dependencies added

```python
# Original behavior from manifest.py
def walk_tree(dep, dep_set=None):
    for d in name_dep_dict[dep]:
        dep_set.add(d)
        dep_set |= walk_tree(d, dep_set)
    return dep_set

# Backfill
for module_name in reversed(flat_top_sorted):
    module.dependencies |= walk_tree(module_name)
```

### sorted_deps

The manifest SHOULD include a `sorted_deps` section with pre-computed topological order:

```json
{
  "sorted_deps": {
    "js": [
      ["paste.util", "js/paste.util.abc.v1.min.js", 1],
      ["paste.dom", "js/paste.dom.def.v1.min.js", 1],
      ["paste.event", "js/paste.event.ghi.v2.min.js", 2],
      ["site.home", "js/site/home.jkl.v1.min.js", 1]
    ],
    "css": [...]
  }
}
```

This allows combo URL serving without runtime dependency resolution.

## Manifest Integration

### Reading paste.js for Inlining

Runtime MUST support reading paste.js from:
1. Filesystem path (absolute or relative)
2. Classpath resource (path starting with `/`)

```scala
// Scala
manifest.jamInlineBootstrap("/paste.js", modules)

// Python
manifest.jam_inline_bootstrap("/paste.js", modules)
```

### Resolving Module Versions

For each module, lookup in manifest:

```json
{
  "assets": {
    "js/paste/dom.js": {
      "version": 1,
      "timestamp": "1387575212",
      "output": "/js/paste.dom.abc123.v1.min.js"
    }
  }
}
```

## BDD Scenarios

### Feature: JAM URL Generation

```gherkin
Scenario: Generate combo URL for single module
  Given a manifest with paste.dom at version 1 and timestamp 1387575212
  When I request a JAM URL for ["paste.dom"]
  Then the URL should be "/paste/1387575212/paste.dom+v1.0.min.js"

Scenario: Generate combo URL for multiple modules
  Given a manifest with:
    | module         | version | timestamp  |
    | paste.dom      | 1       | 1387575210 |
    | paste.event    | 2       | 1387575212 |
  When I request a JAM URL for ["paste.dom", "paste.event"]
  Then the URL should be "/paste/1387575212/paste.dom+v1.0,paste.event+v2.0.min.js"
  And the timestamp should be the maximum (1387575212)

Scenario: Generate debug URL
  Given a manifest with paste.dom at version 1
  When I request a JAM URL for ["paste.dom"] with debug=true
  Then the URL should end with ".js" not ".min.js"
```

### Feature: Bootstrap Generation

```gherkin
Scenario: Generate bootstrap with isLoading
  Given modules ["paste.dom", "paste.event"]
  When I generate a bootstrap
  Then the script should contain "d('paste.dom').isLoading=d('paste.event').isLoading=!0"

Scenario: Generate inline bootstrap with paste.js
  Given paste.js content "/* paste.js code */"
  And modules ["paste.dom"]
  When I generate an inline bootstrap
  Then the first script tag should contain the paste.js content
  And the second script tag should load the combo URL
```

### Feature: Combo URL Serving

```gherkin
Scenario: Serve combo URL
  Given modules paste.dom with content "/* dom */" and paste.event with content "/* event */"
  When I request "/paste/1387575212/paste.dom+v1.0,paste.event+v1.0.min.js"
  Then the response should contain "/* dom */" followed by "/* event */"
  And the Content-Type should be "application/javascript"

Scenario: Serve CSS combo URL
  Given modules paste.base with content "body{}" and paste.ui with content ".ui{}"
  When I request "/paste/1387575212/paste.base+v1.0,paste.ui+v1.0.min.css"
  Then the response should contain "body{}" followed by ".ui{}"
  And the Content-Type should be "text/css"
```

## Build System: Primer

The Primer processes source files at build time:

### JavascriptPrimer

1. Parse `@module` and `@requires` from JS doc comments
2. **Production mode**: Compress using Closure Compiler with `--compilation_level`
3. **Compile mode** (dev): Store raw source, path points to source file
4. Write compressed output to `{hash}.v{version}.min.js`

### SCSSPrimer

1. Parse `@module` and `@requires` from SCSS comments
2. Strip module/require declarations from source before compilation
3. **Production mode**: Compile SCSS to CSS using sass compiler with `--compress`
4. **Compile mode** (dev): Store cleaned source, compile on-the-fly when read
5. `read_primed()` in compile mode triggers SCSS compilation

```python
# Original compile-mode behavior
if env.compile_mode:
    contents = compressor.compress(
        cls._clean_source_contents(contents),
        'css',
        load_paths=cls.SCSS_LOAD_PATHS
    )
```

## Runtime: Jammer

The Jammer serves combo URLs using the manifest:

### Key Behaviors

1. **Use sorted_deps for ordering** - Dependencies ordered per `content_type_manifest.sorted_deps`
2. **Star dependency expansion** - `paste.*` expands to all matching modules
3. **Filter loaded deps** - `filter_loaded()` removes already-loaded dependencies
4. **Read via primer** - `read_contents()` calls `primer.read_primed()` for on-the-fly compilation

### Dependency Resolution

```python
# Order dependencies according to manifest sorted_deps
def _order_dependencies(self, dependencies):
    ordered_dependencies = OrderedDict()
    for name, path, version in self.content_type_manifest.sorted_deps:
        if name in dependencies:
            ordered_dependencies[name] = dependencies.get(name)
    return ordered_dependencies
```

### Debug Mode Detection

```python
@property
def is_debug(self):
    return env.compile_mode
```

In debug mode:
- SCSS compiled on-the-fly via `primer.read_primed()`
- Timestamps from source file stats
- URL uses source paths

## Runtime Implementation Checklist

Each runtime (Scala, Python, Rust, etc.) MUST implement:

- [ ] `jamJsUrl(modules, debug)` - Generate JS combo URL
- [ ] `jamCssUrl(modules, debug)` - Generate CSS combo URL
- [ ] `jamBootstrap(modules, cdnBase)` - Generate bootstrap script
- [ ] `jamInlineBootstrap(pasteJsPath, modules, cdnBase)` - Generate with inlined paste.js
- [ ] `readFile(path)` - Read from filesystem
- [ ] `readResource(path)` - Read from classpath/package resources
- [ ] Combo URL handler for serving concatenated modules

### Build Tool Checklist

- [ ] Parse `@module` and `@requires` from JS/SCSS files
- [ ] Expand transitive dependencies at build time
- [ ] Generate `sorted_deps` (topologically sorted modules)
- [ ] Compress JS using Closure Compiler (production)
- [ ] Compile SCSS to CSS using sass (production)
- [ ] Store raw source paths for compile/debug mode
- [ ] Version tracking with history

### Combo Handler Checklist

- [ ] Parse combo URL to extract modules, versions, and debug flag
- [ ] Order modules using `sorted_deps` from manifest
- [ ] In debug mode: read raw source, compile SCSS on-the-fly
- [ ] In production mode: read pre-compiled content
- [ ] Support star dependencies (`paste.*`)
- [ ] Filter already-loaded dependencies

## Version History

- **v1.0** (2026-02-03): Initial specification
- **v1.1** (2026-02-03): Added Primer/Jammer documentation, sorted_deps, expanded dependencies
