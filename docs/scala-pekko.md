# Using paste-assetgraph with Scala/Pekko HTTP

The Scala runtime reads the manifest and serves assets natively from Pekko HTTP.

## Installation

```scala
// build.sbt
libraryDependencies ++= Seq(
  "com.pastestack" %% "paste-assetgraph-runtime" % "0.1.0",
  "com.pastestack" %% "paste-assetgraph-pekko" % "0.1.0"
)
```

The Rust binary is downloaded automatically on first use.

## Quick Start

```scala
import org.apache.pekko.actor.ActorSystem
import org.apache.pekko.http.scaladsl.Http
import org.apache.pekko.http.scaladsl.server.Directives._
import com.pastestack.assetgraph.Manifest
import com.pastestack.assetgraph.pekko.PasteComboRoute

object Main extends App {
  implicit val system: ActorSystem = ActorSystem()
  implicit val ec = system.dispatcher
  
  // Load manifest (built by paste-assetgraph)
  val manifest = Manifest.load("src/main/public/manifest.json")
  val comboRoute = PasteComboRoute.withManifest(manifest, "src/main/public")
  
  val routes = concat(
    comboRoute.route,  // JAM endpoint with Cache-Control
    path("") {
      get {
        complete(views.html.home(manifest))
      }
    }
  )
  
  Http().newServerAt("0.0.0.0", 8080).bind(routes)
}
```

## Project Structure

```
my-app/
├── build.sbt
├── src/main/
│   ├── resources/
│   │   └── paste-surface.conf
│   ├── assets/                 # Source files
│   │   ├── js/
│   │   └── scss/
│   ├── public/                 # Generated
│   │   ├── manifest.json
│   │   ├── js/
│   │   └── css/
│   ├── scala/
│   │   └── Main.scala
│   └── twirl/
│       └── views/
│           └── home.scala.html
```

## Configuration

`src/main/resources/paste-surface.conf`:
```hocon
debug-mode = false

source.js = "src/main/assets/js"
source.scss = "src/main/assets/scss"
source.images = "src/main/assets/images"

output.base = "src/main/public"
output.js = "src/main/public/js"
output.css = "src/main/public/css"
output.images = "src/main/public/images"

manifest = "src/main/public/manifest.json"

minify = true
source-maps = true

jam.enabled = true
jam.base-path = "/paste"
```

## Twirl Templates

`views/home.scala.html`:
```scala
@(manifest: com.pastestack.assetgraph.Manifest)

<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <title>My App</title>
  
  @* Versioned CSS via JAM *@
  <link rel="stylesheet" href="@manifest.jamUrlForCss(Seq("site.theme"))">
</head>
<body>
  <h1>Hello World</h1>
  
  @* JAM: bundled JS modules with dependencies *@
  <script src="@manifest.jamUrlForJs(Seq("paste.dom", "paste.ui.heroscroll"))" defer></script>
</body>
</html>
```

## Build Integration

### Option 1: sbt task

```scala
// build.sbt
val assetBuild = taskKey[Unit]("Build assets")

assetBuild := {
  import com.pastestack.assetgraph.Binary
  Binary.build("src/main/resources/paste-surface.conf")
}

Compile / compile := (Compile / compile).dependsOn(assetBuild).value
```

### Option 2: Manual

```bash
# Build assets before running
paste-assetgraph build --config src/main/resources/paste-surface.conf

# Run server
sbt run
```

### Option 3: Watch mode (development)

Terminal 1:
```bash
paste-assetgraph watch --config src/main/resources/paste-surface.conf
```

Terminal 2:
```bash
sbt run
```

## API Reference

### `Manifest.load(path: String): Manifest`

Load manifest from JSON file.

### `manifest.resolve(sourcePath: String, debug: Boolean = false): Option[String]`

Resolve source path to versioned output path.

```scala
manifest.resolve("css/main.css")
// => Some("/css/main.def456.v1.min.css")
```

### `manifest.jamUrl(modules: Seq[String], debug: Boolean = false): String`

Generate JAM URL for bundled modules.

```scala
manifest.jamUrl(Seq("paste.dom", "paste.ui.heroscroll"))
// => "/paste/2025.02.02/abc123/paste.dom,paste.ui.heroscroll.min.js"
```

### `manifest.getDependencies(module: String): Seq[String]`

Get all dependencies for a module (recursive, topologically sorted).

```scala
manifest.getDependencies("paste.ui.heroscroll")
// => Seq("paste.dom", "paste.event", "paste.ui.throttle", "paste.ui.heroscroll")
```

### `manifest.getVersion(key: String, version: Option[Int] = None): Option[String]`

Get output path for specific version (for rollback).

```scala
manifest.getVersion("css/main.css", Some(5))
// => Some("/css/main.abc123.v5.min.css")
```

## PasteComboRoute

### Factory Methods

```scala
// With manifest (preferred)
PasteComboRoute.withManifest(manifest: Manifest, outputBasePath: String)

// With custom paths
PasteComboRoute(pastePath: String, pasteElementsPath: String)

// With manifest and custom paths
PasteComboRoute(manifest: Manifest, pastePath: String, pasteElementsPath: String)
```

### `route: Route`

JAM endpoint: `/paste/{timestamp}/{modules}.min.js` or `/paste/{timestamp}/{modules}.min.css`

Responses include `Cache-Control: public, max-age=31536000` for immutable caching.

## Debug Mode

Set `PASTE_DEBUG=true` environment variable to:
- Serve source files instead of minified
- Include source maps
- Disable long cache headers
