use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::ops::Range;
use std::path::Path;

use either::Either;
use itertools::Itertools as _;
use tracing::{Level, span};

use crate::cli::Args;
use crate::colors::ColorScheme;
use crate::core::{
    CountedForest, DbgEntry, ENTRY_CHUNK_SIZE, Entry, Forest, LineageMap, MaybePair, StackAddr,
    TreeSlice, cumsum_size, sort_largest,
};

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

pub fn deforest(forest: Forest) -> LeafIterator {
    into_leaves(forest.into_iter().map(|(_, it)| it))
}

pub fn partition(whole: TreeSlice) -> MaybePair<TreeSlice> {
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

pub fn key_range(whole: TreeSlice) -> Option<Range<usize>> {
    if whole.is_empty() {
        return None;
    }

    let (start, _) = whole[0];

    let end = whole.last().unwrap();
    let end = end.0 + end.1.size;

    Some((start)..end)
}

// Items are sorted by size rather than path so we need to do a linear scan.
// This can be rather slow with wide trees, especially in X-ray mode with
// a large number of extensions and top-level directories.
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

pub fn prune_entry(
    forest: &mut Vec<(usize, Entry)>,
    view_addr: &[usize],
) -> Either<Entry, Vec<(usize, Entry)>> {
    // Traverse twice instead of using a parent var to appease borrow checker
    // Could also use an parent + Some(child_idx) to represent cursor, but that just makes
    // the single traversal more complicated for little practical gain.
    tracing::debug!(?view_addr, "Pruning entry at address");

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

    tracing::debug!(?addr, "Resolved view to indices");

    // Second traversal to the parent Vec, then splice out the entry
    // to ensure we don't leave dangling empty directories that will
    // be confused as empty leaves.
    if let Some(last_idx) = addr.pop() {
        let mut cursor = &mut *forest;
        for idx in &addr {
            cursor = &mut cursor[*idx].1.subtree;
        }

        let (_, entry) = cursor.remove(last_idx);
        tracing::debug!(entry=?DbgEntry(&entry), "Extracted entry from tree");

        // And now a third pass to fix all the counters for surgical edits
        let mut cursor = &mut *forest;
        for idx in &addr {
            let container = &mut cursor[*idx].1;
            container.nfiles -= entry.nfiles;
            container.leaves -= entry.leaves;
            container.size -= entry.size;

            cursor = &mut cursor[*idx].1.subtree;
        }

        tracing::debug!("Fixed ancestor counts");

        Either::Left(entry)
    } else {
        Either::Right(std::mem::take(forest))
    }
}

pub fn treeify(
    _args: &Args,
    kidding: &mut LineageMap,
    path: &Path,
    ext: &Option<OsString>,
) -> Forest {
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

/// Combine subtrees of two entries with the same path & tag
pub fn merge_entries(mut left: Entry, right: Entry) -> Entry {
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

pub fn merge_forests(left: Forest, right: Forest) -> Vec<(usize, Entry)> {
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

// Move entries instead of slicing to reduce allocations. Still need to allocate for interior
// nodes, but should be an order of magnitude less.
pub fn make_forest(
    colors: &ColorScheme,
    args: &Args,
    root: impl AsRef<Path>,
    leaves: impl IntoIterator<Item = Entry>,
) -> Vec<(usize, Entry)> {
    let root = root.as_ref().canonicalize().unwrap_or(args.path.clone());

    let (kidding, extensions) = rehash(colors, args, &root, leaves);

    let mut kidding = kidding;

    if args.xray {
        tracing::debug!(
            "making x-ray forest with {} extensions ({:?}...) from {} nodes",
            extensions.len(),
            extensions.iter().take(10).collect_vec(),
            kidding.len()
        );

        let entries = extensions
            .iter()
            .map(|ext| {
                let subtree = treeify(args, &mut kidding, &root, &Some(ext.clone()));
                let size = subtree.iter().map(|(_, it)| it.size).sum();
                let nfiles = subtree.iter().map(|(_, it)| it.nfiles).sum();
                let leaves = subtree.iter().map(|(_, it)| it.leaves).sum();

                tracing::trace!(ft = ?CountedForest(&subtree), "x-ray for {}", ext.display());

                let label = if ext.is_empty() {
                    "(none)".into()
                } else {
                    format!("**.{}", ext.display())
                };

                let color = colors.ext_color(ext);
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

        tracing::debug!("Rolled up entries into {} trees", entries.len());

        cumsum_size(sort_largest(entries))
    } else {
        treeify(args, &mut kidding, &root, &Default::default())
    }
}

pub fn rehash(
    colors: &ColorScheme,
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

            let color = colors.dir_color(&summary_parent);
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
            let color = colors.dir_color(&parent);
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

pub fn par_forest(
    colors: &ColorScheme,
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

    tracing::info!("Building forest in parallel with {num_threads} workers");
    if num_threads <= 1 {
        return make_forest(colors, args, root, leaves);
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
                    move || make_forest(colors, &args, &root, rx.into_iter().flatten())
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
            let mut results = span!(Level::DEBUG, "joining threads").in_scope(|| {
                handles
                    .into_iter()
                    .map(|h| match h {
                        Either::Left(h) => h.join().expect("Couldn't join threads"),
                        Either::Right(v) => v,
                    })
                    .inspect(|forest| {
                        tracing::trace!(forest = ?CountedForest(forest), "Make/merge forest result.")
                    })
                    .collect_vec()
            });

            tracing::debug!(
                shards = results.len(),
                "Forest workers done. Merging results."
            );

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
