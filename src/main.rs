use std::cmp::Reverse;
use std::collections::HashMap;
use std::convert::identity;
use std::ffi::OsString;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::ops::Range;
use std::path::{Path, PathBuf};

use color_eyre::Result;
use crossterm::ExecutableCommand as _;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, MouseEventKind};
use eyre::Context as _;
use humansize::{DECIMAL, format_size};
use itertools::Itertools as _;
use ratatui::layout::{Constraint, Direction, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::ToLine as _;
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
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Group files in a directory by extension
    #[arg(short, long)]
    group: bool,
}

type DirTree = Vec<(usize, Entry)>;
type TreeSlice<'a> = &'a [(usize, Entry)];

#[derive(Default, Clone, Debug)]
pub struct Entry {
    path: PathBuf,
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
            subtree: Default::default(),
            color: Color::Reset,
            is_group: false,
        }
    }
}

fn scan_fs<P: AsRef<Path>>(path: P, make_groups: bool) -> Result<DirTree> {
    let mut entries = std::fs::read_dir(path)?
        .map_ok(|ent| -> Result<_> {
            let metadata = ent.metadata()?;
            if metadata.is_dir() {
                let subtree = scan_fs(ent.path(), make_groups)?;
                let range = key_range(subtree.as_slice()).unwrap_or_default();
                let size = range.len();

                let mut h = DefaultHasher::default();
                format!("{}", ent.path().file_name().unwrap_or_default().display()).hash(&mut h);
                let id = h.finish();

                // let color = colorous::TABLEAU10[id as usize % 10];
                let color = colorous::VIRIDIS.eval_rational(id as usize, u64::MAX as usize);

                if size > 0 {
                    Ok(Some(Entry {
                        path: ent.path(),
                        size,
                        subtree,
                        color: Color::Rgb(color.r, color.g, color.b),
                        is_group: false,
                    }))
                } else {
                    Ok(None)
                }
            } else if metadata.is_file() && metadata.len() > 0 {
                let color = if let Some(ext) = ent.path().extension() {
                    let mut h = DefaultHasher::default();
                    format!("{}", ext.display()).hash(&mut h);
                    let id = h.finish();

                    // let color = colorous::TABLEAU10[id as usize % 10];
                    let color =
                        colorous::YELLOW_ORANGE_BROWN.eval_rational(id as usize, u64::MAX as usize);
                    Color::Rgb(color.r, color.g, color.b)
                } else {
                    Color::Reset
                };

                Ok(Some(Entry {
                    path: ent.path(),
                    size: metadata.len() as usize,
                    subtree: Default::default(),
                    color,
                    is_group: false,
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

    Ok(entries
        .into_iter()
        .scan(0, |acc, it| {
            let start = *acc;
            *acc += it.size;
            Some((start, it))
        })
        .collect())
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
                let subtree = v
                    .into_iter()
                    .scan(0, |acc, it| {
                        let start = *acc;
                        *acc += it.size;
                        Some((start, it))
                    })
                    .collect();

                Entry {
                    path: label.into(),
                    size,
                    subtree,
                    color,
                    is_group: true,
                }
            }
        })
        .collect();

    entries.sort_by_key(|it| Reverse(it.size));
    entries
}

#[instrument]
fn main() -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    color_eyre::install()?;

    let args = Args::parse();

    let target = args.path;
    let scanned = scan_fs(&target, args.group)?;

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

pub struct App {
    exit: bool,
    path: PathBuf,
    entries: TreeFocus,

    state: TreeState<usize>,
    tree_items: Vec<TreeItem<'static, usize>>,
    selection: Vec<usize>,
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
            entries: focus,
            tree_items,
            exit: false,
            state: Default::default(),
            selection: Default::default(),
        }
    }

    /// runs the application's main loop until the user quits
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let _ = terminal
            .backend_mut()
            .execute(crossterm::event::EnableMouseCapture);

        while !self.exit {
            terminal.draw(|frame| self.draw(frame))?;
            if self.handle_events()? {
                self.entries.select(self.state.selected());
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
            .constraints(vec![Constraint::Fill(10), Constraint::Percentage(25)])
            .split(layout[0]);

        self.selection.clear();
        self.selection.extend_from_slice(self.state.selected());

        let title = self.path.display();
        let widget = Tree::new(&self.tree_items)
            .expect("all item identifiers are unique")
            .block(
                Block::new()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .border_type(ratatui::widgets::BorderType::Double)
                    .padding(Padding::proportional(1))
                    .title(title.to_line().centered())
                    .title_bottom(format!("{:?}", self.state.selected())),
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
        if let Some(entry) = self.entries.borrow_focus() {
            frame.render_widget(
                Paragraph::new(format!("{}", entry.path.display()))
                    .block(Block::new().padding(Padding::proportional(1)))
                    .wrap(Wrap { trim: false }),
                sidebar[1],
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
        let app = self;
        match key.code {
            KeyCode::Char('q') => app.exit(),
            KeyCode::Char('\n' | ' ') => app.state.toggle_selected(),
            KeyCode::Left => app.state.key_left(),
            KeyCode::Right => app.state.key_right(),
            KeyCode::Down => app.state.key_down(),
            KeyCode::Up => app.state.key_up(),
            KeyCode::Esc => app.state.select(Vec::new()),
            KeyCode::Home => app.state.select_first(),
            KeyCode::End => app.state.select_last(),
            KeyCode::PageDown => app.state.scroll_down(3),
            KeyCode::PageUp => app.state.scroll_up(3),
            _ => false,
        }
    }

    fn exit(&mut self) -> bool {
        self.exit = true;
        false
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
        render_subtree(area, buf, self.entries.borrow_tree(), &self.selection);
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
