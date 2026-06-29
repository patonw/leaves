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
use ratatui::widgets::{Borders, Fill, Padding, Paragraph, ScrollbarOrientation, Wrap};
use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Widget},
};

use tracing::instrument;
use tui_tree_widget::{Scrollbar, Tree, TreeItem, TreeState};

use clap::Parser;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
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

type DirTree = Vec<(usize, Entry)>;
type TreeSlice<'a> = &'a [(usize, Entry)];

#[derive(Default, Clone, Debug, Hash, Eq, PartialEq)]
pub struct Entry {
    path: PathBuf,
    tag: OsString,
    size: usize,
    subtree: DirTree,
    color: Color,
    is_group: bool,
}

#[ouroboros::self_referencing]
pub struct TreeFocus {
    tree: DirTree,

    #[borrows(tree)]
    #[covariant]
    focus: Option<&'this Entry>,
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
) -> Result<DirTree> {
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

fn walk_fs(args: &Args, state: Arc<Mutex<ScanState>>) -> Result<DirTree> {
    let root = args.path.canonicalize()?;
    let mut kidding: HashMap<(PathBuf, OsString), HashSet<Entry>> = Default::default();
    let mut extensions: HashSet<OsString> = Default::default();

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

                let ext = if args.xray {
                    ent.path().extension().unwrap_or_default()
                } else {
                    Default::default()
                }
                .to_os_string();

                extensions.insert(ext.clone());

                let color = file_color(ent.path());
                let mut cursor = Entry {
                    path: ent.path().into(),
                    size: metadata.len() as usize,
                    color,
                    tag: ext.clone(),
                    ..Default::default()
                };

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
            Err(err) => tracing::warn!("{}", err),
        }
    }

    if args.xray {
        let mut entries = extensions
            .iter()
            .map(|ext| {
                let subtree = treeify(args, &mut kidding, (&root, ext));
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
        Ok(cumsum_size(entries))
    } else {
        Ok(treeify(args, &mut kidding, (&root, Default::default())))
    }
}

fn treeify(
    args: &Args,
    kidding: &mut HashMap<(PathBuf, OsString), HashSet<Entry>>,
    path: (&Path, &OsStr),
) -> DirTree {
    let key = (path.0.to_path_buf(), path.1.to_os_string());
    let Some(entries) = kidding.remove(&key) else {
        return Default::default();
    };

    let mut entries = entries
        .into_iter()
        .map(|mut it| {
            let subtree = treeify(args, kidding, (&it.path, path.1));
            if !subtree.is_empty() {
                it.size = subtree.iter().map(|(_, it)| it.size).sum();
                it.subtree = subtree;
            }
            // else {
            //     it.path = it.path.join(format!("**.{}", path.1.display()));
            // }
            it
        })
        .collect_vec();

    entries.sort_by_key(|it| Reverse(it.size));

    if args.group {
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
    if args.xray {
        args.group = false;
    }

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

    ratatui::run(|terminal| App::new(&target, scanned).run(terminal))
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
    exit: bool,
    path: PathBuf,
    title: Option<OsString>,
    entries: TreeFocus,

    state: TreeState<usize>,
    tree_items: Vec<TreeItem<'static, usize>>,
    selection: Vec<usize>,
    skip_view: Vec<usize>,
}

impl App {
    pub fn new(path: impl Into<PathBuf>, entries: DirTree) -> Self {
        let tree_items = tree_items(entries.as_slice());
        let focus = TreeFocusBuilder {
            tree: entries.clone(),
            focus_builder: |_| None,
        }
        .build();
        Self {
            path: path.into(),
            title: None,
            entries: focus,
            tree_items,
            exit: false,
            state: Default::default(),
            selection: Default::default(),
            skip_view: Default::default(),
        }
    }

    /// runs the application's main loop until the user quits
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let _ = terminal
            .backend_mut()
            .execute(crossterm::event::EnableMouseCapture);

        while !self.exit {
            if self.title.is_none() {
                let entry = self.view_entry();
                if entry.tag.is_empty() {
                    self.title = Some(entry.path.into_os_string());
                } else {
                    self.title = Some(
                        entry
                            .path
                            .join("**")
                            .with_extension(&entry.tag)
                            .into_os_string(),
                    );
                }
            }

            terminal.draw(|frame| self.draw(frame))?;
            if self.handle_events()? {
                let mut selection = self.skip_view.clone();
                selection.extend(self.state.selected());

                self.entries.select(&selection);
            }
        }

        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Max(50), Constraint::Fill(10)])
            .split(area);

        let sidebar = Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![
                Constraint::Fill(10),
                Constraint::Length(2),
                Constraint::Percentage(25),
            ])
            .split(layout[0]);

        self.selection.clear();
        self.selection.extend_from_slice(self.state.selected());

        let title = self.title.as_deref().unwrap_or_default().display();
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

        frame.render_stateful_widget(widget, sidebar[0], &mut self.state);

        let text = vec![
            format!("{:?}", self.state.selected()),
            format!("{:?}", &self.skip_view),
        ];
        let text = Text::from(text.into_iter().map(Line::from).collect_vec());
        frame.render_widget(Paragraph::new(text), sidebar[1]);

        if let Some(entry) = self.entries.borrow_focus() {
            frame.render_widget(
                Paragraph::new(format!("{}", entry.path.display()))
                    .block(Block::new().padding(Padding::proportional(1)))
                    .wrap(Wrap { trim: false }),
                sidebar[2],
            );
        }
        frame.render_widget(self, layout[1]);
    }

    fn handle_events(&mut self) -> Result<bool> {
        let dirty = match event::read()? {
            // it's important to check that the event is a key press event as
            // crossterm also emits key release and repeat events on Windows.
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                self.handle_key_event(key_event)
            }
            Event::Mouse(mouse) => {
                let app = self;
                match mouse.kind {
                    MouseEventKind::ScrollDown => app.state.scroll_down(1),
                    MouseEventKind::ScrollUp => app.state.scroll_up(1),
                    MouseEventKind::Down(_button) => {
                        app.state.click_at(Position::new(mouse.column, mouse.row))
                    }
                    _ => false,
                }
            }
            _ => false,
        };

        Ok(dirty)
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => self.exit(),
            KeyCode::Char('\n' | ' ') => self.state.toggle_selected(),
            KeyCode::Left => self.state.key_left(),
            KeyCode::Right => self.state.key_right(),
            KeyCode::Down => self.state.key_down(),
            KeyCode::Up => self.state.key_up(),
            KeyCode::Esc => self.state.select(Vec::new()),
            KeyCode::Home => self.state.select_first(),
            KeyCode::End => self.state.select_last(),
            KeyCode::PageDown => self.state.scroll_down(3),
            KeyCode::PageUp => self.state.scroll_up(3),
            KeyCode::Enter => {
                self.skip_view.extend_from_slice(self.state.selected());
                self.selection.clear();
                self.tree_items = tree_items(self.get_view());
                self.state.select(self.selection.clone());
                for i in 0..self.selection.len() {
                    self.state.open(self.selection[..i].to_vec());
                }
                self.title = None;
                true
            }
            KeyCode::Backspace => {
                if let Some(id) = self.skip_view.pop() {
                    self.selection.insert(0, id);
                }
                self.tree_items = tree_items(self.get_view());
                self.state.select(self.selection.clone());
                for i in 0..self.selection.len() {
                    self.state.open(self.selection[..i].to_vec());
                }
                self.title = None;
                true
            }
            _ => false,
        }
    }

    fn exit(&mut self) -> bool {
        self.exit = true;
        false
    }

    fn view_entry(&self) -> Entry {
        // A synthetic entry for the root
        let root = Entry {
            path: self.path.clone(),
            subtree: self.entries.borrow_tree().to_vec(),
            ..Default::default()
        };
        let mut cursor = &root;

        for id in self.skip_view.iter() {
            if let Ok(idx) = cursor.subtree.binary_search_by_key(id, |(id, _)| *id) {
                cursor = &cursor.subtree[idx].1;
            }
        }

        cursor.clone()
    }

    fn get_view(&self) -> &[(usize, Entry)] {
        self.skip_view
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
impl Widget for &mut App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let tree = self.get_view();

        render_subtree(area, buf, tree, &self.selection);
    }
}

fn render_subtree(area: Rect, buf: &mut Buffer, tree: TreeSlice, selection: &[usize]) {
    if tree.is_empty() {
        return;
    }

    // TODO: render with this if partition results in a small block
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

        return;
    }

    if tree.len() == 1 {
        let (key, entry) = &tree[0];

        render_entry(area, buf, *key, entry, selection);

        return;
    }

    match partition(tree) {
        MaybePair::One(entries) => {
            render_subtree(area, buf, entries, selection);
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

            render_subtree(layout[0], buf, left, selection);
            render_subtree(layout[1], buf, right, selection);
        }
    }
}

fn render_entry(area: Rect, buf: &mut Buffer, key: usize, entry: &Entry, selection: &[usize]) {
    let Entry {
        path,
        size,
        subtree,
        is_group,
        ..
    } = entry;

    let title = path.file_name().unwrap_or_default();
    let display = title.display();

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
        render_subtree(inner, buf, subtree, selection);
    }
}
