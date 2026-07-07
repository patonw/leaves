use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use color_eyre::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use humansize::{DECIMAL, format_size};
use ignore::{WalkState, overrides::OverrideBuilder};
use itertools::Itertools as _;
use ratatui::{
    DefaultTerminal, Frame,
    layout::Constraint,
    text::{Line, Text},
    widgets::{Block, Paragraph},
};

use crate::cli::Args;
use crate::core::{Entry, Forest};
use crate::forest::par_forest;
use crate::util::file_color;

#[derive(Default, Clone)]
pub struct ScanState {
    pub done: bool,
    pub path: PathBuf,
    pub count: usize,
    pub total: usize,
}

pub fn walk_fs(args: &Args, state: Arc<Mutex<ScanState>>) -> Result<Forest> {
    let root = args.path.canonicalize()?;

    let rx = spawn_walker(args, state, root)?;

    Ok(par_forest(args, &args.path, rx, None))
}

pub fn spawn_walker(
    args: &Args,
    state: Arc<Mutex<ScanState>>,
    root: impl AsRef<Path>,
) -> Result<mpsc::Receiver<Entry>, eyre::Error> {
    let (tx, rx) = mpsc::channel();
    let mut overrides = OverrideBuilder::new(&args.path);
    for glob in &args.overrides {
        overrides.add(glob)?;
    }

    let walker = ignore::WalkBuilder::new(&root)
        .overrides(overrides.build()?)
        .hidden(!args.include_hidden)
        .ignore(!args.include_ignored)
        .git_ignore(!args.include_gitignored)
        .git_exclude(!args.include_gitexcluded)
        .same_file_system(!args.cross_fs)
        .build_parallel();

    std::thread::spawn(move || {
        walker.run(move || {
            let tx = tx.clone();
            // TODO: lock-free scan state
            let state = state.clone();

            Box::new(move |result| {
                match result {
                    Ok(ent) => {
                        {
                            let mut state = state.lock().unwrap();
                            state.count += 1;
                            state.path = ent.path().into();
                        }

                        let Ok(metadata) = ent.metadata() else {
                            return WalkState::Continue;
                        };
                        if metadata.is_file() && metadata.len() > 0 {
                            let mut state = state.lock().unwrap();
                            state.total += metadata.len() as usize;
                        } else {
                            return WalkState::Continue;
                        }

                        let color = file_color(ent.path());
                        let entry = Entry {
                            path: ent.path().into(),
                            size: metadata.len() as usize,
                            nfiles: 1,
                            leaves: 1,
                            color,
                            ..Default::default()
                        };

                        if tx.send(entry).is_err() {
                            return WalkState::Quit;
                        }
                    }
                    Err(err) => tracing::warn!("{}", err),
                }

                WalkState::Continue
            })
        });
    });

    Ok(rx)
}

#[derive(Default)]
pub struct ScanUI {
    done: bool,
    quit: bool,
    state: Arc<Mutex<ScanState>>,
}

impl ScanUI {
    pub fn new(state: Arc<Mutex<ScanState>>) -> Self {
        Self {
            state,
            ..Default::default()
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<bool> {
        while !self.quit && !self.done {
            let state = {
                let state = self.state.lock().unwrap();
                self.done = state.done;
                (*state).clone()
            };

            terminal.draw(|frame| self.draw(frame, state))?;
            self.handle_events()?;
        }

        Ok(self.quit)
    }

    fn draw(&self, frame: &mut Frame, state: ScanState) {
        let lines = vec![
            format!("Scanning {}", state.path.display()),
            format!("Count: {}", state.count),
            format!("Total: {}", format_size(state.total, DECIMAL)),
        ]
        .into_iter()
        .map(Line::from)
        .collect_vec();
        let text = Text::from(lines);
        let area = frame.area().centered(
            Constraint::Percentage(90),
            Constraint::Length(4 + text.height() as u16),
        );

        frame.render_widget(
            Paragraph::new(text).block(Block::bordered().padding(1.into())),
            area,
        );
    }

    fn handle_events(&mut self) -> Result<()> {
        if !crossterm::event::poll(Duration::from_millis(100))? {
            return Ok(());
        }

        match crossterm::event::read()? {
            // it's important to check that the event is a key press event as
            // crossterm also emits key release and repeat events on Windows.
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                if let KeyCode::Char('q') = key_event.code {
                    self.quit = true;
                }
            }
            _ => {}
        }

        Ok(())
    }
}
