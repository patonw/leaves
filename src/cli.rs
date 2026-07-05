use std::path::{Path, PathBuf};

use clap::Parser;
use color_eyre::Result;

#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Scanning root path
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Maximum depth of tree to keep in memory.
    ///
    /// Subtrees below this depth are replaced with summary nodes.
    /// Does not affect scan depth.
    #[arg(short = 'd', long, default_value_t = 5)]
    pub max_depth: usize,

    /// Swap the file/directory color schemes
    #[arg(short = 'c', long, visible_aliases = ["colors", "spring"])]
    pub swap_colors: bool,

    /// Group files by type at the top-level, then split each region by directory.
    #[arg(short, long)]
    pub xray: bool,

    /// Don't *automatically* skip any files. Only overrides will be used.
    #[arg(short = 'A', long)]
    pub include_all: bool,

    /// Don't skip hidden files and folders
    #[arg(short = 'H', long)]
    pub include_hidden: bool,

    /// Don't skip .ignore'd files
    #[arg(short = 'I', long)]
    pub include_ignored: bool,

    /// Don't skip .gitignore'd files and folders
    #[arg(short = 'G', long)]
    pub include_gitignored: bool,

    /// Don't skip files and folders listed in .git/info/exclude
    #[arg(short = 'E', long)]
    pub include_gitexcluded: bool,

    /// Git-style override globs. '!' prefix negates glob
    pub overrides: Vec<String>,
}

impl Args {
    pub fn with_depth(&self, max_depth: usize) -> Self {
        Self {
            max_depth,
            ..self.clone()
        }
    }

    pub fn with_path(&self, path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            ..self.clone()
        }
    }
}

pub fn init_logging() -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan, prelude::*};

    let proj = env!("CARGO_CRATE_NAME").to_uppercase(); // need compile-time uppercase
    let Some(log_dir_env) = std::env::var_os(format!("{proj}_LOG_DIR")) else {
        return Ok(());
    };

    let log_dir = Path::new(&log_dir_env);
    std::fs::create_dir_all(log_dir)?;

    let log_path = log_dir.join("leaves.log");

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    let filter = EnvFilter::from_default_env();

    let file_subscriber = tracing_subscriber::fmt::layer()
        .with_span_events(FmtSpan::CLOSE)
        .with_file(true)
        .with_line_number(true)
        .with_writer(log_file)
        .with_target(false)
        .with_ansi(false)
        .with_filter(filter.clone());

    tracing_subscriber::registry()
        .with(file_subscriber)
        .with(filter)
        .init();

    Ok(())
}
