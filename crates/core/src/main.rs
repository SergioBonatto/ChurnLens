use anyhow::Result;
use churnlens_core::analyze_repository;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "churnlens")]
#[command(about = "Analyze code complexity and churn", long_about = None)]
struct Args {
    /// Path to repository
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Sort by field (deprecated for streaming mode)
    #[arg(short, long, default_value = "churn_score")]
    sort: String,

    /// Limit number of results (deprecated for streaming mode)
    #[arg(short, long)]
    limit: Option<usize>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.verbose {
        env_logger::Builder::from_default_env()
            .filter_level(log::LevelFilter::Debug)
            .init();
    } else {
        env_logger::Builder::from_default_env()
            .filter_level(log::LevelFilter::Info)
            .init();
    }

    // Set up graceful shutdown
    ctrlc::set_handler(move || {
        log::warn!("Interrupted! Shutting down...");
        std::process::exit(0);
    })?;

    analyze_repository(
        &args.path,
        &args.sort,
        args.limit,
    )?;

    Ok(())
}
