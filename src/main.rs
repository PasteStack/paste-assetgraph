use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod config;
mod graph;
mod manifest;
mod server;

#[derive(Parser)]
#[command(name = "paste-assetgraph")]
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
    },
    
    /// Watch for changes and rebuild
    Watch {
        /// Configuration file path
        #[arg(short, long, default_value = "paste-surface.conf")]
        config: PathBuf,
    },
    
    /// Serve JAM endpoint
    Serve {
        /// Configuration file path
        #[arg(short, long, default_value = "paste-surface.conf")]
        config: PathBuf,
        
        /// Port to serve on
        #[arg(short, long, default_value = "8080")]
        port: u16,
    },
    
    /// Initialize a new paste-surface.conf
    Init {
        /// Project type: basic, react, scala
        #[arg(short = 't', long, default_value = "basic")]
        project_type: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Build { config, debug, react } => {
            println!("Loading config from {:?}", config);
            let cfg = config::Config::load(&config)?;
            let cfg = if debug { cfg.with_debug(true) } else { cfg };
            
            println!("Building assets...");
            println!("  Source JS: {:?}", cfg.source.js);
            println!("  Source SCSS: {:?}", cfg.source.scss);
            println!("  Output: {:?}", cfg.output.base);
            
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
        
        Commands::Watch { config } => {
            let cfg = config::Config::load(&config)?.with_debug(true);
            graph::watch(cfg).await?;
        }
        
        Commands::Serve { config, port } => {
            let cfg = config::Config::load(&config)?;
            server::serve(cfg, port).await?;
        }
        
        Commands::Init { project_type } => {
            config::init_config(&project_type)?;
        }
    }
    
    Ok(())
}
