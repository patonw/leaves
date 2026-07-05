use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Debug;
use std::path::PathBuf;

use itertools::Itertools as _;
use ratatui::style::Color;

pub const ENTRY_CHUNK_SIZE: usize = 5000;

pub type Forest = Vec<(usize, Entry)>;
pub type TreeSlice<'a> = &'a [(usize, Entry)];
pub type LineageMap = HashMap<(PathBuf, Option<OsString>), HashMap<PathBuf, Entry>>;

#[derive(Debug, Clone)]
pub enum MaybePair<T>
where
    T: std::fmt::Debug + Clone,
{
    One(T),
    Two(T, T),
}

#[derive(Clone, Default, Debug)]
pub struct StackAddr<'a>(pub Option<(usize, &'a StackAddr<'a>)>);

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

#[derive(Default, Clone, Debug, Hash, Eq, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub tag: Option<OsString>,
    pub size: usize,
    pub nfiles: usize,
    pub leaves: usize,
    pub subtree: Forest,
    pub color: Color,
    pub is_group: bool,
}

// TODO: consolidation/composition
#[derive(Default, Clone, Debug, Hash, Eq, PartialEq)]
pub struct EntryInfo {
    pub path: PathBuf,
    pub tag: Option<OsString>,
    pub size: usize,
    pub nfiles: usize,
    pub leaves: usize,
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

pub struct DbgEntry<'a>(pub &'a Entry);

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
pub struct DbgTrees<'a>(pub TreeSlice<'a>);

impl<'a> Debug for DbgTrees<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tmp = self.0.iter().map(|(i, v)| (*i, DbgEntry(v))).collect_vec();
        Debug::fmt(&tmp, f)
    }
}

// Lazy debugging wrapper for forests to avoid allocs if not logging.
pub struct CountedForest<'a>(pub TreeSlice<'a>);

impl<'a> Debug for CountedForest<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n_trees = self.0.len();
        let size: usize = self.0.iter().map(|(_, it)| it.size).sum();
        let n_leaves: usize = self.0.iter().map(|(_, it)| it.leaves).sum();
        let n_files: usize = self.0.iter().map(|(_, it)| it.nfiles).sum();

        f.debug_struct("Forest")
            .field("trees", &n_trees)
            .field("size", &size)
            .field("n_leaves", &n_leaves)
            .field("n_files", &n_files)
            .finish()
    }
}

/// Sorts largest entries first. Ties broken by lexical order.
pub fn sort_largest(mut entries: Vec<Entry>) -> Vec<Entry> {
    entries.sort_unstable_by(|a, b| b.size.cmp(&a.size).then(a.path.cmp(&b.path)));
    entries
}

/// Transform list of entries, tagging each with cumulative size of preceding siblings.
pub fn cumsum_size(entries: Vec<Entry>) -> Vec<(usize, Entry)> {
    entries
        .into_iter()
        .scan(0, |acc, it| {
            let start = *acc;
            *acc += it.size;
            Some((start, it))
        })
        .collect()
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
