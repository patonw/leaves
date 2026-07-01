use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use color_eyre::Result;
use crossterm::ExecutableCommand as _;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, MouseEventKind, poll};
use either::Either;
use eyre::Context as _;
use humansize::{DECIMAL, format_size};
use ignore::WalkState;
use ignore::overrides::OverrideBuilder;
use itertools::Itertools as _;
use ratatui::layout::{Constraint, Direction, Layout, Position};
use ratatui::style::{Color, Modifier, Style, Stylize as _};
use ratatui::symbols;
use ratatui::text::{Line, Text, ToLine as _, ToSpan};
use ratatui::widgets::{
    Borders, Fill, Padding, Paragraph, ScrollbarOrientation, StatefulWidget, Wrap,
};
use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Widget},
};

use thousands::Separable;
use tracing::instrument;
use tui_tree_widget::{Scrollbar, Tree, TreeItem, TreeState};

use clap::Parser;

const ENTRY_CHUNK_SIZE: usize = 5000;

#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Scanning root path
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Group files in a directory by extension
    #[arg(short, long)]
    group: bool,

    // Partition top-level by file type
    #[arg(short, long)]
    xray: bool,

    /// Don't *automatically* skip any files. Only overrides will be used.
    #[arg(short = 'A', long)]
    include_all: bool,

    /// Don't skip hidden files and folders
    #[arg(short = 'H', long)]
    include_hidden: bool,

    /// Don't skip .ignore'd files
    #[arg(short = 'I', long)]
    include_ignored: bool,

    /// Don't skip .gitignore'd files and folders
    #[arg(short = 'G', long)]
    include_gitignored: bool,

    /// Don't skip files and folders listed in .git/info/exclude
    #[arg(short = 'E', long)]
    include_gitexcluded: bool,

    /// Git-style override globs. '!' prefix negates glob
    overrides: Vec<String>,
}

type Forest = Vec<(usize, Entry)>;
type TreeSlice<'a> = &'a [(usize, Entry)];
type LineageMap = HashMap<(PathBuf, Option<OsString>), HashSet<Entry>>;

#[derive(Clone, Default)]
pub struct StackAddr<'a>(Option<(usize, &'a StackAddr<'a>)>);

impl<'a> Iterator for &StackAddr<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        let (data, prev) = self.0.as_ref()?;
        *self = *prev;
        Some(*data)
    }
}

impl<'a> StackAddr<'a> {
    pub fn root() -> Self {
        Self::default()
    }

    pub fn push(&'a self, data: usize) -> Self {
        Self(Some((data, self)))
    }
}

pub fn tree_find_path(
    tree: TreeSlice,
    path: &Path,
    tag: Option<&OsStr>,
    addr: &StackAddr,
) -> Option<Vec<usize>> {
    for (id, entry) in tree {
        let addr = addr.push(*id);
        let mut found = entry.path == path;

        if let Some(stag) = tag
            && let Some(etag) = &entry.tag
        {
            found = found && stag == etag;
        }

        if found {
            let mut result = addr.collect_vec();
            result.reverse();
            return Some(result);
        }

        if let Some(result) = tree_find_path(&entry.subtree, path, tag, &addr) {
            return Some(result);
        }
    }

    None
}

fn prune_view(forest: &mut Vec<(usize, Entry)>, view_addr: &[usize]) -> Vec<(usize, Entry)> {
    // Traverse twice instead of using a parent var to appease borrow checker
    // Could also use an parent + Some(child_idx) to represent cursor, but that just makes
    // the single traversal more complicated for little practical gain.
    let mut addr = Vec::new();
    let mut cursor = &*forest;
    for id in view_addr {
        if let Ok(idx) = cursor.binary_search_by_key(id, |(id, _)| *id) {
            addr.push(idx);
            cursor = &cursor[idx].1.subtree;
        } else {
            break;
        }
    }

    // Second traversal to the parent Vec, then splice out the entry
    // to ensure we don't leave dangling empty directories that will
    // be confused as empty leaves.
    if let Some(last_idx) = addr.pop() {
        let mut cursor = forest;
        for idx in addr {
            cursor = &mut cursor[idx].1.subtree;
        }

        let (_, entry) = cursor.remove(last_idx);

        entry.subtree
    } else {
        std::mem::take(forest)
    }
}

#[derive(Default, Clone, Debug, Hash, Eq, PartialEq)]
pub struct Entry {
    path: PathBuf,
    tag: Option<OsString>,
    size: usize,
    nfiles: usize,
    subtree: Forest,
    color: Color,
    is_group: bool,
}

// TODO: consolidation/composition
#[derive(Default, Clone, Debug, Hash, Eq, PartialEq)]
pub struct EntryInfo {
    path: PathBuf,
    tag: Option<OsString>,
    size: usize,
    nfiles: usize,
}

impl From<&Entry> for EntryInfo {
    fn from(value: &Entry) -> Self {
        let Entry {
            path,
            tag,
            size,
            nfiles,
            ..
        } = value;

        Self {
            path: path.clone(),
            tag: tag.clone(),
            size: *size,
            nfiles: *nfiles,
        }
    }
}

#[ouroboros::self_referencing]
pub struct TreeFocus {
    tree: Forest,

    #[borrows(tree)]
    #[covariant]
    focus: Option<&'this Entry>,
}

impl Default for TreeFocus {
    fn default() -> Self {
        TreeFocusBuilder {
            tree: Default::default(),
            focus_builder: |_| None,
        }
        .build()
    }
}

impl TreeFocus {
    pub fn select(&mut self, selection: &[usize]) {
        self.with_mut(|fields| {
            *fields.focus = get_selection(selection, fields.tree);
        });
    }
}

#[derive(Debug, Clone)]
enum MaybePair<T>
where
    T: std::fmt::Debug + Clone,
{
    One(T),
    Two(T, T),
}

impl<P: AsRef<Path>> From<(P, usize)> for Entry {
    fn from((path, size): (P, usize)) -> Self {
        let path = PathBuf::from(path.as_ref());
        Self {
            path,
            size,
            ..Default::default()
        }
    }
}

fn dir_color(dir_path: impl AsRef<Path>) -> Color {
    let mut h = DefaultHasher::default();
    format!(
        "{}",
        dir_path.as_ref().file_name().unwrap_or_default().display()
    )
    .hash(&mut h);
    let id = h.finish();

    // let color = colorous::TABLEAU10[id as usize % 10];
    let color = colorous::VIRIDIS.eval_rational(id as usize, u64::MAX as usize);
    Color::from(color.into_tuple())
}

fn regroup(entries: Vec<Entry>) -> Vec<Entry> {
    let mut groups: HashMap<OsString, Vec<Entry>> = Default::default();
    entries
        .into_iter()
        .map(|it| {
            let ext = it
                .path
                .extension()
                .or_else(|| it.path.file_name())
                .unwrap_or_default()
                .to_owned();

            (ext, it)
        })
        .for_each(|(k, v)| {
            groups.entry(k).or_default().push(v);
        });

    let mut entries: Vec<_> = groups
        .into_iter()
        .map(|(k, mut v)| {
            if v.len() == 1 {
                v.pop().unwrap()
            } else {
                let label = format!("*.{}", k.display());
                let size = v.iter().map(|it| it.size).sum();
                let count = v.iter().map(|it| it.nfiles).sum();
                let color = v[0].color;
                let subtree = cumsum_size(v);

                Entry {
                    path: label.into(),
                    size,
                    nfiles: count,
                    subtree,
                    color,
                    is_group: true,
                    ..Default::default()
                }
            }
        })
        .collect();

    entries.sort_by_key(|it| Reverse(it.size));
    entries
}

fn cumsum_size(entries: Vec<Entry>) -> Vec<(usize, Entry)> {
    entries
        .into_iter()
        .scan(0, |acc, it| {
            let start = *acc;
            *acc += it.size;
            Some((start, it))
        })
        .collect()
}

fn walk_fs(args: &Args, state: Arc<Mutex<ScanState>>) -> Result<Forest> {
    let (tx, rx) = mpsc::channel();
    let root = args.path.canonicalize()?;
    // let mut leaves: Vec<Entry> = Default::default();

    let mut overrides = OverrideBuilder::new(&root);
    for glob in &args.overrides {
        overrides.add(glob)?;
    }

    let walker = ignore::WalkBuilder::new(&root)
        .overrides(overrides.build()?)
        .hidden(!args.include_hidden)
        .ignore(!args.include_ignored)
        .git_ignore(!args.include_gitignored)
        .git_exclude(!args.include_gitexcluded)
        .build_parallel();

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

    Ok(par_forest(args, &args.path, rx, None))
}

// Creates forests from leaves in chunks, then merges the results.
fn _par_forest(
    args: &Args,
    root: impl AsRef<Path>,
    leaves: impl IntoIterator<Item = Entry>,
    _num_workers: Option<usize>,
) -> Vec<(usize, Entry)> {
    use itertools::Itertools as _;
    use rayon::prelude::*;

    let root = root.as_ref().canonicalize().unwrap_or(args.path.clone());

    // path-based chunking might make for quicker merges... maybe
    let chunks = leaves
        .into_iter()
        .chunks(50_000)
        .into_iter()
        .map(|chunk| chunk.collect_vec())
        .collect_vec();

    chunks
        .into_par_iter()
        .map({
            let args = args.clone();
            let root = root.clone();

            move |chunk| make_forest(&args, &root, chunk)
        })
        .reduce(Vec::new, merge_forests)
}

fn par_forest(
    args: &Args,
    root: impl AsRef<Path>,
    leaves: impl IntoIterator<Item = Entry>,
    est_count: Option<usize>,
) -> Vec<(usize, Entry)> {
    use crossbeam_channel::unbounded;
    use itertools::Itertools as _;
    use std::thread;

    let root = root.as_ref().canonicalize().unwrap_or(args.path.clone());

    let num_workers = est_count.map(|c| c / ENTRY_CHUNK_SIZE / 10);
    let num_cores = thread::available_parallelism()
        .map(|c| c.get())
        .unwrap_or(1);

    let num_threads = num_workers.map(|x| x.min(num_cores)).unwrap_or(num_cores);

    if num_threads <= 1 {
        return make_forest(args, root, leaves);
    }

    thread::scope(|ts| {
        let (tx, rx) = unbounded::<Vec<Entry>>();

        // Create forests in parallel from chunks of entries.
        // Use chunking to reduce overhead from channel
        let mut handles = (0..num_threads)
            .map(|_| {
                ts.spawn({
                    let args = args.clone();
                    let root = root.clone();
                    let rx = rx.clone();
                    move || make_forest(&args, &root, rx.into_iter().flatten())
                })
            })
            .map(Either::Left)
            .collect_vec();

        drop(rx);

        for it in leaves.into_iter().chunks(ENTRY_CHUNK_SIZE).into_iter() {
            // hmmm, how to reduce allocations here?
            if tx.send(it.collect_vec()).is_err() {
                break;
            }
        }

        drop(tx);

        // Reduction loop to a single forest
        loop {
            let mut results = handles
                .into_iter()
                .map(|h| match h {
                    Either::Left(h) => h.join().expect("Couldn't join threads"),
                    Either::Right(v) => v,
                })
                .collect_vec();

            if results.len() <= 1 {
                let Some(result) = results.pop() else {
                    return Vec::default();
                };

                return result;
            }

            handles = results
                .into_iter()
                .chunks(2)
                .into_iter()
                .map(|mut chunk| {
                    let Some(left) = chunk.next() else {
                        return Either::Right(Vec::new());
                    };

                    let Some(right) = chunk.next() else {
                        return Either::Right(left);
                    };

                    Either::Left(ts.spawn(|| merge_forests(left, right)))
                })
                .collect_vec();
        }
    })
}

// Move entries instead of slicing to reduce allocations. Still need to allocate for interior
// nodes, but should be an order of magnitude less.
fn make_forest(
    args: &Args,
    root: impl AsRef<Path>,
    leaves: impl IntoIterator<Item = Entry>,
) -> Vec<(usize, Entry)> {
    let root = root.as_ref().canonicalize().unwrap_or(args.path.clone());
    let (kidding, extensions) = rehash(args, leaves);

    let mut kidding = kidding;

    if args.xray {
        let mut entries = extensions
            .iter()
            .map(|ext| {
                let subtree = treeify(args, &mut kidding, &root, &Some(ext.clone()));
                let size = subtree.iter().map(|(_, it)| it.size).sum();
                let nfiles = subtree.iter().map(|(_, it)| it.nfiles).sum();

                let label = if ext.is_empty() {
                    "(none)".into()
                } else {
                    format!("**.{}", ext.display())
                };

                let color = ext_color(ext);
                let path = root.join(label);
                Entry {
                    path,
                    size,
                    nfiles,
                    subtree,
                    color,
                    is_group: true,
                    ..Default::default()
                }
            })
            .collect_vec();
        entries.sort_by_key(|it| Reverse(it.size));
        cumsum_size(entries)
    } else {
        treeify(args, &mut kidding, &root, &Default::default())
    }
}

pub struct LeafIterator {
    entries: VecDeque<Entry>,
}

impl Iterator for LeafIterator {
    type Item = Entry;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(entry) = self.entries.pop_front() {
            if entry.subtree.is_empty() {
                return Some(entry);
            }

            for (_, it) in entry.subtree {
                self.entries.push_front(it);
            }
        }

        None
    }
}

pub fn into_leaves(entries: impl Iterator<Item = Entry>) -> LeafIterator {
    let entries = entries.into_iter().collect();
    LeafIterator { entries }
}

fn deforest(forest: Forest) -> LeafIterator {
    into_leaves(forest.into_iter().map(|(_, it)| it))
}

/// Combine subtrees of two entries with the same path & tag
fn merge_entries(mut left: Entry, right: Entry) -> Entry {
    assert_eq!(left.path, right.path);
    assert_eq!(left.tag, right.tag);

    left.subtree = merge_forests(left.subtree, right.subtree);
    left.size += right.size;
    left.nfiles += right.nfiles;

    left
}

fn merge_forests(left: Forest, right: Forest) -> Vec<(usize, Entry)> {
    let queue = left.into_iter().chain(right).map(|(_, it)| it);
    // .sorted_by_key(|it| (it.path.to_path_buf(), it.tag.clone()))
    // .collect();

    // let mut combined: Vec<Entry> = vec![];
    //
    // if let Some(first) = queue.pop_front() {
    //     combined.push(first);
    // }
    //
    // while let Some(cursor) = combined.pop()
    //     && let Some(other) = queue.pop_front()
    // {
    //     if cursor.path == other.path && cursor.tag == other.tag {
    //         combined.push(merge_entries(cursor, other));
    //     } else {
    //         combined.push(cursor);
    //         combined.push(other);
    //     }
    // }

    let mut crash = HashMap::new();

    for it in queue {
        let key = (it.path.clone(), it.tag.clone());
        if let Some(other) = crash.remove(&key) {
            crash.insert(key, merge_entries(other, it));
        } else {
            crash.insert(key, it);
        }
    }

    let mut combined = crash.into_values().collect_vec();
    combined.sort_by_key(|it| Reverse(it.size));
    cumsum_size(combined)
}

fn rehash(args: &Args, leaves: impl IntoIterator<Item = Entry>) -> (LineageMap, HashSet<OsString>) {
    let mut kidding: LineageMap = Default::default();
    let mut extensions: HashSet<OsString> = Default::default();

    for entry in leaves {
        let ext = if args.xray {
            // entry.path.extension().map(|s| s.to_os_string())
            Some(entry.path.extension().unwrap_or_default().to_os_string())
        } else {
            None
        };

        if let Some(ext) = &ext {
            extensions.insert(ext.clone());
        }

        let mut cursor = entry;

        while let Some(parent) = cursor.path.parent() {
            let parent = parent.to_path_buf();
            let siblings = kidding.entry((parent.clone(), ext.clone())).or_default();
            if siblings.contains(&cursor) {
                break;
            }

            siblings.insert(cursor);
            let color = dir_color(&parent);
            cursor = Entry {
                path: parent,
                color,
                tag: ext.clone(),
                ..Default::default()
            }
        }
    }
    (kidding, extensions)
}

fn treeify(args: &Args, kidding: &mut LineageMap, path: &Path, ext: &Option<OsString>) -> Forest {
    let key = (path.to_path_buf(), ext.clone());
    let Some(entries) = kidding.remove(&key) else {
        return Default::default();
    };

    let mut entries = entries
        .into_iter()
        .map(|mut it| {
            let subtree = treeify(args, kidding, &it.path, ext);
            if !subtree.is_empty() {
                if it.size == 0 {
                    it.size = subtree.iter().map(|(_, it)| it.size).sum();
                }
                if it.nfiles == 0 {
                    it.nfiles = subtree.iter().map(|(_, it)| it.nfiles).sum();
                }
                it.subtree = subtree;
            }

            it
        })
        .collect_vec();

    entries.sort_by_key(|it| Reverse(it.size));

    if !args.xray && args.group {
        entries = regroup(entries);
    }

    cumsum_size(entries)
}

fn file_color(file_path: impl AsRef<Path>) -> Color {
    if let Some(ext) = file_path.as_ref().extension() {
        ext_color(ext)
    } else {
        Color::Reset
    }
}

fn ext_color(ext: &OsStr) -> Color {
    if ext.is_empty() {
        return Color::Reset;
    }

    let mut h = DefaultHasher::default();
    format!("{}", ext.display()).hash(&mut h);
    let id = h.finish();

    // let color = colorous::TABLEAU10[id as usize % 10];
    let color = colorous::YELLOW_ORANGE_BROWN.eval_rational(id as usize, u64::MAX as usize);
    Color::from(color.into_tuple())
}

#[instrument]
fn main() -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    color_eyre::install()?;

    let mut args = Args::parse();

    if args.include_all {
        args.include_hidden = true;
        args.include_ignored = true;
        args.include_gitignored = true;
        args.include_gitexcluded = true;
    }

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

    let quit = ratatui::run(|term| ScanUI::new(scan_state).run(term))?;
    if quit {
        return Ok(());
    }

    let scanned = th
        .join()
        .map_err(|_e| eyre::eyre!("Failed to spawn scanner"))??;

    let mut app = App::new(args.clone(), scanned);

    ratatui::run(|terminal| app.run(terminal))
}

fn partition(whole: TreeSlice) -> MaybePair<TreeSlice> {
    if whole.len() <= 1 {
        return MaybePair::One(whole);
    }

    let range = key_range(whole).unwrap();

    let start = range.start;
    let end = range.end;

    let half = (start + end) / 2;

    let idx = whole.partition_point(|it| it.0 < half);
    if idx > 0 && idx < whole.len() - 1 {
        let left = &whole[..idx];
        let right = &whole[idx..];
        MaybePair::Two(left, right)
    } else if whole.len() > 1 {
        MaybePair::Two(&whole[..1], &whole[1..])
    } else {
        MaybePair::One(whole)
    }
}

fn key_range(whole: TreeSlice) -> Option<Range<usize>> {
    if whole.is_empty() {
        return None;
    }

    let (start, _) = whole[0];

    let end = whole.last().unwrap();
    let end = end.0 + end.1.size;

    Some((start)..end)
}

#[derive(Default, Clone)]
pub struct ScanState {
    done: bool,
    path: PathBuf,
    count: usize,
    total: usize,
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
        if !poll(Duration::from_millis(100))? {
            return Ok(());
        }

        match event::read()? {
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

pub struct App {
    args: Args,
    exit: bool,

    entries: TreeFocus,
    reserve: Vec<Entry>,

    tree_items: Vec<TreeItem<'static, usize>>,
    selection: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Default)]
enum AppMode {
    #[default]
    Normal,
    Xray,
    Scatter,
}

#[derive(Default)]
pub struct AppState {
    root: PathBuf,
    mode: AppMode,
    mode_switch: Option<AppMode>,
    diagnostic: bool,

    view_info: Option<EntryInfo>,
    title: Option<OsString>,
    skip_view: Vec<usize>,
    tree_state: TreeState<usize>,
    tag: Option<OsString>,

    click_pos: Option<Position>,
    click_area: Rect,
    click_addr: Vec<usize>,
}

impl App {
    pub fn new(args: Args, entries: Forest) -> Self {
        let tree_items = par_tree_items(entries.as_slice());
        let focus = TreeFocusBuilder {
            tree: entries.clone(),
            focus_builder: |_| None,
        }
        .build();

        Self {
            args,
            exit: false,
            entries: focus,
            reserve: Default::default(),
            tree_items,
            selection: Default::default(),
        }
    }

    /// runs the application's main loop until the user quits
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let _ = terminal
            .backend_mut()
            .execute(crossterm::event::EnableMouseCapture);

        let mode = if self.args.xray {
            AppMode::Xray
        } else {
            AppMode::Normal
        };

        let mut state = AppState {
            root: self.args.path.to_path_buf(),
            mode,
            ..AppState::default()
        };

        while !self.exit {
            if let Some(mode) = state.mode_switch {
                let restore_info = self.view_info(&state);
                let restore_view = restore_info.path.as_path();
                let restore_path = self.entries.borrow_focus().and_then(|it| {
                    if it.is_group {
                        it.path.parent().map(|p| p.to_path_buf())
                    } else {
                        Some(it.path.to_path_buf())
                    }
                });
                let restore_tag = state.tag.clone();

                let entries = std::mem::take(&mut self.entries);
                let mut forest = entries.into_heads().tree;

                let (leaves, count) = match mode {
                    AppMode::Normal => {
                        self.args.xray = false;
                        self.reserve
                            .extend_from_slice(&deforest(forest).collect_vec());
                        state.root = self.args.path.clone();
                        let items = std::mem::take(&mut self.reserve);
                        let count = items.len();
                        (Either::Left(items.into_iter()), Some(count))
                    }
                    AppMode::Xray => {
                        self.args.xray = true;
                        state.root = self.args.path.clone();
                        let count = key_range(self.entries.borrow_tree()).map(|r| r.end);
                        (Either::Right(deforest(forest)), count)
                    }
                    AppMode::Scatter => {
                        self.args.xray = true;

                        let count = state.view_info.as_ref().map(|it| it.nfiles);
                        let pruned = prune_view(&mut forest, state.skip_view.as_slice());
                        self.reserve = deforest(forest).collect_vec();
                        state.root = restore_view.to_path_buf();

                        (Either::Right(deforest(pruned)), count)
                    }
                };

                // TODO: just create a new UI for this with a proper progress bar
                terminal.draw(|frame| {
                    let text = Text::raw("Recalculating. Please hold...");
                    let area = frame.area().centered(
                        Constraint::Length(text.width() as u16),
                        Constraint::Length(1),
                    );
                    frame.render_widget(text, area);
                })?;

                let tree = par_forest(&self.args, &state.root, leaves, count);

                self.entries = TreeFocusBuilder {
                    tree,
                    focus_builder: |_| None,
                }
                .build();

                self.tree_items = par_tree_items(self.entries.borrow_tree());

                self.selection = Default::default();
                state.tag = restore_tag.clone();
                state.title = Some(restore_view.as_os_str().to_os_string());
                state.mode = mode;
                state.mode_switch = Default::default();
                state.click_pos = Default::default();
                state.click_addr = Default::default();
                state.tree_state = Default::default();
                state.skip_view = Default::default();

                if let Some(addr) = tree_find_path(
                    self.entries.borrow_tree(),
                    restore_view,
                    restore_tag.as_deref(),
                    &StackAddr::root(),
                ) {
                    state.skip_view = addr;
                }

                if let Some(path) = restore_path
                    && let Some(addr) = tree_find_path(
                        self.entries.borrow_tree(),
                        &path,
                        restore_tag.as_deref(),
                        &StackAddr::root(),
                    )
                {
                    state.click_addr = addr.into_iter().skip(state.skip_view.len()).collect_vec();
                }

                self.sync_view(&mut state);
            }

            if state.view_info.is_none() {
                let info = self.view_info(&state);

                let title = get_title(&state, &info);
                state.title = Some(title);
                state.view_info = Some(info);
            }

            if !state.click_addr.is_empty() {
                let addr = std::mem::take(&mut state.click_addr);
                let mut selection = state.skip_view.clone();
                selection.extend_from_slice(&addr);
                self.entries.select(&selection);

                for i in 0..addr.len() {
                    state.tree_state.open(addr[..i].to_vec());
                }

                state.tree_state.select(addr);

                state.click_pos = None;
            }

            self.selection.clear();
            self.selection
                .extend_from_slice(state.tree_state.selected());

            terminal.draw(|frame| self.draw(frame, &mut state))?;
            if self.handle_events(&mut state)? {
                let mut selection = state.skip_view.clone();
                selection.extend(state.tree_state.selected());

                self.entries.select(&selection);
            }

            if let Some(entry) = self.entries.borrow_focus() {
                if let Some(tag) = &entry.tag {
                    state.tag = Some(tag.clone());
                } else if entry.subtree.is_empty() || entry.is_group {
                    state.tag = Some(entry.path.extension().unwrap_or_default().to_os_string());
                }
            }
        }

        Ok(())
    }

    fn draw(&self, frame: &mut Frame, state: &mut AppState) {
        let mut window = Block::bordered();
        let area = window.inner(frame.area());

        state.click_area = area;

        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Max(50), Constraint::Fill(10)])
            .split(area);

        let sidebar = Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![
                Constraint::Fill(10),
                // Constraint::Length(3),
                Constraint::Percentage(25),
            ])
            .split(layout[0]);

        let mut title = vec![];
        if let Some(info) = &state.view_info {
            if let Ok(rel_path) = info.path.strip_prefix(&state.root) {
                // TODO: avoid converting to C-string
                let root = format!("{}", state.root.display());
                title.push(root.bold());
                if !rel_path.as_os_str().is_empty() {
                    let rel_path = format!("/{}", rel_path.display());
                    title.push(rel_path.into())
                }
            } else {
                title.push(format!("{}", info.path.display()).into())
            }

            if let Some(tag) = &info.tag {
                title.push("/**.".into());
                if tag.is_empty() {
                    title.push("(none)".green().bold())
                } else {
                    title.push(format!("{}", tag.display()).green().bold())
                }
            }

            title.push(" | ".into());
            title.push(format_size(info.size, DECIMAL).bold());
            title.push(format!(" ({} files)", info.nfiles.separate_with_commas()).into());
        } else {
            title.push("leaves".cyan().bold())
        }

        let title_text = Line::from(title);
        window = window.title(title_text.centered());

        let mut status_line = match state.mode {
            AppMode::Normal => Line::from(vec![
                " Mode: normal ".into(),
                " | Keys: ".bold(),
                " X-Ray mode ".into(),
                "x ".blue().bold(),
                " Scatter mode ".into(),
                "X ".blue().bold(),
            ]),

            AppMode::Xray => Line::from(vec![
                " Mode: x-ray ".into(),
                " | Keys: ".bold(),
                " Normal mode ".into(),
                "x".blue().bold(),
                "/".into(),
                "X ".blue().bold(),
            ]),
            AppMode::Scatter => Line::from(vec![
                " Mode: scatter ".into(),
                " | Keys: ".bold(),
                " Normal mode ".into(),
                "x".blue().bold(),
                "/".into(),
                "X ".blue().bold(),
            ]),
        };

        if !state.tree_state.selected().is_empty() {
            status_line.push_span(" Focus ".to_span());
            status_line.push_span("<Enter> ".blue().bold());
        }

        if !state.skip_view.is_empty() {
            status_line.push_span(" Defocus ".to_span());
            status_line.push_span("<Back> ".blue().bold());
        }

        status_line.push_span(" Quit ".to_span());
        status_line.push_span("q ".blue().bold());

        window = window.title_bottom(status_line.centered());

        frame.render_widget(window, frame.area());

        let widget = Tree::new(&self.tree_items)
            .expect("all item identifiers are unique")
            .block(
                Block::new()
                    .borders(Borders::BOTTOM)
                    .border_type(ratatui::widgets::BorderType::Double)
                    .padding(Padding::proportional(1)),
            )
            .experimental_scrollbar(Some(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .track_symbol(None)
                    .end_symbol(None),
            ))
            .highlight_style(
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");

        frame.render_stateful_widget(widget, sidebar[0], &mut state.tree_state);

        let mut text = vec![];
        if state.diagnostic {
            text.extend_from_slice(&[
                "--- Diagnostics ---".into(),
                format!("VW {:?}", &state.skip_view),
                format!("SL {:?}", state.tree_state.selected()),
                format!("TG {:?}", &state.tag),
                "".into(),
                "--- File info ---".into(),
            ]);
        }

        if let Some(entry) = self.entries.borrow_focus() {
            text.extend_from_slice(&[
                format!("{}", entry.path.display()),
                "".into(),
                format!(
                    "tag: {}",
                    entry
                        .tag
                        .as_deref()
                        .or_else(|| entry.path.extension())
                        .unwrap_or(OsStr::new("(none)"))
                        .display()
                ),
            ]);

            if entry.nfiles > 1 {
                // is dir
                text.push(format!("files: {}", &entry.nfiles.separate_with_commas()));
            } else {
                // is file
                text.push(format!("bytes: {}", &entry.size.separate_with_commas()));
            }
        }

        let text = Text::from(text.into_iter().map(Line::from).collect_vec());

        frame.render_widget(
            Paragraph::new(text)
                .block(Block::new().padding(Padding::proportional(1)))
                .wrap(Wrap { trim: false }),
            sidebar[1],
        );

        frame.render_stateful_widget(self, layout[1], state);
    }

    fn handle_events(&mut self, state: &mut AppState) -> Result<bool> {
        let dirty = match event::read()? {
            // it's important to check that the event is a key press event as
            // crossterm also emits key release and repeat events on Windows.
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                self.handle_key_event(state, key_event)
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => state.tree_state.scroll_down(1),
                MouseEventKind::ScrollUp => state.tree_state.scroll_up(1),
                MouseEventKind::Down(_button) => {
                    let position = Position::new(mouse.column, mouse.row);
                    state.click_pos = Some(position);
                    state.click_addr.clear();
                    state.tree_state.click_at(position)
                }
                _ => false,
            },
            _ => false,
        };

        Ok(dirty)
    }

    fn handle_key_event(&mut self, state: &mut AppState, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => self.exit(),
            KeyCode::Char('i') => {
                state.diagnostic = !state.diagnostic;
                true
            }
            KeyCode::Char('X') => {
                state.mode_switch = Some(match state.mode {
                    AppMode::Normal => AppMode::Scatter,
                    _ => AppMode::Normal,
                });
                true
            }
            KeyCode::Char('x') => {
                state.mode_switch = Some(match state.mode {
                    AppMode::Normal => AppMode::Xray,
                    _ => AppMode::Normal,
                });
                true
            }
            KeyCode::Char('\n' | ' ') => state.tree_state.toggle_selected(),
            KeyCode::Char('<') => state.tree_state.close_all(),
            KeyCode::Left => state.tree_state.key_left(),
            KeyCode::Right => state.tree_state.key_right(),
            KeyCode::Down => state.tree_state.key_down(),
            KeyCode::Up => state.tree_state.key_up(),
            KeyCode::Esc => state.tree_state.select(Vec::new()),
            KeyCode::Home => state.tree_state.select_first(),
            KeyCode::End => state.tree_state.select_last(),
            KeyCode::PageDown => state.tree_state.scroll_down(3),
            KeyCode::PageUp => state.tree_state.scroll_up(3),
            KeyCode::Enter => {
                state
                    .skip_view
                    .extend_from_slice(state.tree_state.selected());
                self.selection.clear();
                self.sync_view(state);
                true
            }
            KeyCode::Backspace => {
                if let Some(id) = state.skip_view.pop() {
                    self.selection.insert(0, id);
                }
                self.sync_view(state);
                true
            }
            _ => false,
        }
    }

    fn sync_view(&mut self, state: &mut AppState) {
        while let Some(entry) = get_selection(&state.skip_view, self.entries.borrow_tree())
            && entry.subtree.is_empty()
        {
            state.skip_view.pop();
        }

        self.tree_items = par_tree_items(self.get_view(state));
        state.tree_state.select(self.selection.clone());

        for i in 0..self.selection.len() {
            state.tree_state.open(self.selection[..i].to_vec());
        }

        state.view_info = None;
        state.title = None;
    }

    fn exit(&mut self) -> bool {
        self.exit = true;
        false
    }

    fn view_info(&self, state: &AppState) -> EntryInfo {
        let mut result = None;
        let mut cursor = self.entries.borrow_tree();

        for id in state.skip_view.iter() {
            if let Ok(idx) = cursor.binary_search_by_key(id, |(id, _)| *id) {
                result = Some(EntryInfo::from(&cursor[idx].1));
                cursor = &cursor[idx].1.subtree;
            }
        }

        if let Some(info) = result {
            info
        } else {
            let tree = self.entries.borrow_tree();
            let size = tree.iter().map(|(_, it)| it.size).sum();
            let count = tree.iter().map(|(_, it)| it.nfiles).sum();

            EntryInfo {
                path: state.root.clone(),
                size,
                nfiles: count,

                ..Default::default()
            }
        }
    }

    fn get_view(&self, state: &AppState) -> &[(usize, Entry)] {
        state
            .skip_view
            .iter()
            .fold(self.entries.borrow_tree().as_slice(), |view, id| {
                if let Ok(idx) = view.binary_search_by_key(id, |(id, _)| *id) {
                    let subtree = &view[idx].1.subtree;
                    if subtree.is_empty() { view } else { subtree }
                } else {
                    view
                }
            })
    }
}

fn get_title(state: &AppState, info: &EntryInfo) -> OsString {
    let mut title = if info.tag.is_none() {
        info.path.clone().into_os_string()
    } else if let Some(tag) = &info.tag {
        info.path.join("**").with_extension(tag).into_os_string()
    } else {
        state.root.as_os_str().to_os_string()
    };

    title.push(format!(
        " | {} ({} files)",
        format_size(info.size, DECIMAL),
        info.nfiles.separate_with_commas()
    ));
    title
}

fn get_selection<'a>(
    mut selection: &[usize],
    mut level: &'a [(usize, Entry)],
) -> Option<&'a Entry> {
    while let Some(id) = selection.first()
        && let Ok(idx) = level.binary_search_by_key(id, |(k, _)| *k)
    {
        let entry = &level[idx].1;
        if selection.len() == 1 {
            return Some(entry);
        }

        selection = &selection[1..];
        level = &entry.subtree;
    }

    None
}

fn par_tree_items(entries: TreeSlice) -> Vec<TreeItem<'static, usize>> {
    use rayon::prelude::*;

    entries
        .par_iter()
        .map(|(k, v)| {
            let title = v.path.file_name().unwrap_or_default();
            let text = format!("[{}] {}", format_size(v.size, DECIMAL), title.display());

            if v.subtree.is_empty() {
                TreeItem::new_leaf(*k, text)
            } else {
                let subtree = if v.nfiles > ENTRY_CHUNK_SIZE {
                    par_tree_items(v.subtree.as_slice())
                } else {
                    tree_items(v.subtree.as_slice())
                };

                make_tree_node(k, text, subtree)
            }
        })
        .collect()
}

fn tree_items(entries: TreeSlice) -> Vec<TreeItem<'static, usize>> {
    entries
        .iter()
        .map(|(k, v)| {
            let title = v.path.file_name().unwrap_or_default();
            let text = format!("[{}] {}", format_size(v.size, DECIMAL), title.display());
            if v.subtree.is_empty() {
                TreeItem::new_leaf(*k, text)
            } else {
                let subtree = tree_items(v.subtree.as_slice());
                make_tree_node(k, text, subtree)
            }
        })
        .collect_vec()
}

fn make_tree_node(
    k: &usize,
    text: String,
    subtree: Vec<TreeItem<'static, usize>>,
) -> TreeItem<'static, usize> {
    let ids = subtree
        .iter()
        .map(|it| it.identifier())
        .cloned()
        .collect_vec();

    TreeItem::new(*k, text, subtree)
        .context(format!("{ids:?}"))
        .unwrap()
}

impl StatefulWidget for &App {
    type State = AppState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let tree = self.get_view(state);

        render_subtree(state, &StackAddr::root(), area, buf, tree, &self.selection);
    }
}

fn render_subtree(
    state: &mut AppState,
    addr: &StackAddr,
    area: Rect,
    buf: &mut Buffer,
    tree: TreeSlice,
    selection: &[usize],
) {
    if tree.is_empty() {
        return;
    }

    // Can't display useful information if area is too small
    if tree.len() > 1 && (area.height < 2 || area.width <= 2) {
        let head = selection.first();
        let color = tree.first().map(|(_, it)| it.color).unwrap_or_default();
        let style = Style::from(color);
        if tree.iter().any(|(k, _)| Some(k) == head) {
            Fill::new("▓").style(style).render(area, buf);
        } else {
            Fill::new(symbols::DOT).style(style).render(area, buf);
        }

        let addr = addr.push(tree.first().unwrap().0);

        if let Some(click) = &state.click_pos
            && area.contains(*click)
            && state.click_area.intersection(area) == area
        {
            state.click_area = area;
            state.click_addr.clear();
            for id in &addr {
                state.click_addr.push(id)
            }

            state.click_addr.reverse();
        }

        return;
    }

    if tree.len() == 1 {
        let (key, entry) = &tree[0];

        render_entry(state, addr, area, buf, *key, entry, selection);

        return;
    }

    match partition(tree) {
        MaybePair::One(entries) => {
            render_subtree(state, addr, area, buf, entries, selection);
            // Paragraph::new(format!("{entries:?}"))
            //     .centered()
            //     .render(area, buf);
        }
        MaybePair::Two(left, right) => {
            let l = key_range(left).map(|r| (r.end - r.start) as f32).unwrap();
            let r = key_range(right).map(|r| (r.end - r.start) as f32).unwrap();

            // Must interpolate multi-gigabytes down to u16 range
            let lr = (l + r) / 1E5;
            let l = (l / lr) as u16;
            let r = (r / lr) as u16;

            let direction = if area.width > area.height * 2 {
                Direction::Horizontal
            } else {
                Direction::Vertical
            };

            let mut layout = Layout::default()
                .direction(direction)
                .constraints(vec![Constraint::Fill(l), Constraint::Fill(r)])
                .split(area);

            // Ensure tiny left-overs are always represented even if it skews proportions
            if layout[1].width == 0 || layout[1].height == 0 {
                layout = Layout::default()
                    .direction(direction)
                    .constraints(vec![Constraint::Percentage(100), Constraint::Min(1)])
                    .split(area);
            }

            render_subtree(state, addr, layout[0], buf, left, selection);
            render_subtree(state, addr, layout[1], buf, right, selection);
        }
    }
}

fn render_entry(
    state: &mut AppState,
    addr: &StackAddr,
    area: Rect,
    buf: &mut Buffer,
    key: usize,
    entry: &Entry,
    selection: &[usize],
) {
    let Entry {
        path,
        size,
        subtree,
        is_group,
        ..
    } = entry;

    let addr = addr.push(key);
    let title = path.file_name().unwrap_or_default();
    let display = title.display();

    if let Some(click) = &state.click_pos
        && area.contains(*click)
        && state.click_area.intersection(area) == area
    {
        state.click_area = area;
        state.click_addr.clear();
        for id in &addr {
            state.click_addr.push(id)
        }

        state.click_addr.reverse();
    }

    let (selected, selection) = if selection.first() == Some(&key) {
        (true, &selection[1..])
    } else {
        (false, [].as_slice())
    };

    let style = Style::from(entry.color);

    let mut block = Block::bordered()
        .title(display.to_line())
        .border_style(style);

    if area.height > 1 {
        // let mut a = addr.collect_vec();
        // a.reverse();
        // block = block.title_bottom(format!("{a:?}"));
        block = block.title_bottom(format_size(*size, DECIMAL));
    }

    if selected {
        block = block.border_type(ratatui::widgets::BorderType::QuadrantInside);
        // block = block.border_type(ratatui::widgets::BorderType::Thick);
    } else if *is_group || subtree.is_empty() {
        block = block.border_type(ratatui::widgets::BorderType::LightQuadrupleDashed);
    }

    let inner = block.inner(area);
    block.render(area, buf);
    if subtree.is_empty() {
        Fill::new(if selected { "▓" } else { "▒" })
            .style(style)
            .render(inner, buf);
    } else if inner.height > 2 || inner.width > 2 {
        render_subtree(state, &addr, inner, buf, subtree, selection);
    }
}

#[cfg(test)]
mod tests {
    use itertools::Itertools as _;

    use super::StackAddr;

    #[test]
    fn test_addr() {
        let root = StackAddr(None);

        let one = StackAddr(Some((1, &root)));
        let two = one.push(2);

        let all = (&two).collect_vec();
        assert_eq!(all, vec![2, 1]);
    }
}
