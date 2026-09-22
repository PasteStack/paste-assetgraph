use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod config;
mod graph;
mod manifest;
mod server;
mod storage;

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

    /// Serve JAM endpoint
    Serve {
        /// Configuration file path
        #[arg(short, long, default_value = "paste-surface.conf")]
        config: PathBuf,

        /// Port to serve on
        #[arg(short, long, default_value = "8080")]
        port: u16,

        /// Working directory (paths resolved relative to this)
        #[arg(long)]
        cwd: Option<PathBuf>,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
            graph::watch(cfg).await?;
        }

        Commands::Serve { config, port, cwd } => {
            let base_dir = cwd
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let cfg = config::Config::load_with_base_dir(&config, &base_dir)?;
            server::serve(cfg, port).await?;
        }

        Commands::Init { project_type, cwd } => {
            let base_dir = cwd
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            config::init_config(&project_type, &base_dir)?;
        }
    }

    Ok(())
}
