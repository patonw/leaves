use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt::Debug;
use std::hash::{DefaultHasher, Hash, Hasher as _};
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
    BorderType, Borders, Fill, Padding, Paragraph, ScrollbarOrientation, StatefulWidget, Wrap,
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

    /// Maximum depth of tree to keep in memory.
    ///
    /// Subtrees below this depth are replaced with summary nodes.
    /// Does not affect scan depth.
    #[arg(short = 'd', long, default_value_t = 5)]
    max_depth: usize,

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

type Forest = Vec<(usize, Entry)>;
type TreeSlice<'a> = &'a [(usize, Entry)];
type LineageMap = HashMap<(PathBuf, Option<OsString>), HashMap<PathBuf, Entry>>;

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

fn prune_entry(
    forest: &mut Vec<(usize, Entry)>,
    view_addr: &[usize],
) -> Either<Entry, Vec<(usize, Entry)>> {
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
        let mut cursor = &mut *forest;
        for idx in &addr {
            cursor = &mut cursor[*idx].1.subtree;
        }

        let (_, entry) = cursor.remove(last_idx);

        // And now a third pass to fix all the counters for surgical edits
        let mut cursor = &mut *forest;
        for idx in &addr {
            let container = &mut cursor[*idx].1;
            container.nfiles -= entry.nfiles;
            container.leaves -= entry.leaves;
            container.size -= entry.size;

            cursor = &mut cursor[*idx].1.subtree;
        }

        Either::Left(entry)
    } else {
        Either::Right(std::mem::take(forest))
    }
}

#[derive(Default, Clone, Debug, Hash, Eq, PartialEq)]
pub struct Entry {
    path: PathBuf,
    tag: Option<OsString>,
    size: usize,
    nfiles: usize,
    leaves: usize,
    subtree: Forest,
    color: Color,
    is_group: bool,
}

#[derive(Default, Clone, Debug)]
pub struct ByPathTag(Entry);

impl Hash for ByPathTag {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.path.hash(state);
        self.0.tag.hash(state);
    }
}

impl PartialEq for ByPathTag {
    fn eq(&self, other: &Self) -> bool {
        self.0.path == other.0.path && self.0.tag == other.0.tag
    }
}

impl Eq for ByPathTag {}

// TODO: consolidation/composition
#[derive(Default, Clone, Debug, Hash, Eq, PartialEq)]
pub struct EntryInfo {
    path: PathBuf,
    tag: Option<OsString>,
    size: usize,
    nfiles: usize,
    leaves: usize,
}

impl From<&Entry> for EntryInfo {
    fn from(value: &Entry) -> Self {
        let Entry {
            path,
            tag,
            size,
            nfiles,
            leaves,
            ..
        } = value;

        Self {
            path: path.clone(),
            tag: tag.clone(),
            size: *size,
            nfiles: *nfiles,
            leaves: *leaves,
        }
    }
}

pub struct DbgEntry<'a>(&'a Entry);

impl<'a> Debug for DbgEntry<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("path", &self.0.path)
            .field("tag", &self.0.tag)
            .field("size", &self.0.size)
            .field("leaves", &self.0.leaves)
            .finish()
    }
}

// Lazy debugging wrapper for forests to avoid allocs if not logging.
pub struct DbgTrees<'a>(TreeSlice<'a>);

impl<'a> Debug for DbgTrees<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tmp = self.0.iter().map(|(i, v)| (*i, DbgEntry(v))).collect_vec();
        Debug::fmt(&tmp, f)
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

#[allow(unused)]
pub fn regroup(entries: Vec<Entry>) -> Vec<Entry> {
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

    let entries: Vec<_> = groups
        .into_iter()
        .map(|(k, mut v)| {
            if v.len() == 1 {
                v.pop().unwrap()
            } else {
                let label = format!("*.{}", k.display());
                let size = v.iter().map(|it| it.size).sum();
                let nfiles = v.iter().map(|it| it.nfiles).sum();
                let leaves = if v.is_empty() {
                    1
                } else {
                    v.iter().map(|it| it.leaves).sum()
                };
                let color = v[0].color;
                let subtree = cumsum_size(v);

                Entry {
                    path: label.into(),
                    size,
                    nfiles,
                    leaves,
                    subtree,
                    color,
                    is_group: true,
                    ..Default::default()
                }
            }
        })
        .collect();

    sort_largest(entries)
}

/// Sorts largest entries first. Ties broken by lexical order.
fn sort_largest(mut entries: Vec<Entry>) -> Vec<Entry> {
    entries.sort_unstable_by(|a, b| b.size.cmp(&a.size).then(a.path.cmp(&b.path)));
    entries
}

/// Transform list of entries, tagging each with cumulative size of preceding siblings.
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
    let root = args.path.canonicalize()?;

    let rx = spawn_walker(args, state, root)?;

    Ok(par_forest(args, &args.path, rx, None))
}

fn spawn_walker(
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

    let (kidding, extensions) = rehash(args, &root, leaves);

    let mut kidding = kidding;

    if args.xray {
        tracing::debug!(
            "making x-ray forest with extensions {extensions:?} from {} entries",
            kidding.len()
        );
        let entries = extensions
            .iter()
            .map(|ext| {
                let subtree = treeify(args, &mut kidding, &root, &Some(ext.clone()));
                let size = subtree.iter().map(|(_, it)| it.size).sum();
                let nfiles = subtree.iter().map(|(_, it)| it.nfiles).sum();
                let leaves = subtree.iter().map(|(_, it)| it.leaves).sum();

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
                    leaves,
                    subtree,
                    color,
                    is_group: true,
                    ..Default::default()
                }
            })
            .collect_vec();
        cumsum_size(sort_largest(entries))
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
    left.leaves = if left.subtree.is_empty() {
        1
    } else {
        left.subtree.iter().map(|(_, it)| it.leaves).sum()
    };

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

    let combined = crash.into_values().collect_vec();
    cumsum_size(sort_largest(combined))
}

fn rehash(
    args: &Args,
    root: impl AsRef<Path>,
    leaves: impl IntoIterator<Item = Entry>,
) -> (LineageMap, HashSet<OsString>) {
    let mut kidding: LineageMap = Default::default();
    let mut extensions: HashSet<OsString> = Default::default();

    let root_depth = root.as_ref().components().count();

    for entry in leaves {
        let ext = entry.path.extension().unwrap_or_default().to_os_string();

        let ext = if args.xray {
            // entry.path.extension().map(|s| s.to_os_string())
            Some(ext)
        } else {
            None
        };

        if let Some(ext) = &ext {
            extensions.insert(ext.clone());
        }

        let color = entry.color;
        let mut cursor = entry;
        let comps = cursor.path.components().skip(root_depth).collect_vec();

        if comps.len() > args.max_depth {
            // Create/Update a summary node and drop entry
            let mut summary_path = root.as_ref().to_path_buf();
            summary_path.extend(comps.iter().take(args.max_depth));
            let summary_parent = summary_path.to_path_buf();

            if let Some(ext) = cursor.path.extension() {
                summary_path.extend(["**"]);
                summary_path.set_extension(ext);
            } else {
                summary_path.extend(["(none)"]);
            }

            let siblings = kidding
                .entry((summary_parent.clone(), ext.clone()))
                .or_default();
            let entry = siblings
                .entry(summary_path.clone())
                .or_insert_with(|| Entry {
                    path: summary_path,
                    color,
                    leaves: 1,
                    is_group: true,
                    ..Default::default()
                });
            entry.nfiles += cursor.nfiles;
            entry.size += cursor.size;

            let color = dir_color(&summary_parent);
            cursor = Entry {
                path: summary_parent,
                color,
                tag: ext.clone(),
                ..Default::default()
            }
        }

        while let Some(parent) = cursor.path.parent() {
            let parent = parent.to_path_buf();
            let path = cursor.path.to_path_buf();
            let siblings = kidding.entry((parent.clone(), ext.clone())).or_default();
            if siblings.contains_key(&path) {
                break;
            }

            siblings.insert(path, cursor);
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

fn treeify(_args: &Args, kidding: &mut LineageMap, path: &Path, ext: &Option<OsString>) -> Forest {
    let key = (path.to_path_buf(), ext.clone());
    let Some(entries) = kidding.remove(&key) else {
        return Default::default();
    };

    let mut entries = entries
        .into_values()
        .map(|mut it| {
            let subtree = treeify(_args, kidding, &it.path, ext);
            if !subtree.is_empty() {
                if it.size == 0 {
                    it.size = subtree.iter().map(|(_, it)| it.size).sum();
                }
                if it.nfiles == 0 {
                    it.nfiles = subtree.iter().map(|(_, it)| it.nfiles).sum();
                }
                it.leaves = subtree.iter().map(|(_, it)| it.leaves).sum();
                it.subtree = subtree;
            } else {
                it.leaves = 1;
            }

            it
        })
        .collect_vec();

    entries = sort_largest(entries);

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

pub fn init_logging() -> Result<()> {
    use tracing_subscriber::{EnvFilter, prelude::*};

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

#[instrument]
fn main() -> Result<()> {
    init_logging()?;
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

    // After initial scan, default this to 1 for on-demand expansion
    args.max_depth = 1;

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

#[derive(Clone, Debug, Default)]
enum AppAction {
    #[default]
    Noop,
    SwitchMode(AppMode),
    Deflate,
    Expand,
}

#[derive(Default)]
pub struct AppState {
    root: PathBuf,
    mode: AppMode,
    action: AppAction,
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

impl AppState {
    /// Address of selection qualified with current view
    pub fn qual_select(&self) -> Vec<usize> {
        let mut selection = self.skip_view.clone();
        selection.extend(self.tree_state.selected());
        selection
    }

    pub fn show_selection(&mut self, addr: &[usize]) {
        for i in 0..addr.len() {
            self.tree_state.open(addr[..i].to_vec());
        }

        self.tree_state.select(addr.to_vec());
    }
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
            self.handle_action(terminal, &mut state)?;

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

                state.show_selection(&addr);

                state.click_pos = None;
            }

            self.selection.clear();
            self.selection
                .extend_from_slice(state.tree_state.selected());

            terminal.draw(|frame| self.draw(frame, &mut state))?;
            if self.handle_events(&mut state)? {
                self.entries.select(&state.qual_select());
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
                Constraint::Percentage(if state.diagnostic { 50 } else { 25 }),
            ])
            .split(layout[0]);

        let mut title = vec![];
        if let Some(info) = &state.view_info {
            if let Ok(rel_path) = info.path.strip_prefix(&state.root) {
                // TODO: avoid converting to C-string
                let root = format!("{}", state.root.display());
                title.push(root.clone().bold());
                if !rel_path.as_os_str().is_empty() {
                    if !root.ends_with('/') {
                        title.push("/".into());
                    }
                    title.push(format!("{}", rel_path.display()).into())
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

            if state.diagnostic {
                title.push(format!(" [{} leaves]", info.leaves.separate_with_commas()).into());
            }
        } else {
            title.push("leaves".cyan().bold())
        }

        let title_text = Line::from(title);
        window = window.title(title_text.centered());

        let mut status_line = match state.mode {
            AppMode::Normal => Line::from(vec![
                " Mode: normal ".into(),
                " | Keys: ".bold(),
                " x".blue().bold(),
                "-ray ".into(),
                " s".blue().bold(),
                "catter ".into(),
            ]),

            AppMode::Xray => Line::from(vec![
                " Mode: x-ray ".into(),
                " | Keys: ".bold(),
                " (".into(),
                "x".blue().bold(),
                "/".into(),
                "s".blue().bold(),
                ")Normal ".into(),
            ]),
            AppMode::Scatter => Line::from(vec![
                " Mode: scatter ".into(),
                " | Keys: ".bold(),
                " (".into(),
                "x".blue().bold(),
                "/".into(),
                "s".blue().bold(),
                ")Normal ".into(),
            ]),
        };

        // Not root or leaf
        let (is_interior, is_group) = self
            .entries
            .borrow_focus()
            .map(|it| (!it.subtree.is_empty(), it.is_group))
            .unwrap_or_default();

        if is_interior && !state.tree_state.selected().is_empty() {
            status_line.push_span(" <Enter>".blue().bold());
            status_line.push_span("Focus ".to_span());
        }

        if !state.skip_view.is_empty() {
            status_line.push_span(" <Back>".blue().bold());
            status_line.push_span("Defocus ".to_span());
        }
        if is_interior && !is_group {
            status_line.push_span(" e".blue().bold());
            status_line.push_span("xpand ".to_span());

            status_line.push_span(" d".blue().bold());
            status_line.push_span("eflate ".to_span());
        }

        status_line.push_span(" +".blue().bold());
        status_line.push_span("/".to_span());
        status_line.push_span("-".blue().bold());
        let depth_help = &format!("depth[{}]", self.args.max_depth);
        status_line.push_span(depth_help.to_span());

        status_line.push_span(" q".blue().bold());
        status_line.push_span("uit ".to_span());

        window = window.title_bottom(status_line.centered());

        frame.render_widget(window, frame.area());

        let widget = Tree::new(&self.tree_items)
            .expect("all item identifiers are unique")
            .block(
                Block::new()
                    .borders(Borders::BOTTOM)
                    .border_type(BorderType::Double)
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

            if state.diagnostic
            // && entry.subtree.is_empty()
            {
                text.push(format!("leaves: {}", &entry.leaves.separate_with_commas()));
            }

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
            KeyCode::Char('+' | '=') => {
                self.args.max_depth += 1;
                true
            }
            KeyCode::Char('-' | '_') => {
                if self.args.max_depth > 1 {
                    self.args.max_depth -= 1;
                }
                true
            }
            KeyCode::Char('i') => {
                state.diagnostic = !state.diagnostic;
                self.entries.select(&state.qual_select());
                state.view_info = None;
                true
            }
            KeyCode::Char('s') => {
                state.action = AppAction::SwitchMode(match state.mode {
                    AppMode::Normal => AppMode::Scatter,
                    _ => AppMode::Normal,
                });
                true
            }
            KeyCode::Char('x') => {
                state.action = AppAction::SwitchMode(match state.mode {
                    AppMode::Normal => AppMode::Xray,
                    _ => AppMode::Normal,
                });
                true
            }
            KeyCode::Char('e') => {
                state.action = AppAction::Expand;
                true
            }
            KeyCode::Char('d') => {
                state.action = AppAction::Deflate;
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
        state.show_selection(&self.selection);

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
            tracing::debug!("Calculating root view {:?}", DbgTrees(tree));

            let size = tree.iter().map(|(_, it)| it.size).sum();
            let nfiles = tree.iter().map(|(_, it)| it.nfiles).sum();
            let leaves = tree.iter().map(|(_, it)| it.leaves).sum();

            EntryInfo {
                path: state.root.clone(),
                size,
                nfiles,
                leaves,
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

    fn handle_action(
        &mut self,
        terminal: &mut DefaultTerminal,
        state: &mut AppState,
    ) -> Result<()> {
        match std::mem::take(&mut state.action) {
            AppAction::Noop => {}
            AppAction::SwitchMode(mode) => {
                let restore_info = self.view_info(state);
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
                        state.root = self.args.path.clone();

                        let count = self.reserve.len()
                            + self
                                .entries
                                .borrow_tree()
                                .iter()
                                .map(|(_, it)| it.leaves)
                                .sum::<usize>();
                        let items = std::mem::take(&mut self.reserve);
                        let items = items.into_iter().chain(deforest(forest));
                        (Either::Left(items), Some(count))
                    }
                    AppMode::Xray => {
                        self.args.xray = true;
                        state.root = self.args.path.clone();
                        let count = self
                            .entries
                            .borrow_tree()
                            .iter()
                            .map(|(_, it)| it.leaves)
                            .sum();
                        (Either::Right(deforest(forest)), Some(count))
                    }
                    AppMode::Scatter => {
                        self.args.xray = true;

                        let count = state.view_info.as_ref().map(|it| it.leaves);
                        let pruned = match prune_entry(&mut forest, state.skip_view.as_slice()) {
                            Either::Left(entry) => entry.subtree,
                            Either::Right(forest) => forest,
                        };
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

                let tree = par_forest(
                    &self.args.with_depth(usize::MAX),
                    &state.root,
                    leaves,
                    count,
                );

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

                self.sync_view(state);
            }
            AppAction::Deflate => {
                let Some(focus) = self.entries.borrow_focus() else {
                    return Ok(());
                };

                if focus.is_group || focus.subtree.is_empty() {
                    return Ok(());
                }

                let target = if focus.is_group {
                    focus.path.parent().map(|p| p.to_path_buf())
                } else {
                    Some(focus.path.to_path_buf())
                };

                let Some(target) = target else { return Ok(()) }; // is that really ok?

                // calculate depth to fold_path
                let depth = target.strip_prefix(&state.root)?.components().count();
                let args = self.args.with_depth(depth);

                // Extract forest into mutable and prune the target subtree
                let entries = std::mem::take(&mut self.entries);
                let mut forest = entries.into_heads().tree;
                let pruned = match prune_entry(&mut forest, &state.qual_select()) {
                    Either::Left(entry) => {
                        if entry.subtree.is_empty() {
                            unreachable!("Cannot fold a leaf");
                        }
                        entry.subtree
                    }
                    Either::Right(_) => unreachable!("Cannot fold root"),
                };

                // Use rehash to summarize node & rebuild tree structure for summaries
                let tree = make_forest(&args, &state.root, deforest(pruned));
                // let (mut kidding, _) = rehash(&args, &root, deforest(pruned));
                // let tree = treeify(&args, &mut kidding, &root, &tag);
                let tree = merge_forests(forest, tree);

                let selection = state.qual_select();

                self.entries = TreeFocusBuilder {
                    tree,
                    focus_builder: |tree| get_selection(&selection, tree),
                }
                .build();

                self.tree_items = par_tree_items(self.get_view(state));
                state.view_info = None;
            }
            AppAction::Expand => {
                let Some(focus) = self.entries.borrow_focus() else {
                    return Ok(());
                };

                tracing::debug!("Expanding node {:?}", DbgEntry(focus));

                let is_group = focus.is_group;
                if focus.subtree.is_empty() && focus.nfiles == 1 && !focus.is_group {
                    return Ok(());
                }

                // Selective group expansion leads to misleading subdirectory representation
                if is_group
                // && !focus.subtree.is_empty()
                {
                    return Ok(());
                }

                let target = if is_group {
                    focus.path.parent().map(|p| p.to_path_buf())
                } else {
                    Some(focus.path.to_path_buf())
                };

                let Some(target) = target else { return Ok(()) }; // is that really ok?

                let tag = focus
                    .tag
                    .clone()
                    .or_else(|| focus.path.extension().map(|s| s.to_os_string()));

                // calculate depth to fold_path
                let scan_root = self.args.path.canonicalize()?;
                let rel_target = target.strip_prefix(&state.root)?;
                let depth = rel_target.components().count();

                tracing::debug!(
                    "Rescanning {target:?}. Root {scan_root:?}. tag {tag:?}. depth {depth}"
                );

                let args = self.args.with_depth(depth + self.args.max_depth);

                // Extract forest into mutable and prune the target subtree
                let entries = std::mem::take(&mut self.entries);
                let mut forest = entries.into_heads().tree;

                // Discard the entry entirely
                let pruned = prune_entry(&mut forest, &state.qual_select());
                match pruned {
                    Either::Left(it) => tracing::debug!("Pruned entry {:?}", DbgEntry(&it)),
                    Either::Right(subtree) => tracing::debug!("Pruned {} subtrees", subtree.len()),
                }

                // should this use state.root instead?
                let rx = spawn_walker(&args, Default::default(), &target)?;

                let leaves = rx.into_iter().filter(|it| {
                    it.path.starts_with(target.as_path())
                        && (tag.is_none()
                            || it.path.extension().unwrap_or_default() == tag.as_ref().unwrap())
                });

                // Use rehash to summarize node & rebuild tree structure for summaries
                let tree = make_forest(&args, &state.root, leaves);

                tracing::debug!("Expanded subtree {:?}", DbgTrees(&tree));

                let tree = merge_forests(forest, tree);

                self.entries = TreeFocusBuilder {
                    tree,
                    focus_builder: |_| None,
                }
                .build();

                self.tree_items = par_tree_items(self.get_view(state));
                if is_group {
                    self.selection.pop();
                    state.tree_state.key_left();

                    // let mut selection = state.skip_view.clone();
                    // selection.extend(&self.selection);
                    // self.entries.select(&selection);
                }

                self.entries.select(&state.qual_select());
                state.view_info = None;
            }
        }

        Ok(())
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
                let subtree = if v.leaves > ENTRY_CHUNK_SIZE {
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
        nfiles,
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
        block = block.border_type(BorderType::QuadrantInside);
    } else if *is_group {
        block = block.border_type(BorderType::Double);
    } else if subtree.is_empty() && *nfiles == 1 {
        block = block.border_type(BorderType::LightDoubleDashed);
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
