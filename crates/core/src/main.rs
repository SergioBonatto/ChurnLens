use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use uchikomi::analyze_repository_with_authors;

#[derive(Parser, Debug)]
#[command(name = "uchikomi")]
#[command(about = "Analyze code complexity and churn", long_about = None)]
struct Args {
    /// Path to repository
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Sort by field: file, risk, churn_score, cognitive, cyclomatic, loc
    #[arg(short, long, default_value = "file")]
    sort: String,

    /// Limit number of functions in the report
    #[arg(short, long)]
    limit: Option<usize>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,

    /// Include list of authors for each function
    #[arg(long)]
    include_authors: bool,
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

    let report = analyze_repository_with_authors(
        &args.path,
        &args.sort,
        args.limit,
        args.include_authors,
        shutdown,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);

    Ok(())
}
