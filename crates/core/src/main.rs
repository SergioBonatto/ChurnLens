use anyhow::Result;
use churnlens::analyze_repository;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

    let shutdown = Arc::new(AtomicBool::new(false));
    let r = shutdown.clone();

    // Set up graceful shutdown
    ctrlc::set_handler(move || {
        log::warn!("Interrupted! Initiating graceful shutdown...");
        r.store(true, Ordering::SeqCst);
    })?;

    analyze_repository(
        &args.path,
        &args.sort,
        args.limit,
        shutdown,
    )?;

    Ok(())
}
