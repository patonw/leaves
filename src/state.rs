use std::fmt::Debug;
use std::path::PathBuf;
use std::{ffi::OsString, path::Path};

use humansize::{DECIMAL, format_size};
use ratatui::layout::{Position, Rect};

use thousands::Separable;
use tui_tree_widget::TreeState;

use crate::core::{Entry, EntryInfo, Forest};

#[derive(Clone, Copy, Debug, Default)]
pub enum AppMode {
    #[default]
    Normal,
    Xray,
}

#[derive(Clone, Debug, Default)]
pub enum AppAction {
    #[default]
    Noop,
    SwitchMode(AppMode),
    Deflate,
    Expand,
}

#[derive(Default)]
pub struct AppState {
    #[cfg(feature = "clipboard")]
    pub clipboard: Option<arboard::Clipboard>,

    pub root: PathBuf,
    pub mode: AppMode,
    pub action: AppAction,
    pub diagnostic: bool,

    pub view_info: Option<EntryInfo>,
    pub title: Option<OsString>,
    pub skip_view: Vec<usize>,
    pub tree_state: TreeState<usize>,
    pub tag: Option<OsString>,

    pub click_pos: Option<Position>,
    pub click_area: Rect,
    pub click_addr: Vec<usize>,
}

impl AppState {
    pub fn new(path: impl AsRef<Path>, mode: AppMode) -> Self {
        Self {
            root: path.as_ref().to_path_buf(),
            mode,
            #[cfg(feature = "clipboard")]
            clipboard: arboard::Clipboard::new().ok(),
            ..Default::default()
        }
    }

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

#[ouroboros::self_referencing(pub_extras)]
pub struct TreeFocus {
    pub tree: Forest,

    #[borrows(tree)]
    #[covariant]
    pub focus: Option<&'this Entry>,
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

pub fn get_title(state: &AppState, info: &EntryInfo) -> OsString {
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

pub fn get_selection<'a>(
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
