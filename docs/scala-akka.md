# Using paste-assetgraph with Scala/Akka HTTP

The Scala runtime reads the manifest and serves assets natively from Akka HTTP.

## Installation

```scala
// build.sbt
libraryDependencies += "com.pastestack" %% "paste-assetgraph" % "0.1.0"
```

The Rust binary is downloaded automatically on first use.

## Quick Start

```scala
import akka.actor.ActorSystem
import akka.http.scaladsl.Http
import akka.http.scaladsl.server.Directives._
import com.pastestack.assetgraph.Manifest
import com.pastestack.assetgraph.akka.AssetRoutes

import java.nio.file.Paths

object Main extends App {
  implicit val system: ActorSystem = ActorSystem()
  implicit val ec = system.dispatcher
  
  // Load manifest (built by paste-assetgraph)
  val assetRoutes = new AssetRoutes(
    manifest = Manifest.load("src/main/public/manifest.json"),
    publicDir = Paths.get("src/main/public"),
    debug = sys.env.get("PASTE_DEBUG").contains("true")
  )
  
  val routes = 
    assetRoutes.routes ~  // JAM endpoint + static files
    path("") {
      get {
        complete(views.html.home(assetRoutes))
      }
    }
  
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
paste.surface.assets {
  source {
    js = "src/main/assets/js"
    scss = "src/main/assets/scss"
  }
  output {
    base = "src/main/public"
    js = "src/main/public/js"
    css = "src/main/public/css"
  }
  manifest = "src/main/public/manifest.json"
}
```

## Twirl Templates

`views/home.scala.html`:
```scala
@(assetRoutes: com.pastestack.assetgraph.akka.AssetRoutes)

<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <title>My App</title>
  
  @* Versioned CSS *@
  @assetRoutes.resolve("css/main.css").map { path =>
    <link rel="stylesheet" href="@path">
  }
</head>
<body>
  <h1>Hello World</h1>
  
  @* JAM: bundled modules with dependencies *@
  <script src="@assetRoutes.jamUrl(Seq("paste.dom", "paste.ui.heroscroll"))" defer></script>
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

## AssetRoutes

### Constructor

```scala
new AssetRoutes(
  manifest: Manifest,
  publicDir: Path,
  debug: Boolean = false
)
```

### `routes: Route`

Combined route for JAM endpoint + static files.

### `jamRoute: Route`

JAM endpoint only: `/paste/{version}/{checksum}/{modules}.min.js`

### `staticRoute: Route`

Static file serving for `/js/`, `/css/`, `/images/`.

## Debug Mode

Set `PASTE_DEBUG=true` environment variable to:
- Serve source files instead of minified
- Include source maps
- Disable long cache headers
