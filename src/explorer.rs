use std::collections::HashMap;

use eyre::Context as _;
use humansize::{DECIMAL, format_size};
use itertools::Itertools as _;

use tracing::{Level, instrument, span};
use tui_tree_widget::TreeItem;

use crate::core::{ENTRY_CHUNK_SIZE, Entry, StackAddr, TreeSlice};

#[instrument(level = "debug", skip_all)]
pub fn build_nav_tree(entries: TreeSlice) -> Vec<TreeItem<'static, usize>> {
    use crossbeam_channel::{Sender, unbounded};
    use itertools::Itertools as _;
    use smallvec::{SmallVec, smallvec};
    use std::thread;

    type Addr = SmallVec<[usize; 4]>;

    let num_leaves: usize = entries.iter().map(|(_, it)| it.leaves).sum();

    let num_workers = num_leaves / ENTRY_CHUNK_SIZE / 10;
    let num_cores = thread::available_parallelism()
        .map(|c| c.get())
        .unwrap_or(1);

    let num_threads = num_workers.min(num_cores);

    if num_threads <= 1 {
        return span!(Level::INFO, "skipping parallelism", num_leaves)
            .in_scope(|| tree_items(entries));
    }

    let (tx, rx) = unbounded::<(Addr, &Entry)>();

    fn split_tree<'a>(tx: &Sender<(Addr, &'a Entry)>, addr: &StackAddr, entry: &'a Entry) {
        if entry.leaves < ENTRY_CHUNK_SIZE * 4 {
            let mut addr: Addr = addr.collect();
            addr.reverse();
            tx.send((addr, entry)).expect("Couldn't send via channel");
            return;
        }

        for (id, it) in &entry.subtree {
            let addr = addr.push(*id);
            split_tree(tx, &addr, it);
        }
    }

    span!(Level::DEBUG, "Splitting tree", num_leaves, num_threads).in_scope(|| {
        let addr = StackAddr::root();
        for (id, it) in entries {
            let addr = addr.push(*id);
            split_tree(&tx, &addr, it);
        }
        drop(tx);
    });

    let mut shards = span!(Level::DEBUG, "transforming subtrees").in_scope(|| {
        thread::scope(|ts| {
            let handles = (0..num_threads)
                .map(|_| {
                    let rx = rx.clone();
                    ts.spawn(move || {
                        let mut branches: HashMap<Addr, TreeItem<'static, usize>> =
                            Default::default();

                        while let Ok((addr, entry)) = rx.recv() {
                            let id = addr.last().cloned().unwrap();
                            let nav = enty_to_nav(id, entry);
                            branches.insert(addr, nav);
                        }

                        branches
                    })
                })
                .collect_vec();

            handles
                .into_iter()
                .map(|it| it.join().expect("Could not join thread"))
                .collect_vec()
        })
    });

    fn search_shards(
        addr: &Addr,
        shards: &mut Vec<HashMap<Addr, TreeItem<'static, usize>>>,
    ) -> Option<TreeItem<'static, usize>> {
        for shard in shards {
            let item = shard.remove(addr);
            if item.is_some() {
                return item;
            }
        }
        None
    }

    fn combine_shards(
        addr: &Addr,
        entries: TreeSlice,
        shards: &mut Vec<HashMap<Addr, TreeItem<'static, usize>>>,
    ) -> Vec<TreeItem<'static, usize>> {
        let mut subtrees = vec![];
        for (id, entry) in entries {
            let mut addr = addr.clone();
            addr.push(*id);

            if let Some(tree) = search_shards(&addr, shards) {
                subtrees.push(tree);
            } else {
                let title = entry.path.file_name().unwrap_or_default();
                let text = format!("[{}] {}", format_size(entry.size, DECIMAL), title.display());
                if entry.subtree.is_empty() {
                    subtrees.push(TreeItem::new_leaf(*id, text));
                } else {
                    let subtree = combine_shards(&addr, entry.subtree.as_slice(), shards);
                    subtrees.push(make_tree_node(*id, text, subtree));
                }
            }
        }

        subtrees
    }

    span!(Level::DEBUG, "combining shards")
        .in_scope(|| combine_shards(&smallvec![], entries, &mut shards))
}

fn tree_items(entries: TreeSlice) -> Vec<TreeItem<'static, usize>> {
    entries
        .iter()
        .map(|(k, v)| enty_to_nav(*k, v))
        .collect_vec()
}

fn enty_to_nav(id: usize, entry: &Entry) -> TreeItem<'static, usize> {
    let title = entry.path.file_name().unwrap_or_default();
    let text = format!("[{}] {}", format_size(entry.size, DECIMAL), title.display());
    if entry.subtree.is_empty() {
        TreeItem::new_leaf(id, text)
    } else {
        let subtree = tree_items(entry.subtree.as_slice());
        make_tree_node(id, text, subtree)
    }
}

fn make_tree_node(
    id: usize,
    text: String,
    subtree: Vec<TreeItem<'static, usize>>,
) -> TreeItem<'static, usize> {
    let ids = subtree
        .iter()
        .map(|it| it.identifier())
        .cloned()
        .collect_vec();

    TreeItem::new(id, text, subtree)
        .context(format!("{ids:?}"))
        .unwrap()
}
