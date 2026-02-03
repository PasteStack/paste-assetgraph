# paste-assetgraph Scala Runtime

Scala library for reading paste-assetgraph manifests and serving assets via Akka HTTP.

## Installation

```scala
libraryDependencies += "com.pastestack" %% "paste-assetgraph" % "0.1.0"
```

## Usage

```scala
import com.pastestack.assetgraph.Manifest
import com.pastestack.assetgraph.akka.AssetRoutes

// Load manifest
val manifest = Manifest.load("public/manifest.json")

// Create Akka HTTP routes
val assetRoutes = new AssetRoutes(manifest, Paths.get("public"))

// Use in your server
val routes = assetRoutes.routes ~ yourRoutes
```

## In Twirl Templates

```scala
<link rel="stylesheet" href="@assetRoutes.resolve("css/main.css")">
<script src="@assetRoutes.jamUrl(Seq("paste.dom", "paste.ui.heroscroll"))" defer></script>
```

## Auto-download Binary

The library automatically downloads the Rust binary on first use:

```scala
import com.pastestack.assetgraph.Binary

// Runs paste-assetgraph build
Binary.build("paste-surface.conf")
```
