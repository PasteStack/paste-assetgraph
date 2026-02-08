# paste-assetgraph Scala Runtime

Scala library for reading paste-assetgraph manifests and serving assets via Pekko HTTP.

## Installation

```scala
// Runtime library (manifest loading, combo handling)
libraryDependencies += "com.pastestack" %% "paste-assetgraph-runtime" % "0.1.0"

// Pekko HTTP routes (optional)
libraryDependencies += "com.pastestack" %% "paste-assetgraph-pekko" % "0.1.0"

// sbt plugin (add to project/plugins.sbt)
addSbtPlugin("com.pastestack" % "paste-assetgraph-sbt" % "0.1.0")
```

## Usage

```scala
import com.pastestack.assetgraph.Manifest
import com.pastestack.assetgraph.pekko.PasteComboRoute

// Load manifest
val manifest = Manifest.load("public/manifest.json")

// Create Pekko HTTP routes
val comboRoute = PasteComboRoute.withManifest(manifest, "target/public")

// Use in your server
val routes = concat(comboRoute.route, yourRoutes)
```

## In Twirl Templates

```scala
@* Generate JAM URL for JS modules *@
<script src="@manifest.jamUrlForJs(Seq("paste.dom", "paste.ui.heroscroll"))" defer></script>

@* Generate JAM URL for CSS modules *@
<link rel="stylesheet" href="@manifest.jamUrlForCss(Seq("site.theme"))">

@* Resolve individual asset paths *@
<link rel="stylesheet" href="@manifest.resolve("css/main.css")">
```

## sbt Integration

The sbt plugin automatically builds assets during compilation:

```scala
// build.sbt
enablePlugins(AssetGraphPlugin)

// Configure source directories
assetGraphSourceDirs := Seq("src/main/assets/js", "src/main/assets/scss")
assetGraphOutputDir := (Compile / resourceManaged).value / "public"
assetGraphMinify := true
assetGraphSourceMaps := true
```
