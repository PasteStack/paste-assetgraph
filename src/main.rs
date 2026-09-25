use clap::{Parser, Subcommand};
use std::path::PathBuf;

use paste_assetgraph::{api, config, graph};

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
        #[arg(short, long, default_value = "paste-surface.conf")]
        config: PathBuf,

        /// Build in debug mode (no minification)
        #[arg(short, long)]
        debug: bool,

        /// Build for React/Vite project
        #[arg(long)]
        react: bool,

        /// Working directory (paths resolved relative to this)
        #[arg(long)]
        cwd: Option<PathBuf>,
    },

    /// Watch for changes and rebuild
    Watch {
        /// Configuration file path
        #[arg(short, long, default_value = "paste-surface.conf")]
        config: PathBuf,

        /// Working directory (paths resolved relative to this)
        #[arg(long)]
        cwd: Option<PathBuf>,
    },

    /// Render JAM tags for a replayed event list (events JSON on stdin,
    /// the last event's dump HTML on stdout)
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

    /// Initialize a new paste-surface.conf
    Init {
        /// Project type: basic, react, scala
        #[arg(short = 't', long, default_value = "basic")]
        project_type: String,

        /// Working directory (config written here)
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
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

            println!("Loading config from {:?}", config);
            let cfg = config::Config::load_with_base_dir(&config, &base_dir)?;
            let cfg = if debug { cfg.with_debug(true) } else { cfg };

            println!("Building assets...");
            println!("  Source JS: {:?}", cfg.source.js);
            println!("  Source SCSS: {:?}", cfg.source.scss);
            println!("  Output: {:?}", cfg.output.base);
            println!("  Include paths: {:?}", cfg.include_paths);

            let mut graph = graph::AssetGraph::new(cfg.clone());

            if react {
                graph.set_mode(graph::BuildMode::React);
            }

            let manifest = graph.build()?;

            println!("\nBuilt {} assets", manifest.assets.len());
            println!("Manifest saved to {:?}", cfg.manifest_path);

            for (key, entry) in &manifest.assets {
                println!("  {} → {} (v{})", key, entry.output, entry.version);
            }
        }

        Commands::Watch { config, cwd } => {
            let base_dir = cwd
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let cfg = config::Config::load_with_base_dir(&config, &base_dir)?.with_debug(true);
            graph::watch(cfg)?;
        }

        Commands::Render {
            manifest: manifest_path,
            output_base,
            output_js,
            output_css,
            cdn_base,
            base_path,
            inline_threshold,
        } => {
            let settings = api::Settings {
                manifest: manifest_path,
                output_base,
                output_js,
                output_css,
                cdn_base,
                base_path,
                inline_threshold,
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
