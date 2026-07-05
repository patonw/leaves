use std::sync::{Arc, Mutex};

use color_eyre::Result;
use tracing::{Level, instrument, span};

mod app;
mod cli;
mod core;
mod explorer;
mod forest;
mod render;
mod scanfs;
mod state;
mod util;

use app::App;
use cli::{Args, init_logging};
use scanfs::{ScanState, ScanUI, walk_fs};

use crate::util::SWAP_COLORS;

#[instrument]
fn main() -> Result<()> {
    init_logging()?;
    color_eyre::install()?;

    use clap::Parser as _;
    let mut args = Args::parse();

    if args.include_all {
        args.include_hidden = true;
        args.include_ignored = true;
        args.include_gitignored = true;
        args.include_gitexcluded = true;
    }

    if args.swap_colors {
        SWAP_COLORS.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    args.path = args.path.canonicalize()?;

    let scan_state = Arc::new(Mutex::new(ScanState::default()));

    let th = {
        let state = scan_state.clone();
        let args = args.clone();
        std::thread::spawn(move || {
            let result = walk_fs(&args, state.clone());
            let mut state = state.lock().unwrap();
            state.done = true;
            result
        })
    };

    let quit = span!(Level::DEBUG, "Scanning")
        .in_scope(|| ratatui::run(|term| ScanUI::new(scan_state).run(term)))?;
    if quit {
        return Ok(());
    }

    let scanned = span!(Level::DEBUG, "Gathering scan results").in_scope(|| {
        th.join()
            .map_err(|_e| eyre::eyre!("Failed to join scanner thread"))
    })??;

    // After initial scan, default this to 1 for on-demand expansion
    args.max_depth = 1;

    let mut app =
        span!(Level::DEBUG, "Initializing app").in_scope(|| App::new(args.clone(), scanned));

    ratatui::run(|terminal| app.run(terminal))
}
