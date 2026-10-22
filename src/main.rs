use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use paste_assetgraph::{api, config, graph};

/// The configuration file `build` and `watch` read when `--config` is
/// not given. `paste-assetgraph.conf` is the current default; a lone
/// `paste-surface.conf` is still used, with a notice, so an existing
/// project keeps building until the file is renamed.
use config::{CONFIG_FILE_NAME as CONFIG_NAME, FORMER_CONFIG_FILE_NAME as FORMER_CONFIG_NAME};

/// Resolve the configuration path for a command: an explicit `--config`
/// wins verbatim; otherwise the new default name is preferred, a lone
/// former name still loads with a one-line notice, and neither yields
/// the new name so the ordinary load error names it.
fn resolve_config_path(given: Option<PathBuf>, base_dir: &Path) -> PathBuf {
    if let Some(path) = given {
        return path;
    }
    if base_dir.join(CONFIG_NAME).exists() {
        if base_dir.join(FORMER_CONFIG_NAME).exists() {
            eprintln!("note: using {CONFIG_NAME}; ignoring {FORMER_CONFIG_NAME}");
        }
        PathBuf::from(CONFIG_NAME)
    } else if base_dir.join(FORMER_CONFIG_NAME).exists() {
        eprintln!(
            "note: {FORMER_CONFIG_NAME} is the former default name; rename it to {CONFIG_NAME}"
        );
        PathBuf::from(FORMER_CONFIG_NAME)
    } else {
        PathBuf::from(CONFIG_NAME)
    }
}

#[derive(Parser)]
#[command(name = "paste-assetgraph")]
#[command(version)]
#[command(about = "Asset pipeline for PasteStack", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build all assets
    Build {
        /// Configuration file path
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Build in debug mode (no minification)
        #[arg(short, long)]
        debug: bool,

        /// Deprecated and ignored: the build is layout-agnostic (kept so
        /// existing scripts keep parsing)
        #[arg(long)]
        react: bool,

        /// Working directory (paths resolved relative to this)
        #[arg(long)]
        cwd: Option<PathBuf>,
    },

    /// Watch for changes and rebuild, in the same mode as `build`
    Watch {
        /// Configuration file path
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Build in debug mode (no minification)
        #[arg(short, long)]
        debug: bool,

        /// Working directory (paths resolved relative to this)
        #[arg(long)]
        cwd: Option<PathBuf>,
    },

    /// Render JAM tags for a replayed event list (events JSON on stdin,
    /// the last event's dump HTML on stdout; a list ending in "finish"
    /// validates the page and prints nothing)
    Render {
        /// Manifest JSON path
        #[arg(long)]
        manifest: PathBuf,

        /// Compiled output directory
        #[arg(long)]
        output_base: PathBuf,

        /// Compiled JS directory (default: <output-base>/js)
        #[arg(long)]
        output_js: Option<PathBuf>,

        /// Compiled CSS directory (default: <output-base>/css)
        #[arg(long)]
        output_css: Option<PathBuf>,

        /// URL prefix for emitted tags (a CDN origin)
        #[arg(long, default_value = "")]
        cdn_base: String,

        /// Combo URL path prefix
        #[arg(long, default_value = "/paste")]
        base_path: String,

        /// Automatic inline limit in UTF-8 bytes (0 disables)
        #[arg(long, default_value = "1024")]
        inline_threshold: usize,

        /// Request each resolved module's versioned file in dependency
        /// order instead of one combo URL per group
        #[arg(long)]
        per_module: bool,
    },

    /// Resolve a combo URL to its response, printed as JSON on stdout
    Combo {
        /// Manifest JSON path
        #[arg(long)]
        manifest: PathBuf,

        /// Compiled output directory
        #[arg(long)]
        output_base: PathBuf,

        /// Compiled JS directory (default: <output-base>/js)
        #[arg(long)]
        output_js: Option<PathBuf>,

        /// Compiled CSS directory (default: <output-base>/css)
        #[arg(long)]
        output_css: Option<PathBuf>,

        /// Combo URL path prefix
        #[arg(long, default_value = "/paste")]
        base_path: String,

        /// Request path, optionally followed by ?query
        path: String,
    },

    /// Initialize a new paste-assetgraph.conf
    Init {
        /// Project type: basic, react, scala
        #[arg(short = 't', long, default_value = "basic")]
        project_type: String,

        /// Working directory (config written here)
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
}

/// The CLI's recovery from a dead file watcher backend: start this same
/// command over (same arguments, same pid) so the dead backend's open
/// descriptors are released and the fresh process's initial build covers
/// anything missed. It refuses — and watch replaces only the watcher — when
/// the executable on disk is no longer the one running (a binary replaced
/// mid-session would run silently otherwise) or when the configuration no
/// longer loads (the restarted process would exit on it).
#[cfg(unix)]
fn restart_watch_process(config: PathBuf, base_dir: PathBuf) -> Option<Box<dyn Fn() -> String>> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::CommandExt;
    let identity = |path: &Path| {
        std::fs::metadata(path)
            .ok()
            .map(|m| (m.dev(), m.ino(), m.mtime(), m.mtime_nsec()))
    };
    let exe = std::env::current_exe().ok()?;
    let started = identity(&exe)?;
    Some(Box::new(move || {
        if identity(&exe) != Some(started) {
            return format!("{} changed on disk since this watch started", exe.display());
        }
        if let Err(e) = config::Config::load_with_base_dir(&config, &base_dir) {
            return format!("the configuration no longer loads: {e:#}");
        }
        std::process::Command::new(&exe)
            .args(std::env::args_os().skip(1))
            .exec()
            .to_string()
    }))
}

#[cfg(not(unix))]
fn restart_watch_process(_config: PathBuf, _base_dir: PathBuf) -> Option<Box<dyn Fn() -> String>> {
    None
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build {
            config,
            debug,
            react,
            cwd,
        } => {
            let base_dir = cwd
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let config = resolve_config_path(config, &base_dir);

            println!("Loading config from {:?}", config);
            let cfg = config::Config::load_with_base_dir(&config, &base_dir)?;
            let cfg = if debug { cfg.with_debug(true) } else { cfg };

            println!("Building assets...");
            // The roots discovery walks: `source.dirs`, the legacy
            // `source.js`/`source.scss` pair, or the defaults.
            println!("  Sources: {:?}", cfg.source_dirs());
            println!("  Output: {:?}", cfg.output.base);
            println!("  Include paths: {:?}", cfg.include_paths);

            let mut graph = graph::AssetGraph::new(cfg.clone());

            if react {
                eprintln!(
                    "Warning: --react is deprecated and has no effect; the build is layout-agnostic"
                );
            }

            let manifest = graph.build()?;

            // Retired entries stay recorded so their pinned URLs keep
            // serving, but this build compiled none of them.
            let retired = manifest.assets.values().filter(|e| e.retired).count();
            println!("\nBuilt {} assets", manifest.assets.len() - retired);
            if retired > 0 {
                println!("Kept {} retired assets for their pinned URLs", retired);
            }
            println!("Manifest saved to {:?}", cfg.manifest_path);

            for (key, entry) in manifest.assets.iter().filter(|(_, e)| !e.retired) {
                println!("  {} → {} (v{})", key, entry.output, entry.version);
            }
        }

        Commands::Watch { config, debug, cwd } => {
            let base_dir = cwd
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let config = resolve_config_path(config, &base_dir);
            // The same mode `build` uses: watch output lands in the same
            // retained version history, so a forced debug mode would mint
            // an unminified version on every switch between the two.
            let cfg = config::Config::load_with_base_dir(&config, &base_dir)?;
            let cfg = if debug { cfg.with_debug(true) } else { cfg };
            graph::install_watch_backend_monitor();
            graph::watch_with(
                cfg,
                graph::WatchOptions {
                    restart_process: restart_watch_process(config, base_dir),
                },
            )?;
        }

        Commands::Render {
            manifest: manifest_path,
            output_base,
            output_js,
            output_css,
            cdn_base,
            base_path,
            inline_threshold,
            per_module,
        } => {
            let settings = api::Settings {
                manifest: manifest_path,
                output_base,
                output_js,
                output_css,
                cdn_base,
                base_path,
                inline_threshold,
                per_module,
            };
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
            let html = api::render(&settings, &input)?;
            use std::io::Write;
            print!("{}", html);
            std::io::stdout().flush()?;
        }

        Commands::Combo {
            manifest: manifest_path,
            output_base,
            output_js,
            output_css,
            base_path,
            path,
        } => {
            let settings = api::Settings {
                manifest: manifest_path,
                output_base,
                output_js,
                output_css,
                cdn_base: String::new(),
                base_path,
                inline_threshold: 0,
                per_module: false,
            };
            let response = api::combo(&settings, &path)?;
            println!("{}", serde_json::to_string(&response)?);
        }

        Commands::Init { project_type, cwd } => {
            let base_dir = cwd
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            config::init_config(&project_type, &base_dir)?;
        }
    }

    Ok(())
}
