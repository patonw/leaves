use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::convert::identity;
use std::ffi::{OsStr, OsString};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use color_eyre::Result;
use crossterm::ExecutableCommand as _;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, MouseEventKind, poll};
use eyre::Context as _;
use humansize::{DECIMAL, format_size};
use ignore::overrides::OverrideBuilder;
use itertools::Itertools as _;
use ratatui::layout::{Constraint, Direction, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Text, ToLine as _};
use ratatui::widgets::{
    Borders, Fill, Padding, Paragraph, ScrollbarOrientation, StatefulWidget, Wrap,
};
use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Widget},
};

use tracing::instrument;
use tui_tree_widget::{Scrollbar, Tree, TreeItem, TreeState};

use clap::Parser;

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

#[derive(Default, Clone, Debug, Hash, Eq, PartialEq)]
pub struct Entry {
    path: PathBuf,
    tag: Option<OsString>,
    size: usize,
    subtree: Forest,
    color: Color,
    is_group: bool,
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

#[allow(unused)]
fn scan_fs<P: AsRef<Path>>(
    state: Arc<Mutex<ScanState>>,
    path: P,
    make_groups: bool,
) -> Result<Forest> {
    let mut entries = std::fs::read_dir(path)?
        .map_ok(|ent| -> Result<_> {
            {
                let mut state = state.lock().unwrap();
                state.count += 1;
                state.path = ent.path();
            }

            let metadata = ent.metadata()?;
            if metadata.is_dir() {
                let subtree = scan_fs(state.clone(), ent.path(), make_groups)?;
                let range = key_range(subtree.as_slice()).unwrap_or_default();
                let size = range.len();

                let color = dir_color(ent.path());

                if size > 0 {
                    Ok(Some(Entry {
                        path: ent.path(),
                        size,
                        subtree,
                        color,
                        ..Default::default()
                    }))
                } else {
                    Ok(None)
                }
            } else if metadata.is_file() && metadata.len() > 0 {
                let color = file_color(ent.path());
                {
                    let mut state = state.lock().unwrap();
                    state.total += metadata.len() as usize;
                }

                Ok(Some(Entry {
                    path: ent.path(),
                    size: metadata.len() as usize,
                    color,
                    ..Default::default()
                }))
            } else {
                // TODO: deal with hard links possibly at different levels
                // Ignore symlinks
                Ok(None)
            }
        })
        .flatten()
        .filter_map_ok(identity)
        .collect::<Result<Vec<_>>>()?;

    entries.sort_by_key(|it| Reverse(it.size));

    if make_groups {
        entries = regroup(entries);
    }

    Ok(cumsum_size(entries))
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
                let color = v[0].color;
                let subtree = cumsum_size(v);

                Entry {
                    path: label.into(),
                    size,
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
    let root = args.path.canonicalize()?;
    let mut leaves: Vec<Entry> = Default::default();

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
        .build();

    for result in walker {
        // Each item yielded by the iterator is either a directory entry or an
        // error, so either print the path or the error.
        match result {
            Ok(ent) => {
                {
                    let mut state = state.lock().unwrap();
                    state.count += 1;
                    state.path = ent.path().into();
                }

                let metadata = ent.metadata()?;
                if metadata.is_file() && metadata.len() > 0 {
                    let mut state = state.lock().unwrap();
                    state.total += metadata.len() as usize;
                } else {
                    continue;
                }

                let color = file_color(ent.path());
                let entry = Entry {
                    path: ent.path().into(),
                    size: metadata.len() as usize,
                    color,
                    ..Default::default()
                };

                leaves.push(entry);
            }
            Err(err) => tracing::warn!("{}", err),
        }
    }

    Ok(make_forest(args, leaves))
}

// TODO: parallelize
fn make_forest(args: &Args, leaves: Vec<Entry>) -> Vec<(usize, Entry)> {
    let root = args.path.canonicalize().unwrap_or(args.path.clone());
    let (kidding, extensions) = rehash(args, leaves);

    let mut kidding = kidding;

    if args.xray {
        let mut entries = extensions
            .iter()
            .map(|ext| {
                let subtree = treeify(args, &mut kidding, &root, &Some(ext.clone()));
                let size = if !subtree.is_empty() {
                    subtree.iter().map(|(_, it)| it.size).sum()
                } else {
                    0
                };

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

fn leaves(entry: Entry) -> Vec<Entry> {
    if entry.subtree.is_empty() {
        return vec![entry];
    }

    deforest(entry.subtree)
}

fn deforest(forest: Forest) -> Vec<Entry> {
    forest
        .into_iter()
        .flat_map(|(_, it)| leaves(it))
        .collect_vec()
}

fn rehash(args: &Args, leaves: Vec<Entry>) -> (LineageMap, HashSet<OsString>) {
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
                it.size = subtree.iter().map(|(_, it)| it.size).sum();
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

    let target = args.path.clone();
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

    let mut app = App::new(args.clone(), &target, scanned);

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
        let area = frame.area();
        let buf = frame.buffer_mut();

        let lines = vec![
            format!("Scanning {}", state.path.display()),
            format!("Count: {}", state.count),
            format!("Total: {}", format_size(state.total, DECIMAL)),
        ]
        .into_iter()
        .map(Line::from)
        .collect_vec();
        let text = Text::from(lines);
        Paragraph::new(text).render(area, buf);
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
    mode_switch: bool,

    path: PathBuf,
    entries: TreeFocus,

    tree_items: Vec<TreeItem<'static, usize>>,
    selection: Vec<usize>,
}

#[derive(Default)]
pub struct AppState {
    title: Option<OsString>,
    skip_view: Vec<usize>,
    tree_state: TreeState<usize>,
    tag: Option<OsString>,

    click_pos: Option<Position>,
    click_area: Rect,
    click_addr: Vec<usize>,
}

impl App {
    pub fn new(args: Args, path: impl Into<PathBuf>, entries: Forest) -> Self {
        let tree_items = tree_items(entries.as_slice());
        let focus = TreeFocusBuilder {
            tree: entries.clone(),
            focus_builder: |_| None,
        }
        .build();

        Self {
            args,
            exit: false,
            mode_switch: false,
            path: path.into(),
            entries: focus,
            tree_items,
            selection: Default::default(),
        }
    }

    /// runs the application's main loop until the user quits
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let _ = terminal
            .backend_mut()
            .execute(crossterm::event::EnableMouseCapture);

        let mut state = AppState::default();

        while !self.exit {
            if self.mode_switch {
                self.mode_switch = false;

                let restore_view = self.view_entry(&state).path;
                let restore_path = self.entries.borrow_focus().map(|it| it.path.to_path_buf());
                let restore_tag = state.tag.clone();

                self.args.xray = !self.args.xray;
                let entries = std::mem::take(&mut self.entries);
                let forest = entries.into_heads().tree;
                let leaves = deforest(forest);
                let tree = make_forest(&self.args, leaves);

                self.entries = TreeFocusBuilder {
                    tree,
                    focus_builder: |_| None,
                }
                .build();

                self.tree_items = tree_items(self.entries.borrow_tree());

                self.selection = Default::default();
                state = AppState {
                    tag: restore_tag.clone(),
                    ..Default::default()
                };

                if let Some(addr) = tree_find_path(
                    self.entries.borrow_tree(),
                    &restore_view,
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

            if state.title.is_none() {
                let entry = self.view_entry(&state);
                if entry.tag.is_none() {
                    state.title = Some(entry.path.into_os_string());
                } else if let Some(tag) = &entry.tag {
                    state.title = Some(entry.path.join("**").with_extension(tag).into_os_string());
                }
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
                } else if entry.subtree.is_empty() {
                    state.tag = Some(entry.path.extension().unwrap_or_default().to_os_string());
                }
            }
        }

        Ok(())
    }

    fn draw(&self, frame: &mut Frame, state: &mut AppState) {
        let area = frame.area();
        state.click_area = area;
        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Max(50), Constraint::Fill(10)])
            .split(area);

        let sidebar = Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![
                Constraint::Fill(10),
                Constraint::Length(3),
                Constraint::Percentage(25),
            ])
            .split(layout[0]);

        let title = state.title.as_deref().unwrap_or_default().display();
        let widget = Tree::new(&self.tree_items)
            .expect("all item identifiers are unique")
            .block(
                Block::new()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .border_type(ratatui::widgets::BorderType::Double)
                    .padding(Padding::proportional(1))
                    .title(title.to_line().centered()),
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

        let text = vec![
            format!("{:?}", &state.skip_view),
            format!("{:?}", state.tree_state.selected()),
            format!("active tag: {:?}", &state.tag),
        ];
        let text = Text::from(text.into_iter().map(Line::from).collect_vec());
        frame.render_widget(Paragraph::new(text), sidebar[1]);

        if let Some(entry) = self.entries.borrow_focus() {
            let text = vec![
                format!("{}", entry.path.display()),
                format!("entry tag: {:?}", &entry.tag),
            ];
            let text = Text::from(text.into_iter().map(Line::from).collect_vec());

            frame.render_widget(
                Paragraph::new(text)
                    .block(Block::new().padding(Padding::proportional(1)))
                    .wrap(Wrap { trim: false }),
                sidebar[2],
            );
        }
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
            KeyCode::Char('x') => {
                self.mode_switch = true;
                true
            }
            KeyCode::Char('\n' | ' ') => state.tree_state.toggle_selected(),
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
        self.tree_items = tree_items(self.get_view(state));
        state.tree_state.select(self.selection.clone());
        for i in 0..self.selection.len() {
            state.tree_state.open(self.selection[..i].to_vec());
        }
        state.title = None;
    }

    fn exit(&mut self) -> bool {
        self.exit = true;
        false
    }

    fn view_entry(&self, state: &AppState) -> Entry {
        // A synthetic entry for the root
        let root = Entry {
            path: self.path.clone(),
            subtree: self.entries.borrow_tree().to_vec(),
            ..Default::default()
        };
        let mut cursor = &root;

        for id in state.skip_view.iter() {
            if let Ok(idx) = cursor.subtree.binary_search_by_key(id, |(id, _)| *id) {
                cursor = &cursor.subtree[idx].1;
            }
        }

        cursor.clone()
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
                let ids = subtree
                    .iter()
                    .map(|it| it.identifier())
                    .cloned()
                    .collect_vec();
                TreeItem::new(*k, text, subtree)
                    .context(format!("{ids:?}"))
                    .unwrap()
            }
        })
        .collect_vec()
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
