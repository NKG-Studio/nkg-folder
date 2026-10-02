//! Immutable search blocks: edits rebuild only touched blocks, readers never hold the index lock.
use super::*;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    ops::Range,
};

const BLOCK: usize = 4096;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
mod cache;

fn grams(text: &str) -> impl Iterator<Item = u32> + '_ {
    text.as_bytes()
        .windows(3)
        .map(|b| u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16))
}

#[derive(Default)]
struct Block {
    items: Vec<Option<Arc<Item>>>,
    postings: HashMap<u32, Range<usize>>,
    ids: Vec<u16>,
    prefix: String,
    suffix: String,
    text: String,
    ranges: Vec<(usize, usize, usize)>,
}

impl Block {
    fn new(items: Vec<Option<Arc<Item>>>) -> Self {
        let mut lists: HashMap<u32, Vec<u16>> = HashMap::new();
        let mut unique = Vec::new();
        for (id, item) in items.iter().enumerate() {
            if let Some(item) = item {
                unique.clear();
                unique.extend(grams(&item.full_folded));
                unique.sort_unstable();
                unique.dedup();
                for gram in &unique {
                    lists.entry(*gram).or_default().push(id as u16);
                }
            }
        }
        let mut postings = HashMap::with_capacity(lists.len());
        let mut ids = Vec::with_capacity(lists.values().map(Vec::len).sum());
        for (gram, list) in lists {
            let start = ids.len();
            ids.extend(list);
            postings.insert(gram, start..ids.len());
        }
        let mut block = Self {
            items,
            postings,
            ids,
            ..Self::default()
        };
        block.common_text();
        block
    }

    fn common_text(&mut self) {
        self.text = String::with_capacity(
            self.items
                .iter()
                .flatten()
                .map(|item| item.full_folded.len())
                .sum(),
        );
        self.ranges = Vec::with_capacity(self.items.len());
        for item in &self.items {
            if let Some(item) = item {
                let start = self.text.len();
                self.text.push_str(&item.full_folded);
                self.ranges
                    .push((start, start + item.parent.folded.len(), self.text.len()));
            } else {
                self.ranges.push((0, 0, 0));
            }
        }
        let mut texts = self
            .items
            .iter()
            .flatten()
            .map(|item| item.full_folded.as_str());
        let Some(first) = texts.next() else {
            return;
        };
        let (mut prefix, mut suffix) = (first.len(), first.len());
        for text in texts {
            prefix = first[..prefix]
                .chars()
                .zip(text.chars())
                .take_while(|(a, b)| a == b)
                .map(|(c, _)| c.len_utf8())
                .sum();
            suffix = first[first.len() - suffix..]
                .chars()
                .rev()
                .zip(text.chars().rev())
                .take_while(|(a, b)| a == b)
                .map(|(c, _)| c.len_utf8())
                .sum();
            if prefix == 0 && suffix == 0 {
                break;
            }
        }
        self.prefix = first[..prefix].to_owned();
        self.suffix = first[first.len() - suffix..].to_owned();
    }

    fn matching(
        &self,
        query: &Query,
        prior: Option<&[u16]>,
        ticket: u64,
        current: &AtomicU64,
        limit: usize,
    ) -> Option<Vec<u16>> {
        let mut candidates = prior;
        for gram in &query.grams {
            let Some(range) = self.postings.get(gram) else {
                return Some(Vec::new());
            };
            let posting = &self.ids[range.clone()];
            if candidates.is_none_or(|ids| posting.len() < ids.len()) {
                candidates = Some(posting);
            }
        }
        if current.load(Ordering::Relaxed) != ticket {
            return None;
        }
        if self.all_match(query) {
            return Some(match candidates {
                Some(ids) => ids[..ids.len().min(limit)].to_vec(),
                None => self
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.is_some())
                    .map(|(id, _)| id as u16)
                    .take(limit)
                    .collect(),
            });
        }
        let mut found = Vec::new();
        let len = candidates.map_or(self.items.len(), <[u16]>::len);
        for offset in 0..len {
            if offset % 256 == 0 && current.load(Ordering::Relaxed) != ticket {
                return None;
            }
            let id = candidates.map_or(offset, |ids| usize::from(ids[offset]));
            let (start, name, end) = self.ranges[id];
            if end > start {
                let text = &self.text[start..end];
                let name = &self.text[name..end];
                if !query.words.iter().all(|word| {
                    text.ends_with(word.as_str())
                        || name.starts_with(word.as_str())
                        || text.contains(word.as_str())
                }) {
                    continue;
                }
                found.push(id as u16);
                if found.len() == limit {
                    break;
                }
            }
        }
        Some(found)
    }

    fn all_match(&self, query: &Query) -> bool {
        query
            .words
            .iter()
            .all(|word| self.prefix.contains(word.as_str()) || self.suffix.contains(word.as_str()))
    }
}

#[derive(Default, Clone)]
pub(super) struct Snapshot {
    blocks: Vec<Arc<Block>>,
    pub count: usize,
    pub checkpoints: Vec<Checkpoint>,
}

impl Snapshot {
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn iter(&self) -> impl Iterator<Item = &Item> {
        self.blocks
            .iter()
            .flat_map(|block| block.items.iter().filter_map(|item| item.as_deref()))
    }

    fn hits(&self, ids: &[Vec<u16>]) -> Vec<Hit> {
        ids.iter()
            .enumerate()
            .flat_map(|(block, ids)| {
                ids.iter().map(move |id| {
                    let item = self.blocks[block].items[usize::from(*id)].as_ref().unwrap();
                    Hit::new(item.path(), item.directory)
                })
            })
            .take(LIMIT)
            .collect()
    }
}

#[derive(Default)]
pub(super) struct Builder {
    snapshot: Snapshot,
    paths: HashMap<PathBuf, usize>,
    children: HashMap<PathBuf, HashSet<usize>>,
    directories: HashMap<PathBuf, HashSet<PathBuf>>,
    dirty: BTreeMap<usize, Vec<Option<Arc<Item>>>>,
    slots: usize,
}

impl Builder {
    pub fn checkpoints(&self) -> &[Checkpoint] {
        &self.snapshot.checkpoints
    }
    pub fn set_checkpoints(&mut self, checkpoints: Vec<Checkpoint>) {
        self.snapshot.checkpoints = checkpoints;
    }
    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        let mut paths = HashMap::with_capacity(snapshot.count);
        let mut children = HashMap::new();
        let mut directories = HashMap::new();
        for (block, data) in snapshot.blocks.iter().enumerate() {
            for (slot, item) in data.items.iter().enumerate() {
                if let Some(item) = item {
                    let path = item.path();
                    let id = block * BLOCK + slot;
                    if path != item.parent.path {
                        Self::register_child(
                            &mut children,
                            &mut directories,
                            &item.parent.path,
                            id,
                        );
                    }
                    paths.insert(path, id);
                }
            }
        }
        let slots = snapshot.blocks.last().map_or(0, |last| {
            (snapshot.blocks.len() - 1) * BLOCK + last.items.len()
        });
        Self {
            snapshot,
            paths,
            children,
            directories,
            dirty: BTreeMap::new(),
            slots,
        }
    }
    pub fn from_items(items: impl IntoIterator<Item = Item>) -> Self {
        let mut builder = Self::default();
        builder.extend(items);
        builder
    }

    fn block_mut(&mut self, block: usize) -> &mut Vec<Option<Arc<Item>>> {
        self.dirty.entry(block).or_insert_with(|| {
            self.snapshot
                .blocks
                .get(block)
                .map_or_else(Vec::new, |b| b.items.clone())
        })
    }

    fn register_child(
        children: &mut HashMap<PathBuf, HashSet<usize>>,
        directories: &mut HashMap<PathBuf, HashSet<PathBuf>>,
        parent: &Path,
        id: usize,
    ) {
        if let Some(ids) = children.get_mut(parent) {
            ids.insert(id);
            return;
        }
        children.insert(parent.to_owned(), HashSet::from([id]));
        // Keep implicit ancestors too: a cached/partial index can contain files before
        // their directory records. Removing that ancestor must still remove its subtree.
        let mut child = parent;
        while let Some(ancestor) = child.parent() {
            if !directories
                .entry(ancestor.to_owned())
                .or_default()
                .insert(child.to_owned())
            {
                break;
            }
            child = ancestor;
        }
    }

    pub fn extend(&mut self, items: impl IntoIterator<Item = Item>) {
        for item in items {
            let path = item.path();
            if let Some(id) = self.paths.get(&path).copied() {
                self.block_mut(id / BLOCK)[id % BLOCK] = Some(Arc::new(item));
            } else {
                let id = self.slots;
                self.slots += 1;
                if path != item.parent.path {
                    Self::register_child(
                        &mut self.children,
                        &mut self.directories,
                        &item.parent.path,
                        id,
                    );
                }
                self.paths.insert(path, id);
                self.block_mut(id / BLOCK).push(Some(Arc::new(item)));
                self.snapshot.count += 1;
            }
        }
    }

    pub fn remove_subtrees(&mut self, roots: &[PathBuf]) {
        let mut pending = roots.to_vec();
        while let Some(path) = pending.pop() {
            if let Some(directories) = self.directories.remove(&path) {
                pending.extend(directories);
            }
            if let Some(parent) = path.parent()
                && let Some(directories) = self.directories.get_mut(parent)
            {
                directories.remove(&path);
            }
            if let Some(children) = self.children.remove(&path) {
                for id in children {
                    if let Some(item) = self.item(id) {
                        pending.push(item.path());
                    }
                }
            }
            if let Some(id) = self.paths.remove(&path) {
                if let Some(parent) = self.item(id).map(|item| item.parent.path.clone())
                    && let Some(children) = self.children.get_mut(&parent)
                {
                    children.remove(&id);
                }
                self.block_mut(id / BLOCK)[id % BLOCK] = None;
                self.snapshot.count -= 1;
            }
        }
    }

    fn item(&self, id: usize) -> Option<&Item> {
        let items = self.dirty.get(&(id / BLOCK)).or_else(|| {
            self.snapshot
                .blocks
                .get(id / BLOCK)
                .map(|block| &block.items)
        })?;
        items.get(id % BLOCK)?.as_deref()
    }

    pub fn publish(&mut self) -> Arc<Snapshot> {
        let dirty: Vec<_> = std::mem::take(&mut self.dirty).into_iter().collect();
        let build = |chunks: Vec<(usize, Vec<Option<Arc<Item>>>)>| {
            chunks
                .into_iter()
                .map(|(id, items)| (id, Arc::new(Block::new(items))))
                .collect::<Vec<_>>()
        };
        let blocks = if dirty.len() < 4 {
            build(dirty)
        } else {
            let mut partitions: Vec<Vec<_>> = (0..4).map(|_| Vec::new()).collect();
            for (i, block) in dirty.into_iter().enumerate() {
                partitions[i % 4].push(block);
            }
            thread::scope(|scope| {
                let handles: Vec<_> = partitions
                    .into_iter()
                    .map(|part| scope.spawn(move || build(part)))
                    .collect();
                let mut blocks: Vec<_> = handles
                    .into_iter()
                    .flat_map(|h| h.join().expect("index build worker panicked"))
                    .collect();
                blocks.sort_unstable_by_key(|(id, _)| *id);
                blocks
            })
        };
        for (id, block) in blocks {
            if id == self.snapshot.blocks.len() {
                self.snapshot.blocks.push(block);
            } else {
                self.snapshot.blocks[id] = block;
            }
        }
        // Reclaim tombstones off the reader lock. IDs belong to a snapshot revision.
        if self.slots > BLOCK && self.slots - self.snapshot.count > self.slots / 4 {
            let items: Vec<_> = self.snapshot.iter().cloned().collect();
            let checkpoints = std::mem::take(&mut self.snapshot.checkpoints);
            *self = Self::from_items(items);
            self.snapshot.checkpoints = checkpoints;
            return self.publish();
        }
        Arc::new(self.snapshot.clone())
    }
}

#[derive(Clone)]
struct Query {
    words: Vec<String>,
    grams: Vec<u32>,
}

impl Query {
    fn new(text: &str) -> Self {
        let mut words: Vec<_> = text
            .replace('\\', "/")
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        words.sort();
        words.dedup();
        let grams = words
            .iter()
            .flat_map(|word| grams(word))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // Check long terms first, while preserving a canonical order for cache keys.
        words.sort_by_key(|word| std::cmp::Reverse(word.len()));
        Self { words, grams }
    }

    fn refines(&self, previous: &Self) -> bool {
        previous
            .words
            .iter()
            .all(|old| self.words.iter().any(|new| new.contains(old)))
    }
}

type Candidates = Vec<Vec<u16>>;
struct Cached {
    query: Query,
    ids: Arc<Candidates>,
    count: usize,
}
struct Job {
    snapshot: Arc<Snapshot>,
    query: Arc<Query>,
    prior: Option<Arc<Candidates>>,
    range: Range<usize>,
    ticket: u64,
    current: Arc<AtomicU64>,
    reply: mpsc::Sender<(usize, Option<Candidates>)>,
}

pub(super) struct Searcher {
    workers: Vec<mpsc::Sender<Job>>,
    cache: VecDeque<Cached>,
    revision: u64,
}

impl Searcher {
    pub fn new() -> Self {
        let count = thread::available_parallelism()
            .map_or(1, usize::from)
            .min(16);
        Self::with_workers(count)
    }

    fn with_workers(count: usize) -> Self {
        let workers = (0..count)
            .map(|_| {
                let (tx, rx) = mpsc::channel::<Job>();
                thread::spawn(move || {
                    while let Ok(job) = rx.recv() {
                        let start = job.range.start;
                        let ids = job
                            .range
                            .map(|block| {
                                job.snapshot.blocks[block].matching(
                                    &job.query,
                                    job.prior.as_ref().map(|ids| ids[block].as_slice()),
                                    job.ticket,
                                    &job.current,
                                    usize::MAX,
                                )
                            })
                            .collect::<Option<Candidates>>();
                        let _ = job.reply.send((start, ids));
                    }
                });
                tx
            })
            .collect();
        Self {
            workers,
            cache: VecDeque::new(),
            revision: u64::MAX,
        }
    }

    pub fn find(
        &mut self,
        snapshot: Arc<Snapshot>,
        revision: u64,
        text: &str,
        ticket: u64,
        current: &Arc<AtomicU64>,
        mut emit: impl FnMut(Results),
    ) {
        let start = Instant::now();
        if current.load(Ordering::Relaxed) != ticket {
            return;
        }
        if self.revision != revision {
            self.cache.clear();
            self.revision = revision;
        }
        let query = Arc::new(Query::new(text));
        let result = |ids: &Candidates, total, complete| Results {
            ticket,
            revision,
            hits: snapshot.hits(ids),
            total,
            complete,
            elapsed: start.elapsed(),
        };
        if query.words.is_empty() {
            emit(Results {
                ticket,
                revision,
                complete: true,
                ..Results::default()
            });
            return;
        }
        if let Some(position) = self.cache.iter().position(|c| c.query.words == query.words) {
            let cached = self.cache.remove(position).unwrap();
            emit(result(&cached.ids, cached.count, true));
            self.cache.push_back(cached);
            return;
        }
        let prior = self
            .cache
            .iter()
            .filter(|c| query.refines(&c.query))
            .min_by_key(|c| c.count)
            .map(|c| c.ids.clone());
        // Publish the ordered first page without waiting for an exact broad-query count.
        let mut first = Vec::new();
        let mut count = 0;
        for (block, data) in snapshot.blocks.iter().enumerate() {
            let Some(ids) = data.matching(
                &query,
                prior.as_ref().map(|ids| ids[block].as_slice()),
                ticket,
                current,
                LIMIT - count,
            ) else {
                return;
            };
            count += ids.len();
            first.push(ids);
            if count == LIMIT {
                break;
            }
        }
        if current.load(Ordering::Relaxed) != ticket {
            return;
        }
        if count < LIMIT {
            emit(result(&first, count, true));
            self.remember((*query).clone(), first, count);
            return;
        }
        emit(result(&first, count, false));
        let (estimated, cheap) = snapshot.blocks.iter().enumerate().fold(
            (0usize, true),
            |(total, cheap), (id, block)| {
                let prior = prior
                    .as_ref()
                    .map_or(block.items.len(), |ids| ids[id].len());
                let count = query
                    .grams
                    .iter()
                    .try_fold(prior, |smallest, gram| {
                        block
                            .postings
                            .get(gram)
                            .map(|range| smallest.min(range.len()))
                    })
                    .unwrap_or(0);
                (
                    total + count,
                    cheap && (count == 0 || block.all_match(&query)),
                )
            },
        );
        // Keep small candidate sets local; large scans amortize additional workers.
        let maximum = if cheap {
            self.workers.len().min(4)
        } else {
            self.workers.len()
        };
        let workers = (estimated / 250_000)
            .max(1)
            .next_power_of_two()
            .min(maximum);
        if workers == 1 {
            let ids: Option<Candidates> = snapshot
                .blocks
                .iter()
                .enumerate()
                .map(|(id, block)| {
                    block.matching(
                        &query,
                        prior.as_ref().map(|ids| ids[id].as_slice()),
                        ticket,
                        current,
                        usize::MAX,
                    )
                })
                .collect();
            if let Some(ids) = ids
                && current.load(Ordering::Relaxed) == ticket
            {
                let total = ids.iter().map(Vec::len).sum();
                emit(result(&ids, total, true));
                self.remember((*query).clone(), ids, total);
            }
            return;
        }
        let mut ids = vec![Vec::new(); snapshot.blocks.len()];
        let (tx, rx) = mpsc::channel();
        let width = snapshot.blocks.len().div_ceil(workers).max(1);
        let mut jobs = 0;
        for (worker, start) in (0..snapshot.blocks.len()).step_by(width).enumerate() {
            let job = Job {
                snapshot: snapshot.clone(),
                query: query.clone(),
                prior: prior.clone(),
                range: start..(start + width).min(snapshot.blocks.len()),
                ticket,
                current: current.clone(),
                reply: tx.clone(),
            };
            if self.workers[worker].send(job).is_err() {
                return;
            }
            jobs += 1;
        }
        drop(tx);
        for _ in 0..jobs {
            let Ok((start, Some(part))) = rx.recv() else {
                return;
            };
            for (offset, found) in part.into_iter().enumerate() {
                ids[start + offset] = found;
            }
        }
        if current.load(Ordering::Relaxed) != ticket {
            return;
        }
        let total = ids.iter().map(Vec::len).sum();
        emit(result(&ids, total, true));
        self.remember((*query).clone(), ids, total);
    }

    fn remember(&mut self, query: Query, ids: Candidates, count: usize) {
        self.cache.push_back(Cached {
            query,
            ids: Arc::new(ids),
            count,
        });
        while self.cache.len() > 32
            || self
                .cache
                .iter()
                .map(|c| {
                    c.ids.iter().map(|v| v.capacity() * 2).sum::<usize>()
                        + c.ids.capacity() * std::mem::size_of::<Vec<u16>>()
                })
                .sum::<usize>()
                > CACHE_BYTES
        {
            self.cache.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        searcher: &mut Searcher,
        snapshot: &Arc<Snapshot>,
        revision: u64,
        query: &str,
    ) -> Results {
        let current = Arc::new(AtomicU64::new(7));
        let mut last = None;
        searcher.find(snapshot.clone(), revision, query, 7, &current, |result| {
            last = Some(result)
        });
        let result = last.unwrap();
        assert!(result.complete);
        result
    }

    #[test]
    fn indexed_queries_cache_and_edits_match_reference() {
        let mut parents = HashMap::new();
        let items: Vec<_> = (0..20_000)
            .map(|i| {
                Item::new(
                    PathBuf::from(format!(
                        "C:/项目/Assets/group-{}/配置-file-{i}.META",
                        i / 100
                    )),
                    false,
                    &mut parents,
                )
            })
            .collect();
        let mut builder = Builder::from_items(items.iter().cloned());
        let snapshot = builder.publish();
        let mut searcher = Searcher::new();
        for query in [
            "",
            " ",
            "a",
            "配置",
            "配",
            "项",
            ".meta",
            "file-19999",
            "ASSETS .META",
            "group-199/配置-file-19999",
            "C:\\项目\\Assets",
            "meta file-19999",
            "file-1",
            "file-19",
            "file-19999",
            "file-19",
            "不存在",
            "assets group-1 file-12",
        ] {
            let expected = super::super::find_serial(&items, query, 7, &AtomicU64::new(7)).unwrap();
            let actual = run(&mut searcher, &snapshot, 1, query);
            assert_eq!(actual.total, expected.total, "{query}");
            assert_eq!(
                actual.hits.iter().map(|h| &h.path).collect::<Vec<_>>(),
                expected.hits.iter().map(|h| &h.path).collect::<Vec<_>>(),
                "{query}"
            );
        }
        // Cache invalidation, immutable old readers, component-aware subtree deletion.
        builder.remove_subtrees(&[PathBuf::from("C:/项目/Assets/group-1")]);
        builder.extend([Item::new(
            PathBuf::from("C:/项目/Assets/group-1/new.txt"),
            false,
            &mut parents,
        )]);
        let edited = builder.publish();
        assert_eq!(snapshot.count, 20_000);
        assert_eq!(edited.count, 19_901);
        assert_eq!(run(&mut searcher, &edited, 2, "group-1/配置").total, 0);
        assert_eq!(run(&mut searcher, &edited, 2, "group-10/配置").total, 100);
        assert_eq!(run(&mut searcher, &edited, 2, "new.txt").total, 1);
        assert_eq!(run(&mut searcher, &snapshot, 3, "group-1/配置").total, 100);
        // Reclaiming tombstones changes IDs only in the new revision.
        builder.remove_subtrees(&[PathBuf::from("C:/项目")]);
        let empty = builder.publish();
        assert_eq!(run(&mut searcher, &empty, 4, ".meta").total, 0);
        assert_eq!(snapshot.count, 20_000);
        let current = Arc::new(AtomicU64::new(8));
        searcher.find(snapshot, 5, ".meta", 7, &current, |_| {
            panic!("cancelled query published")
        });
    }

    #[test]
    fn million_path_index_latency_and_first_page() {
        let mut parents = HashMap::new();
        let items: Vec<_> = (0..1_000_000)
            .map(|i| {
                Item::new(
                    PathBuf::from(format!("C:/项目/Assets/group-{}/file-{i}.meta", i / 100)),
                    false,
                    &mut parents,
                )
            })
            .collect();
        let started = Instant::now();
        let mut builder = Builder::from_items(items.iter().cloned());
        let mapped = started.elapsed();
        let snapshot = builder.publish();
        println!(
            "1M index build: {:?} (path map {mapped:?})",
            started.elapsed()
        );
        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("index.bin");
        let started = Instant::now();
        super::super::save_cache(&path, &snapshot).unwrap();
        let saved = started.elapsed();
        let file = fs::File::open(&path).unwrap();
        let size = file.metadata().unwrap().len();
        let started = Instant::now();
        let loaded = Snapshot::read_cache(BufReader::new(file), size).unwrap();
        let decoded = started.elapsed();
        let mut restored = Builder::from_snapshot(loaded);
        assert_eq!(restored.publish().count, 1_000_000);
        println!(
            "1M binary cache: {:.1} MiB, save={saved:?}, decode={decoded:?}, restore={:?}",
            size as f64 / 1048576.0,
            started.elapsed()
        );
        drop(restored);
        let mut searcher = Searcher::new();
        for query in [
            ".meta",
            "group-345 file-34567",
            "assets/group-9999/file-999999",
            "没有匹配",
            "file-999999",
            "a",
        ] {
            let expected = super::super::find_serial(&items, query, 7, &AtomicU64::new(7)).unwrap();
            let mut cold = Vec::new();
            let mut warm = Vec::new();
            for revision in 1..=5 {
                let result = run(&mut searcher, &snapshot, revision, query);
                assert_eq!(result.total, expected.total);
                assert_eq!(
                    result.hits.iter().map(|h| &h.path).collect::<Vec<_>>(),
                    expected.hits.iter().map(|h| &h.path).collect::<Vec<_>>()
                );
                cold.push(result.elapsed);
                warm.push(run(&mut searcher, &snapshot, revision, query).elapsed);
            }
            cold.sort();
            warm.sort();
            println!(
                "1M indexed {query:?}: cold={:?}, cached={:?}",
                cold[2], warm[2]
            );
        }
        let current = Arc::new(AtomicU64::new(7));
        let mut stages = Vec::new();
        searcher.find(snapshot, 100, ".meta", 7, &current, |r| {
            stages.push((r.complete, r.total, r.elapsed))
        });
        assert_eq!(stages.len(), 2);
        assert!(!stages[0].0);
        assert_eq!(stages[0].1, LIMIT);
        assert_eq!(stages[1].1, 1_000_000);
        println!(
            "1M broad query first page: {:?}, complete: {:?}",
            stages[0].2, stages[1].2
        );
    }

    #[test]
    fn varied_unicode_substrings_and_cancelled_count_are_exact() {
        let mut parents = HashMap::new();
        let words = [
            "İSTANBUL",
            "配置",
            "Русский",
            "😀emoji",
            "résumé",
            "aababc",
            "x.meta",
            "AaAa",
            "目录 space",
        ];
        let items: Vec<_> = (0..9000)
            .map(|i| {
                Item::new(
                    PathBuf::from(format!(
                        "C:/目录-{}/{}/{}-{i}.{}",
                        i % 17,
                        words[i % words.len()],
                        words[(i * 7) % words.len()],
                        ["rs", "txt", "meta"][i % 3]
                    )),
                    i % 29 == 0,
                    &mut parents,
                )
            })
            .collect();
        let snapshot = Builder::from_items(items.iter().cloned()).publish();
        let mut searcher = Searcher::new();
        let current = Arc::new(AtomicU64::new(7));
        for i in 0..180 {
            let item = &items[(i * 499) % items.len()];
            let chars: Vec<_> = item
                .full_folded
                .char_indices()
                .map(|(i, _)| i)
                .chain(Some(item.full_folded.len()))
                .collect();
            let start = (i * 3) % (chars.len() - 1);
            let end = (start + i % 9 + 1).min(chars.len() - 1);
            let query = &item.full_folded[chars[start]..chars[end]];
            let expected = super::super::find_serial(&items, query, 7, &current).unwrap();
            let actual = run(&mut searcher, &snapshot, 1, query);
            assert_eq!(actual.total, expected.total, "{query:?}");
            assert_eq!(
                actual.hits.iter().map(|h| &h.path).collect::<Vec<_>>(),
                expected.hits.iter().map(|h| &h.path).collect::<Vec<_>>(),
                "{query:?}"
            );
        }
        let mut published = 0;
        searcher.find(snapshot.clone(), 2, "C:", 7, &current, |r| {
            assert!(!r.complete);
            published += 1;
            current.store(8, Ordering::Relaxed);
        });
        assert_eq!(published, 1);
        current.store(7, Ordering::Relaxed);
        assert_eq!(run(&mut searcher, &snapshot, 3, "C:").total, items.len());
    }

    #[test]
    #[ignore = "manual 10-million-entry performance and memory check"]
    fn ten_million_paths() {
        let mut parents = HashMap::new();
        let started = Instant::now();
        let mut builder = Builder::from_items((0..10_000_000).map(|i| {
            Item::new(
                PathBuf::from(format!(
                    "C:/项目/group-{}/file-{i}.{}",
                    i / 100,
                    ["rs", "meta", "txt"][i % 3]
                )),
                false,
                &mut parents,
            )
        }));
        let snapshot = builder.publish();
        println!(
            "10M build: {:?}, {} entries",
            started.elapsed(),
            snapshot.count
        );
        let mut searcher = Searcher::new();
        for (revision, query) in [
            "file-9999999",
            ".meta",
            "不存在",
            "项目",
            "group-99999/file-9999999",
            "file-9",
        ]
        .into_iter()
        .enumerate()
        {
            let expected = snapshot.iter().filter(|item| item.contains(query)).count();
            let result = run(&mut searcher, &snapshot, revision as u64, query);
            assert_eq!(result.total, expected, "{query}");
            let cached = run(&mut searcher, &snapshot, revision as u64, query);
            println!(
                "10M {query:?}: total={}, cold={:?}, cached={:?}",
                result.total, result.elapsed, cached.elapsed
            );
        }
        let started = Instant::now();
        builder.remove_subtrees(&[PathBuf::from("C:/项目/group-99999/file-9999999.rs")]);
        let updated = builder.publish();
        assert_eq!(updated.count, 9_999_999);
        assert_eq!(run(&mut searcher, &updated, 100, "file-9999999").total, 0);
        assert_eq!(run(&mut searcher, &snapshot, 101, "file-9999999").total, 1);
        println!(
            "10M single removal including search validation: {:?}",
            started.elapsed()
        );
        for workers in [1, 2, 4, 8, 16] {
            if workers > thread::available_parallelism().map_or(1, usize::from) {
                continue;
            }
            let mut searcher = Searcher::with_workers(workers);
            let mut times = Vec::new();
            for revision in 0..5 {
                let result = run(&mut searcher, &snapshot, revision, ".meta");
                assert_eq!(result.total, 3_333_333);
                if revision > 0 {
                    times.push(result.elapsed);
                }
            }
            times.sort();
            println!("10M broad count / {workers} workers: {:?}", times[2]);
        }
    }
}
