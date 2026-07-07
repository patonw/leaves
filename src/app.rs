use std::{
    ffi::OsStr,
    ops::{Deref, DerefMut},
};

use color_eyre::Result;
use crossterm::{
    ExecutableCommand as _,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, MouseEventKind},
};
use either::Either;
use humansize::{DECIMAL, format_size};
use itertools::Itertools as _;
use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style, Stylize as _},
    text::{Line, Text, ToSpan},
    widgets::{
        Block, BorderType, Borders, Padding, Paragraph, ScrollbarOrientation, StatefulWidget, Wrap,
    },
};
use thousands::Separable;
use tracing::{Level, instrument, span};
use tui_tree_widget::{Scrollbar, Tree, TreeItem};

use crate::core::{DbgEntry, DbgTrees, ENTRY_CHUNK_SIZE, Entry, EntryInfo, Forest, StackAddr};
use crate::explorer::build_nav_tree;
use crate::forest::{
    deforest, make_forest, merge_forests, par_forest, prune_entry, tree_find_path,
};
use crate::render::render_subtree;
use crate::scanfs::spawn_walker;
use crate::state::{
    AppAction, AppMode, AppState, TreeFocus, TreeFocusBuilder, get_selection, get_title,
};
use crate::util::ext_color;
use crate::{cli::Args, config::Config};

struct MouseyTerm<'a>(&'a mut DefaultTerminal);

impl<'a> MouseyTerm<'a> {
    pub fn new(term: &'a mut DefaultTerminal) -> Self {
        let _ = term
            .backend_mut()
            .execute(crossterm::event::EnableMouseCapture);
        Self(term)
    }
}

impl<'a> DerefMut for MouseyTerm<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

impl<'a> Deref for MouseyTerm<'a> {
    type Target = DefaultTerminal;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl<'a> Drop for MouseyTerm<'a> {
    fn drop(&mut self) {
        use crossterm::style::ResetColor;

        let _ = self
            .0
            .backend_mut()
            .execute(ResetColor)
            .unwrap()
            .execute(crossterm::event::DisableMouseCapture);
    }
}

pub struct App {
    config: Config,
    args: Args,
    exit: bool,

    entries: TreeFocus,
    reserve: Vec<Entry>,

    tree_items: Vec<TreeItem<'static, usize>>,
    selection: Vec<usize>,
}

impl App {
    #[instrument(level = "debug", skip_all)]
    pub fn new(config: Config, args: Args, entries: Forest) -> Self {
        let tree_items = build_nav_tree(entries.as_slice());
        let focus = span!(Level::DEBUG, "Wrapping initial tree focus").in_scope(|| {
            TreeFocusBuilder {
                tree: entries,
                focus_builder: |_| None,
            }
            .build()
        });

        Self {
            config,
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
        let mut terminal = MouseyTerm::new(terminal);

        let mode = if self.args.xray {
            AppMode::Xray
        } else {
            AppMode::Normal
        };

        let mut state = AppState::new(&self.args.path, mode);

        while !self.exit {
            if !matches!(state.action, AppAction::Noop) {
                self.handle_action(&mut terminal, &mut state)?;
            }

            if state.view_info.is_none() {
                span!(Level::DEBUG, "Refreshing view info").in_scope(|| {
                    let info = self.view_info(&state);

                    let title = get_title(&state, &info);
                    state.title = Some(title);
                    state.view_info = Some(info);
                });
            }

            if !state.click_addr.is_empty() {
                span!(Level::DEBUG, "Updating selection from area click").in_scope(|| {
                    let addr = std::mem::take(&mut state.click_addr);
                    let mut selection = state.skip_view.clone();
                    selection.extend_from_slice(&addr);
                    self.entries.select(&selection);

                    state.show_selection(&addr);

                    state.click_pos = None;
                });
            }

            self.selection.clear();
            self.selection
                .extend_from_slice(state.tree_state.selected());

            terminal.draw(|frame| self.draw(frame, &mut state))?;
            while !self.handle_events(&mut state)? {}

            self.entries.select(&state.qual_select());

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

        if self.args.xray {
            window = window.border_type(BorderType::HeavyDoubleDashed);

            if !state.skip_view.is_empty()
                && let Some(tag) = &state.tag
            {
                let style = Style::from(ext_color(tag));
                window = window.border_style(style);
            }
        }

        let mut status_line = match state.mode {
            AppMode::Normal => Line::from(vec![
                " Mode: normal ".into(),
                " | Keys: ".bold(),
                " x".blue().bold(),
                "-ray ".into(),
            ]),

            AppMode::Xray => Line::from(vec![
                " Mode: x-ray ".into(),
                " | Keys: ".bold(),
                " (".into(),
                "x".blue().bold(),
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

        let diag_text = if state.diagnostic {
            let lines = vec![
                "--- Diagnostics ---".into(),
                format!("VW {:?}", &state.skip_view),
                format!("SL {:?}", state.tree_state.selected()),
                format!("TG {:?}", &state.tag),
            ];

            let lines = lines.into_iter().map(Line::from).collect_vec();
            Some(Text::from(lines))
        } else {
            None
        };

        let path = self
            .entries
            .borrow_focus()
            .as_ref()
            .map(|entry| entry.path.clone());

        let path_text = path
            .as_ref()
            .map(|it| Text::from(format!("{}", it.display())));

        #[cfg(feature = "clipboard")]
        if let Some(path) = path
            && let Some(click) = state.click_pos
            && sidebar[1].contains(click)
            && let Some(clipboard) = &mut state.clipboard
        {
            let text = path.display().to_string();
            tracing::info!(text, "Attempting to copy path to clipboard");

            let result = clipboard.set_text(text);

            tracing::debug!(?result, "Clipboard result");
        }

        let mut info_lines = vec![];
        if let Some(entry) = self.entries.borrow_focus() {
            info_lines.extend_from_slice(&[format!(
                "tag: {}",
                entry
                    .tag
                    .as_deref()
                    .or_else(|| entry.path.extension())
                    .unwrap_or(OsStr::new("(none)"))
                    .display()
            )]);
            if state.diagnostic {
                info_lines.push(format!("leaves: {}", &entry.leaves.separate_with_commas()));
            }

            if entry.nfiles > 1 {
                // is dir
                info_lines.push(format!("files: {}", &entry.nfiles.separate_with_commas()));
            } else if entry.subtree.is_empty() {
                // is file
                info_lines.push(format!("bytes: {}", &entry.size.separate_with_commas()));
            }
        }

        let info_text = Text::from(info_lines.into_iter().map(Line::from).collect_vec());

        let info_box = Block::new().padding(1.into());

        let mut inspector = vec![];

        if let Some(text) = &diag_text {
            inspector.push(Constraint::Length(text.height() as u16 + 1));
        }
        if let Some(text) = &path_text {
            let height = text.width().div_ceil(sidebar[1].width as usize) as u16;
            inspector.push(Constraint::Length(height + 1));
        }

        inspector.push(Constraint::Min(info_text.height() as u16));

        let inspector = Layout::default()
            .direction(Direction::Vertical)
            .flex(ratatui::layout::Flex::Start)
            .constraints(inspector)
            .split(info_box.inner(sidebar[1]));

        let widgets = [diag_text, path_text, Some(info_text)]
            .into_iter()
            .flatten();

        for (area, text) in inspector.iter().zip(widgets) {
            frame.render_widget(
                Paragraph::new(text)
                    // .block(Block::new().padding(Padding::proportional(1)))
                    .wrap(Wrap { trim: false }),
                *area,
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
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press => false,
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => state.tree_state.scroll_down(1),
                MouseEventKind::ScrollUp => state.tree_state.scroll_up(1),
                MouseEventKind::Down(_button) => {
                    let position = Position::new(mouse.column, mouse.row);
                    state.tree_state.click_at(position);

                    // This requires two cycles so we take advantage of the mouse up event
                    state.click_pos = Some(position);
                    state.click_addr.clear();
                    true
                }
                MouseEventKind::Up(_button) => true,
                _ => false,
            },
            Event::Resize(_, _) => true,
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
            KeyCode::Char(' ') => state.tree_state.toggle_selected(),
            KeyCode::Char('<') => {
                self.selection.clear();
                state.tree_state.close_all()
            }
            KeyCode::Left => {
                let dirty = state.tree_state.key_left();

                tracing::debug!(sel=?state.tree_state.selected(), qual=?state.qual_select(), "Key left");
                dirty
            }
            KeyCode::Right => state.tree_state.key_right(),
            KeyCode::Down => state.tree_state.key_down(),
            KeyCode::Up => state.tree_state.key_up(),
            KeyCode::Esc | KeyCode::Char('`') => state.tree_state.select(Vec::new()),
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
            KeyCode::Backspace | KeyCode::Char('\\') => {
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

        if state
            .view_info
            .as_ref()
            .map(|it| it.leaves > ENTRY_CHUNK_SIZE * 8)
            .unwrap_or(true)
        {
            // This takes a long time on large trees so do it the background
            let old_nav = std::mem::take(&mut self.tree_items);

            std::thread::spawn(move || {
                span!(Level::DEBUG, "Dropping old navigator tree").in_scope(|| drop(old_nav))
            });
        }

        // Careful here. Plain assignment generates a large chunk of overhead (drop/dealloc?).
        span!(Level::DEBUG, "Synchronizing view", addr=?state.skip_view).in_scope(|| {
            // self.tree_items = build_nav_tree(self.get_view(state));
            self.tree_items.clear();
            self.tree_items.extend(build_nav_tree(self.get_view(state)));
        });

        tracing::debug!(items = self.tree_items.len(), "Rebuilt navigator tree");

        state.show_selection(&self.selection);

        state.view_info = None;
        state.title = None;
    }

    fn exit(&mut self) -> bool {
        self.exit = true;
        true
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

    #[instrument(skip_all, fields(action = ?state.action))]
    fn handle_action(
        &mut self,
        terminal: &mut DefaultTerminal,
        state: &mut AppState,
    ) -> Result<()> {
        match std::mem::take(&mut state.action) {
            AppAction::Noop => {}
            AppAction::SwitchMode(mode) => {
                let restore_info = self.view_info(state);
                let init_root = self.args.path.canonicalize()?;

                let restore_view = if state.skip_view.is_empty() && init_root == state.root {
                    None
                } else {
                    Some(restore_info.path.as_path())
                };
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
                            + forest.iter().map(|(_, it)| it.leaves).sum::<usize>();
                        let items = std::mem::take(&mut self.reserve);
                        let items = items.into_iter().chain(deforest(forest));
                        (Either::Left(items), Some(count))
                    }
                    AppMode::Xray => {
                        self.args.xray = true;

                        let count = state.view_info.as_ref().map(|it| it.leaves);
                        let pruned = match prune_entry(&mut forest, state.skip_view.as_slice()) {
                            Either::Left(entry) => entry.subtree,
                            Either::Right(forest) => forest,
                        };
                        self.reserve = deforest(forest).collect_vec();
                        state.root = restore_view.unwrap_or(&self.args.path).to_path_buf();

                        (Either::Right(deforest(pruned)), count)
                    }
                };

                tracing::info!(
                    ?restore_view,
                    ?restore_path,
                    ?restore_tag,
                    "Switching to mode {mode:?} with estimated {count:?} items"
                );

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

                // Actually quite fast up to this point with a wide forest.

                self.entries = TreeFocusBuilder {
                    tree,
                    focus_builder: |_| None,
                }
                .build();

                self.selection = Default::default();
                state.tag = restore_tag.clone();
                state.title = None;
                state.mode = mode;
                state.click_pos = Default::default();
                state.click_addr = Default::default();
                state.tree_state = Default::default();
                state.skip_view = Default::default();

                span!(Level::DEBUG, "Restoring view.").in_scope(|| {
                    if let Some(path) = restore_view
                        && let Some(addr) = tree_find_path(
                            self.entries.borrow_tree(),
                            path,
                            restore_tag.as_deref(),
                            &StackAddr::root(),
                        )
                    {
                        state.skip_view = addr;
                    }
                });

                span!(Level::DEBUG, "Restoring selection.").in_scope(|| {
                    if let Some(path) = restore_path
                        && let Some(addr) = tree_find_path(
                            self.entries.borrow_tree(),
                            &path,
                            restore_tag.as_deref(),
                            &StackAddr::root(),
                        )
                    {
                        state.click_addr =
                            addr.into_iter().skip(state.skip_view.len()).collect_vec();
                    }
                });

                self.sync_view(state);

                tracing::debug!("Done with switch.");
            }
            AppAction::Deflate => {
                let mut selection = state.qual_select();
                let Some(focus) = self.entries.borrow_focus() else {
                    return Ok(());
                };

                let focus = if focus.subtree.is_empty()
                    && selection.pop().is_some()
                    && let Some(entry) = get_selection(&selection, self.entries.borrow_tree())
                {
                    let rel_sel = selection
                        .iter()
                        .skip(state.skip_view.len())
                        .cloned()
                        .collect_vec();
                    state.tree_state.select(rel_sel);
                    entry
                } else {
                    *focus
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
                let pruned = match prune_entry(&mut forest, &selection) {
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

                self.entries = TreeFocusBuilder {
                    tree,
                    focus_builder: |tree| get_selection(&selection, tree),
                }
                .build();

                self.tree_items = build_nav_tree(self.get_view(state));
                state.view_info = None;
            }
            AppAction::Expand => {
                let dirty = if !state.qual_select().is_empty() {
                    self.expand_selected(state)?
                } else {
                    self.refresh_root(state)?
                };

                if dirty {
                    self.tree_items = build_nav_tree(self.get_view(state));
                    self.entries.select(&state.qual_select());
                    state.view_info = None;
                }
            }
        }

        Ok(())
    }

    fn expand_selected(&mut self, state: &mut AppState) -> Result<bool> {
        let mut selection = state.qual_select();
        let Some(focus) = self.entries.borrow_focus() else {
            return Ok(false);
        };

        let focus = if focus.subtree.is_empty()
            && selection.pop().is_some()
            && let Some(entry) = get_selection(&selection, self.entries.borrow_tree())
        {
            let rel_sel = selection
                .iter()
                .skip(state.skip_view.len())
                .cloned()
                .collect_vec();
            state.tree_state.select(rel_sel);
            entry
        } else {
            *focus
        };

        tracing::debug!("Expanding node {:?}", DbgEntry(focus));
        if focus.subtree.is_empty() || focus.is_group {
            return Ok(false);
        }

        let target = Some(focus.path.to_path_buf());
        let Some(target) = target else {
            return Ok(false);
        };
        let tag = focus.tag.clone().or_else(|| {
            if !focus.subtree.is_empty() {
                None
            } else {
                focus.path.extension().map(|s| s.to_os_string())
            }
        });

        let init_root = self.args.path.canonicalize()?;
        let rel_target = target.strip_prefix(&state.root)?;
        let depth = rel_target.components().count();
        tracing::debug!("Rescanning {target:?}. Root {init_root:?}. tag {tag:?}. depth {depth}");
        let args = self.args.with_depth(depth + self.args.max_depth);
        let entries = std::mem::take(&mut self.entries);
        let mut forest = entries.into_heads().tree;
        let pruned = prune_entry(&mut forest, &selection);
        match pruned {
            Either::Left(it) => tracing::debug!("Pruned entry {:?}", DbgEntry(&it)),
            Either::Right(subtree) => tracing::debug!("Pruned {} subtrees", subtree.len()),
        }
        let rx = spawn_walker(&args, Default::default(), &target)?;
        let leaves = rx.into_iter().filter(|it| {
            it.path.starts_with(target.as_path())
                && (tag.is_none()
                    || it.path.extension().unwrap_or_default() == tag.as_ref().unwrap())
        });
        let tree = make_forest(&args, &state.root, leaves);
        tracing::debug!("Expanded subtree {:?}", DbgTrees(&tree));
        let tree = merge_forests(forest, tree);
        self.entries = TreeFocusBuilder {
            tree,
            focus_builder: |tree| get_selection(&selection, tree),
        }
        .build();
        Ok(true)
    }

    fn refresh_root(&mut self, state: &mut AppState) -> Result<bool> {
        let init_root = self.args.path.canonicalize()?;
        let state_root = &state.root;
        let nfiles = state.view_info.as_ref().map(|info| info.nfiles);
        tracing::info!(?init_root, ?state_root, ?nfiles, "Refreshing root forest");

        // Safety for the foot-gun. If you want to rescan from the top, quit and restart.
        if init_root == state.root {
            return Ok(false);
        }

        // Unnecessary, since pruning is based on target depth
        // let depth = state.root.strip_prefix(scan_root)?.components().count();
        // let args = self.args.with_depth(depth + self.args.max_depth);
        // tracing::debug!(depth, "Expansion depth");

        // Don't need to prune since we're replacing the entire tree
        let rx = spawn_walker(&self.args, Default::default(), &state.root)?;

        let tree = par_forest(&self.args, &state.root, rx, nfiles);

        self.entries = TreeFocusBuilder {
            tree,
            focus_builder: |_| None,
        }
        .build();

        tracing::debug!("Done refreshing root");

        Ok(true)
    }
}

impl StatefulWidget for &App {
    type State = AppState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let tree = self.get_view(state);

        render_subtree(
            &self.config,
            state,
            &StackAddr::root(),
            area,
            buf,
            tree,
            &self.selection,
        );
    }
}
