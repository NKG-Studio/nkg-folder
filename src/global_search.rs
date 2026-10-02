//! Local filename/path index. Disk I/O and matching never run on the UI thread.
use eframe::egui;
use notify::EventKind;
#[cfg(not(windows))]
use notify::{RecursiveMode, Watcher};
use serde::Serialize;
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const LIMIT: usize = 500;
mod index;
#[cfg(windows)]
mod ntfs;
mod scanner;
#[cfg(windows)]
mod watch;
use index::{Builder, Searcher, Snapshot};

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
struct Checkpoint {
    root: PathBuf,
    volume: u32,
    journal: u64,
    next: i64,
}

struct Parent {
    path: PathBuf,
    folded: String,
}

#[derive(Clone)]
struct Item {
    parent: Arc<Parent>,
    name: OsString,
    directory: bool,
    folded: String,
    full_folded: String,
}

impl Item {
    fn new(path: PathBuf, directory: bool, parents: &mut HashMap<PathBuf, Arc<Parent>>) -> Self {
        let directory_path = path.parent().unwrap_or(&path);
        let parent = parents
            .entry(directory_path.to_owned())
            .or_insert_with(|| {
                Arc::new(Parent {
                    path: directory_path.to_owned(),
                    folded: format!(
                        "{}/",
                        directory_path
                            .to_string_lossy()
                            .replace('\\', "/")
                            .trim_end_matches('/')
                            .to_lowercase()
                    ),
                })
            })
            .clone();
        let name = path.file_name().unwrap_or_default().to_owned();
        let folded = name.to_string_lossy().to_lowercase();
        let full_folded = format!("{}{folded}", parent.folded);
        Self {
            parent,
            name,
            directory,
            folded,
            full_folded,
        }
    }

    fn path(&self) -> PathBuf {
        self.parent.path.join(&self.name)
    }

    #[cfg(test)]
    fn contains(&self, word: &str) -> bool {
        self.folded.contains(word)
            || self.parent.folded.contains(word)
            || word.match_indices('/').any(|(i, _)| {
                self.parent.folded.ends_with(&word[..=i]) && self.folded.starts_with(&word[i + 1..])
            })
    }
}

impl Serialize for Item {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (self.path(), self.directory).serialize(serializer)
    }
}

// Decode one record at a time and share parents immediately, avoiding a second full path list.
fn load_cache(reader: impl std::io::Read) -> Result<Vec<Item>, serde_json::Error> {
    struct Items;
    impl<'de> serde::de::Visitor<'de> for Items {
        type Value = Vec<Item>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("file index records")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut items = Vec::new();
            let mut parents = HashMap::new();
            while let Some((path, directory)) = seq.next_element::<(PathBuf, bool)>()? {
                items.push(Item::new(path, directory, &mut parents));
            }
            Ok(items)
        }
    }
    use serde::Deserializer;
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let items = deserializer.deserialize_seq(Items)?;
    deserializer.end()?;
    Ok(items)
}

#[derive(Default)]
struct Index {
    items: Arc<Snapshot>,
    remote_count: Option<usize>,
    failed: bool,
    revision: u64,
    scanning: bool,
    skipped: usize,
    warning: String,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct Hit {
    #[serde(with = "crate::backend::native_path")]
    pub path: PathBuf,
    pub directory: bool,
    title: String,
    display: String,
}

impl Hit {
    fn new(path: PathBuf, directory: bool) -> Self {
        let title = format!(
            "{}  {}",
            if directory { "文件夹" } else { "文件" },
            crate::model::path_name(&path)
        );
        let display = crate::model::display_path(&path);
        Self {
            path,
            directory,
            title,
            display,
        }
    }
}

#[derive(Default, Serialize, serde::Deserialize)]
struct Results {
    ticket: u64,
    revision: u64,
    hits: Vec<Hit>,
    total: usize,
    elapsed: Duration,
    complete: bool,
}

struct Engine {
    index: Arc<RwLock<Index>>,
    stop: Arc<AtomicBool>,
    rebuild: Arc<AtomicBool>,
    ticket: Arc<AtomicU64>,
    tx: mpsc::Sender<(u64, String)>,
    rx: mpsc::Receiver<Results>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.ticket.fetch_add(1, Ordering::Relaxed);
    }
}

impl Engine {
    fn start(roots: Vec<PathBuf>, cache: Option<PathBuf>, ctx: egui::Context) -> Self {
        if let Some(client) = crate::backend::client() {
            return Self::remote(roots, cache, ctx, client.clone());
        }
        let index = Arc::new(RwLock::new(Index {
            scanning: true,
            ..Index::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let rebuild = Arc::new(AtomicBool::new(false));
        let ticket = Arc::new(AtomicU64::new(0));
        let (tx, queries) = mpsc::channel::<(u64, String)>();
        let (results, rx) = mpsc::channel();
        let engine = Self {
            index,
            stop,
            rebuild,
            ticket,
            tx,
            rx,
        };
        let data = engine.index.clone();
        let stop = engine.stop.clone();
        let current = engine.ticket.clone();
        let repaint = ctx.clone();
        thread::spawn(move || {
            let mut searcher = Searcher::new();
            while !stop.load(Ordering::Relaxed) {
                let Ok(mut query) = queries.recv_timeout(Duration::from_millis(200)) else {
                    continue;
                };
                while let Ok(newer) = queries.try_recv() {
                    query = newer;
                }
                let (snapshot, revision) = {
                    let index = data.read().unwrap();
                    (index.items.clone(), index.revision)
                };
                searcher.find(snapshot, revision, &query.1, query.0, &current, |result| {
                    let _ = results.send(result);
                    repaint.request_repaint();
                });
            }
        });
        let data = engine.index.clone();
        let stop = engine.stop.clone();
        let rebuild = engine.rebuild.clone();
        thread::spawn(move || maintain(data, roots, cache, stop, rebuild, ctx));
        engine
    }
}

#[derive(Serialize, serde::Deserialize)]
enum RemoteEvent {
    Status {
        count: usize,
        revision: u64,
        scanning: bool,
        skipped: usize,
        warning: String,
    },
    Result(Results),
}

pub(crate) struct RemoteControl {
    ticket: Arc<AtomicU64>,
    tx: mpsc::Sender<(u64, String)>,
    rebuild: Arc<AtomicBool>,
}
impl RemoteControl {
    pub fn query(&self, ticket: u64, query: String) {
        self.ticket.store(ticket, Ordering::Relaxed);
        if !query.trim().is_empty() {
            let _ = self.tx.send((ticket, query));
        }
    }
    pub fn rebuild(&self) {
        self.rebuild.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn serve_remote(
    roots: Vec<PathBuf>,
    cache: Option<PathBuf>,
    sink: &crate::backend::Sink,
    ready: impl FnOnce(RemoteControl),
) {
    let engine = Engine::start(roots, cache, egui::Context::default());
    ready(RemoteControl {
        ticket: engine.ticket.clone(),
        tx: engine.tx.clone(),
        rebuild: engine.rebuild.clone(),
    });
    let mut last_status = Instant::now() - Duration::from_secs(1);
    while !sink.stop.load(Ordering::Relaxed) {
        if last_status.elapsed() >= Duration::from_millis(100) {
            let index = engine.index.read().unwrap();
            if !sink.emit(
                &RemoteEvent::Status {
                    count: index.items.len(),
                    revision: index.revision,
                    scanning: index.scanning,
                    skipped: index.skipped,
                    warning: index.warning.clone(),
                },
                false,
            ) {
                break;
            }
            last_status = Instant::now();
        }
        match engine.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => {
                if !sink.emit(&RemoteEvent::Result(result), false) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

impl Engine {
    fn remote(
        roots: Vec<PathBuf>,
        cache: Option<PathBuf>,
        ctx: egui::Context,
        client: Arc<crate::backend::Client>,
    ) -> Self {
        let index = Arc::new(RwLock::new(Index {
            scanning: true,
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let rebuild = Arc::new(AtomicBool::new(false));
        let ticket = Arc::new(AtomicU64::new(0));
        let (tx, requests) = mpsc::channel::<(u64, String)>();
        let (results, rx) = mpsc::channel();
        let engine = Self {
            index: index.clone(),
            stop: stop.clone(),
            rebuild: rebuild.clone(),
            ticket: ticket.clone(),
            tx,
            rx,
        };
        thread::spawn(move || {
            let run = || -> Result<(), String> {
                let stream = client.stream(crate::backend::Command::Global { roots, cache })?;
                let session = stream.id;
                // First status acknowledges registration before queries may reference this session.
                let (first, _): (RemoteEvent, bool) = stream.recv()?;
                let control_stop = stop.clone();
                let control = client.clone();
                thread::spawn(move || {
                    let mut last_ticket = 0;
                    while !control_stop.load(Ordering::Relaxed) {
                        match requests.recv_timeout(Duration::from_millis(10)) {
                            Ok((number, query)) => {
                                last_ticket = number;
                                control.send(crate::backend::Command::Query {
                                    session,
                                    ticket: number,
                                    query,
                                });
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        let current = ticket.load(Ordering::Relaxed);
                        if current != last_ticket {
                            last_ticket = current;
                            control.send(crate::backend::Command::Query {
                                session,
                                ticket: current,
                                query: String::new(),
                            });
                        }
                        if rebuild.swap(false, Ordering::Relaxed) {
                            control.send(crate::backend::Command::Rebuild(session));
                        }
                    }
                    control.send(crate::backend::Command::Cancel(session));
                });
                let mut event = first;
                loop {
                    match event {
                        RemoteEvent::Status {
                            count,
                            revision,
                            scanning,
                            skipped,
                            warning,
                        } => {
                            let mut index = index.write().unwrap();
                            index.remote_count = Some(count);
                            index.revision = revision;
                            index.scanning = scanning;
                            index.skipped = skipped;
                            index.warning = warning;
                        }
                        RemoteEvent::Result(result) => {
                            if results.send(result).is_err() {
                                break;
                            }
                        }
                    }
                    ctx.request_repaint();
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    event = stream.recv()?.0;
                }
                Ok(())
            };
            if let Err(error) = run() {
                let mut index = index.write().unwrap();
                index.scanning = false;
                index.warning = error;
                index.failed = true;
            }
            stop.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
        engine
    }
}

#[cfg(test)]
fn find(items: &[Item], query: &str, ticket: u64, current: &AtomicU64) -> Option<Results> {
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4);
    if items.len() < 250_000 || workers == 1 || query.trim().is_empty() {
        return find_serial(items, query, ticket, current);
    }
    let start = Instant::now();
    thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(items.len().div_ceil(workers))
            .map(|chunk| scope.spawn(move || find_serial(chunk, query, ticket, current)))
            .collect();
        let mut result = Results {
            ticket,
            ..Results::default()
        };
        // Join in index order, preserving the same first 500 hits as a serial search.
        for handle in handles {
            let partial = handle.join().expect("search worker panicked")?;
            result.total += partial.total;
            result
                .hits
                .extend(partial.hits.into_iter().take(LIMIT - result.hits.len()));
        }
        if current.load(Ordering::Relaxed) != ticket {
            return None;
        }
        result.elapsed = start.elapsed();
        Some(result)
    })
}

#[cfg(test)]
fn find_serial(items: &[Item], query: &str, ticket: u64, current: &AtomicU64) -> Option<Results> {
    let start = Instant::now();
    let normalized = query.replace('\\', "/").to_lowercase();
    let words: Vec<_> = normalized.split_whitespace().collect();
    let mut result = Results {
        ticket,
        ..Results::default()
    };
    if !words.is_empty() {
        for (i, item) in items.iter().enumerate() {
            if i % 1024 == 0 && current.load(Ordering::Relaxed) != ticket {
                return None;
            }
            if words.iter().all(|word| item.contains(word)) {
                result.total += 1;
                // ponytail: retain 500 hits; add paging if browsing broad queries becomes necessary.
                if result.hits.len() < LIMIT {
                    result.hits.push(Hit::new(item.path(), item.directory));
                }
            }
        }
    }
    result.elapsed = start.elapsed();
    Some(result)
}

#[cfg(test)]
fn scan(roots: &[PathBuf], stop: &AtomicBool, mut batch: impl FnMut(Vec<Item>)) -> (usize, String) {
    let mut pending: Vec<_> = roots.iter().cloned().map(|path| (path, None)).collect();
    let mut items = Vec::new();
    let mut parents = HashMap::new();
    let mut skipped = 0;
    let mut warning = String::new();
    while let Some((path, cached)) = pending.pop() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let metadata = match cached
            .map(Ok)
            .unwrap_or_else(|| fs::symlink_metadata(&path))
        {
            Ok(metadata) => metadata,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    skipped += 1;
                    if warning.is_empty() {
                        warning = format!("{}：{error}", crate::model::display_path(&path));
                    }
                }
                continue;
            }
        };
        let directory = metadata.is_dir();
        let mut link = metadata.file_type().is_symlink();
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            link |= metadata.file_attributes() & 0x400 != 0;
        }
        items.push(Item::new(path.clone(), directory, &mut parents));
        if directory && !link {
            match fs::read_dir(&path) {
                Ok(entries) => {
                    for entry in entries {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        match entry {
                            // On Windows DirEntry metadata comes from the directory enumeration.
                            Ok(entry) => pending.push((entry.path(), entry.metadata().ok())),
                            Err(error) => {
                                skipped += 1;
                                if warning.is_empty() {
                                    warning = error.to_string();
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    skipped += 1;
                    if warning.is_empty() {
                        warning = format!("{}：{error}", crate::model::display_path(&path));
                    }
                }
            }
        }
        if items.len() >= 4096 {
            batch(std::mem::take(&mut items));
        }
    }
    if !items.is_empty() {
        batch(items);
    }
    (skipped, warning)
}

fn scan_parallel(
    roots: &[PathBuf],
    stop: &AtomicBool,
    batch: impl FnMut(Vec<Item>) + Send,
) -> (usize, String) {
    scanner::run(roots, stop, batch)
}

fn save_cache(path: &Path, items: &Snapshot) -> Result<(), String> {
    crate::backend::user_io(|| save_cache_as_user(path, items))
}

fn save_cache_as_user(path: &Path, items: &Snapshot) -> Result<(), String> {
    let mut file = tempfile::Builder::new()
        .prefix(".nkg-index-")
        .tempfile_in(path.parent().ok_or("索引路径无父目录")?)
        .map_err(|e| e.to_string())?;
    {
        let mut writer = BufWriter::new(file.as_file_mut());
        items.write_cache(&mut writer).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
    }
    file.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}

fn maintain(
    data: Arc<RwLock<Index>>,
    roots: Vec<PathBuf>,
    cache: Option<PathBuf>,
    stop: Arc<AtomicBool>,
    rebuild: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    let mut builder = Builder::default();
    let publish = |builder: &mut Builder| {
        let snapshot = builder.publish();
        let old = {
            let mut index = data.write().unwrap();
            index.revision += 1;
            std::mem::replace(&mut index.items, snapshot)
        };
        drop(old);
        ctx.request_repaint();
    };
    // Bounded notifications: overflow triggers a full reconciliation instead of losing changes.
    let (events, receiver) = mpsc::sync_channel(65_536);
    #[cfg(windows)]
    let (_watcher, mut watch_warning) = watch::Watches::start(&roots, events, rebuild.clone());
    #[cfg(not(windows))]
    let (_watcher, mut watch_warning) = {
        let overflow = rebuild.clone();
        let mut watcher = notify::RecommendedWatcher::new(
            move |event: notify::Result<notify::Event>| {
                if let Ok(event) = &event
                    && !event.need_rescan()
                    && !matches!(
                        event.kind,
                        EventKind::Create(_)
                            | EventKind::Remove(_)
                            | EventKind::Modify(notify::event::ModifyKind::Name(_))
                            | EventKind::Any
                    )
                {
                    return;
                }
                if events.try_send(event).is_err() {
                    overflow.store(true, Ordering::Relaxed);
                }
            },
            notify::Config::default().with_follow_symlinks(false),
        );
        let mut watch_warning = String::new();
        match &mut watcher {
            Ok(watcher) => {
                for root in &roots {
                    if let Err(error) = watcher.watch(root, RecursiveMode::Recursive) {
                        watch_warning = format!("部分位置无法自动更新，请手动重建：{error}");
                    }
                }
            }
            Err(error) => watch_warning = format!("自动更新不可用，请手动重建：{error}"),
        }
        (watcher, watch_warning)
    };
    if let Some(path) = &cache {
        let _ =
            crate::backend::user_io(|| {
                let loaded = fs::File::open(path).ok().and_then(|file| {
                    let size = file.metadata().ok()?.len();
                    Snapshot::read_cache(BufReader::new(file), size).ok()
                });
                if let Some(snapshot) = loaded {
                    if snapshot
                        .iter()
                        .all(|item| roots.iter().any(|root| item.parent.path.starts_with(root)))
                    {
                        builder = Builder::from_snapshot(snapshot);
                    } else {
                        builder.extend(
                            snapshot
                                .iter()
                                .filter(|item| {
                                    roots.iter().any(|root| item.parent.path.starts_with(root))
                                })
                                .cloned(),
                        );
                    }
                } else if let Ok(file) = fs::File::open(path.with_file_name("file-index.json"))
                    && let Ok(items) = load_cache(BufReader::new(file))
                {
                    builder.extend(items.into_iter().filter(|item| {
                        roots.iter().any(|root| item.parent.path.starts_with(root))
                    }));
                }
                publish(&mut builder);
                Ok(())
            });
    }
    let mut full = true;
    let mut startup = true;
    let mut dirty = false;
    let mut last_save = Instant::now() - Duration::from_secs(30);
    let mut saving: Option<mpsc::Receiver<Result<(), String>>> = None;
    while !stop.load(Ordering::Relaxed) {
        let requested = rebuild.swap(false, Ordering::Relaxed);
        if full || requested {
            let partial = data.read().unwrap().items.is_empty();
            data.write().unwrap().scanning = true;
            ctx.request_repaint();
            let mut scan_roots = Vec::new();
            let mut checkpoints = Vec::new();
            for root in &roots {
                #[cfg(windows)]
                if startup
                    && !partial
                    && !requested
                    && let Some(old) = builder.checkpoints().iter().find(|c| &c.root == root)
                    && let Some((paths, checkpoint)) = ntfs::changes(old, &stop)
                {
                    scan_roots.extend(paths);
                    checkpoints.push(checkpoint);
                    continue;
                }
                scan_roots.push(root.clone());
                #[cfg(windows)]
                if let Some(checkpoint) = ntfs::checkpoint(root) {
                    checkpoints.push(checkpoint);
                }
            }
            let reuse = startup
                && !partial
                && !requested
                && roots.iter().any(|root| !scan_roots.contains(root));
            let mut next = if reuse {
                std::mem::take(&mut builder)
            } else {
                Builder::default()
            };
            startup = false;
            next.remove_subtrees(&scan_roots);
            next.set_checkpoints(checkpoints);
            let mut last_publish = Instant::now();
            let (skipped, warning) = scan_parallel(&scan_roots, &stop, |mut batch| {
                batch.retain(|item| !cached_item(item, cache.as_deref()));
                next.extend(batch);
                if partial && last_publish.elapsed() >= Duration::from_millis(100) {
                    publish(&mut next);
                    last_publish = Instant::now();
                }
            });
            if stop.load(Ordering::Relaxed) {
                break;
            }
            builder = next;
            publish(&mut builder);
            let mut index = data.write().unwrap();
            index.skipped = skipped;
            index.warning = [watch_warning.as_str(), warning.as_str()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("；");
            index.scanning = false;
            index.revision += 1;
            full = false;
            dirty = true;
            ctx.request_repaint();
        }
        let mut changed = Vec::new();
        let until = Instant::now() + Duration::from_millis(40);
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            match receiver.recv_timeout(Duration::from_millis(40)) {
                Ok(Ok(event)) => {
                    if event.need_rescan() {
                        full = true;
                    }
                    changed.extend(event.paths.into_iter().filter(|p| {
                        roots.iter().any(|root| p.starts_with(root))
                            && !cache_file(p, cache.as_deref())
                    }));
                }
                Ok(Err(error)) => {
                    watch_warning = format!("目录通知失败，已安排核对；后续请手动重建：{error}");
                    data.write().unwrap().warning.clone_from(&watch_warning);
                    full = true;
                    ctx.request_repaint();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    thread::sleep(Duration::from_millis(40));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        if full {
            continue;
        }
        #[cfg(windows)]
        let next_checkpoints =
            if dirty && saving.is_none() && last_save.elapsed() > Duration::from_secs(30) {
                let mut checkpoints = Vec::new();
                for old in builder.checkpoints() {
                    if let Some((paths, checkpoint)) = ntfs::changes(old, &stop) {
                        changed.extend(paths);
                        checkpoints.push(checkpoint);
                    } else {
                        changed.push(old.root.clone());
                        if let Some(checkpoint) = ntfs::checkpoint(&old.root) {
                            checkpoints.push(checkpoint);
                        }
                    }
                }
                Some(checkpoints)
            } else {
                None
            };
        changed.sort();
        changed.dedup();
        // A changed parent covers descendants, including directory rename/delete.
        let mut minimal: Vec<PathBuf> = Vec::new();
        for path in changed {
            if !minimal
                .last()
                .is_some_and(|parent| path.starts_with(parent))
            {
                minimal.push(path);
            }
        }
        if !minimal.is_empty() {
            let mut additions = Vec::new();
            let (skipped, warning) =
                scan_parallel(&minimal, &stop, |batch| additions.extend(batch));
            if stop.load(Ordering::Relaxed) {
                break;
            }
            additions.retain(|item| !cached_item(item, cache.as_deref()));
            builder.remove_subtrees(&minimal);
            builder.extend(additions);
            publish(&mut builder);
            let mut index = data.write().unwrap();
            index.skipped += skipped;
            if !warning.is_empty() {
                index.warning = [watch_warning.as_str(), warning.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("；");
            }
            index.revision += 1;
            dirty = true;
            ctx.request_repaint();
        }
        #[cfg(windows)]
        if let Some(checkpoints) = next_checkpoints {
            builder.set_checkpoints(checkpoints);
            publish(&mut builder);
        }
        if let Some(receiver) = &saving {
            match receiver.try_recv() {
                Ok(result) => {
                    if let Err(error) = result {
                        data.write().unwrap().warning = format!("索引缓存未保存：{error}");
                        dirty = true;
                        ctx.request_repaint();
                    }
                    saving = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    saving = None;
                    dirty = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if dirty && saving.is_none() && last_save.elapsed() > Duration::from_secs(30) {
            if let Some(path) = &cache {
                let path = path.clone();
                let snapshot = data.read().unwrap().items.clone();
                let (tx, rx) = mpsc::channel();
                thread::spawn(move || {
                    let _ = tx.send(save_cache(&path, &snapshot));
                });
                saving = Some(rx);
            }
            dirty = false;
            last_save = Instant::now();
        }
    }
}

fn cache_file(path: &Path, cache: Option<&Path>) -> bool {
    cache.is_some_and(|cache| {
        path == cache
            || path == cache.with_file_name("file-index.json")
            || (path.parent() == cache.parent()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".nkg-index-")))
    })
}

fn cached_item(item: &Item, cache: Option<&Path>) -> bool {
    cache.is_some_and(|cache| {
        cache.parent() == Some(item.parent.path.as_path()) && cache_file(&item.path(), Some(cache))
    })
}

pub fn local_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        use windows::{
            Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives},
            core::PCWSTR,
        };
        let mask = unsafe { GetLogicalDrives() };
        (0..26)
            .filter(|i| mask & (1 << i) != 0)
            .filter_map(|i| {
                let root = format!("{}:\\", (b'A' + i) as char);
                let wide: Vec<_> = root.encode_utf16().chain(Some(0)).collect();
                // Fixed and removable local disks; network/offline locations must not stall local indexing.
                matches!(unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) }, 2 | 3)
                    .then(|| PathBuf::from(root))
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    #[ignore = "read-only real-disk indexing benchmark, stops at one million entries"]
    fn live_million_index_latency() {
        let start = Instant::now();
        let engine = Engine::start(local_roots(), None, egui::Context::default());
        let mut first = None;
        loop {
            let (count, scanning) = {
                let index = engine.index.read().unwrap();
                (index.items.len(), index.scanning)
            };
            if count > 0 && first.is_none() {
                first = Some(start.elapsed());
            }
            if count >= 1_000_000 || !scanning || start.elapsed() > Duration::from_secs(120) {
                eprintln!(
                    "real_index first_searchable={first:?} count={count} elapsed={:?} scanning={scanning}",
                    start.elapsed()
                );
                assert!(count > 0);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    #[ignore = "read-only first-batch latency probe on local volumes"]
    fn live_first_batch_latency() {
        for root in local_roots() {
            let stop = AtomicBool::new(false);
            let started = Instant::now();
            let mut first = None;
            scan_parallel(std::slice::from_ref(&root), &stop, |items| {
                if first.is_none() && !items.is_empty() {
                    first = Some(started.elapsed());
                    stop.store(true, Ordering::Relaxed);
                }
            });
            eprintln!(
                "first_batch root={} latency={first:?} cancel_complete={:?}",
                root.display(),
                started.elapsed()
            );
            assert!(first.is_some(), "no initial batch on {}", root.display());
        }
    }

    #[test]
    fn scanner_does_not_follow_junctions() {
        use std::os::windows::process::CommandExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("must-not-be-indexed.txt"), "data").unwrap();
        let link = root.join("junction");
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut found = Vec::new();
        scan_parallel(
            std::slice::from_ref(&root),
            &AtomicBool::new(false),
            |batch| found.extend(batch.into_iter().map(|item| item.path())),
        );
        found.sort();
        assert_eq!(found, vec![root, link.clone()]);
        fs::remove_dir(link).unwrap();
        assert!(outside.join("must-not-be-indexed.txt").exists());
    }

    #[test]
    fn typing_submits_in_the_same_frame_and_empty_input_cancels() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut search = GlobalSearch {
            open: true,
            focus: true,
            engine: Some(Engine::start(
                vec![temp.path().to_owned()],
                None,
                ctx.clone(),
            )),
            ..GlobalSearch::default()
        };
        let frame = |search: &mut GlobalSearch, events| {
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 800.0),
                    )),
                    events,
                    ..Default::default()
                },
                |_| {
                    search.show(&ctx);
                },
            );
        };
        frame(&mut search, vec![]);
        frame(&mut search, vec![egui::Event::Text("x".into())]);
        assert_eq!(search.query, "x");
        assert!(
            search.deadline.is_none(),
            "typing must not start a debounce timer"
        );
        assert!(
            search
                .engine
                .as_ref()
                .unwrap()
                .ticket
                .load(Ordering::Relaxed)
                >= 2
        );
        let ticket = search
            .engine
            .as_ref()
            .unwrap()
            .ticket
            .load(Ordering::Relaxed);
        frame(
            &mut search,
            vec![egui::Event::Key {
                key: egui::Key::Backspace,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
        );
        assert!(search.query.is_empty());
        assert!(search.deadline.is_none());
        assert!(search.results.hits.is_empty());
        assert!(
            search
                .engine
                .as_ref()
                .unwrap()
                .ticket
                .load(Ordering::Relaxed)
                > ticket
        );
    }

    #[test]
    fn legacy_cache_is_readable_and_rejects_trailing_garbage() {
        let input = br#"[["C:/folder/a.txt",false],["C:/folder/b.txt",false]]"#;
        let items = load_cache(input.as_slice()).unwrap();
        assert_eq!(items.len(), 2);
        assert!(Arc::ptr_eq(&items[0].parent, &items[1].parent));
        let mut invalid = input.to_vec();
        invalid.extend(b"garbage");
        assert!(load_cache(invalid.as_slice()).is_err());
    }

    #[test]
    fn bulk_changes_hard_links_and_restart_converge() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        fs::create_dir_all(root.join("old/deep")).unwrap();
        let cache = temp.path().join("index.bin");
        let engine = Engine::start(
            vec![root.clone()],
            Some(cache.clone()),
            egui::Context::default(),
        );
        for i in 0..1200 {
            fs::write(root.join(format!("old/deep/file-{i}.txt")), "data").unwrap();
        }
        fs::hard_link(root.join("old/deep/file-0.txt"), root.join("alias.txt")).unwrap();
        fs::rename(root.join("old"), root.join("renamed")).unwrap();
        for i in (0..1200).step_by(3) {
            fs::remove_file(root.join(format!("renamed/deep/file-{i}.txt"))).unwrap();
        }
        let mut expected = Vec::new();
        scan(
            std::slice::from_ref(&root),
            &AtomicBool::new(false),
            |batch| expected.extend(batch.into_iter().map(|i| i.path())),
        );
        expected.sort();
        let start = Instant::now();
        loop {
            let snapshot = engine.index.read().unwrap().items.clone();
            let mut actual: Vec<_> = snapshot.iter().map(Item::path).collect();
            actual.sort();
            if actual == expected && !engine.index.read().unwrap().scanning {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "bulk notifications did not converge: {} vs {}",
                actual.len(),
                expected.len()
            );
            thread::sleep(Duration::from_millis(20));
        }
        let snapshot = engine.index.read().unwrap().items.clone();
        save_cache(&cache, &snapshot).unwrap();
        let restored = Engine::start(vec![root], Some(cache), egui::Context::default());
        let start = Instant::now();
        loop {
            let index = restored.index.read().unwrap();
            if !index.scanning {
                let mut actual: Vec<_> = index.items.iter().map(Item::path).collect();
                actual.sort();
                assert_eq!(actual, expected);
                break;
            }
            drop(index);
            assert!(start.elapsed() < Duration::from_secs(20));
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn parallel_query_preserves_results_and_measures_latency() {
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
        let ticket = AtomicU64::new(1);
        for query in [
            ".meta",
            "group-345 file-34567",
            "assets/group-9999/file-999999",
            "没有匹配",
        ] {
            let mut serial_times = Vec::new();
            let mut parallel_times = Vec::new();
            for round in 0..6 {
                let (serial, parallel) = if round % 2 == 0 {
                    (
                        find_serial(&items, query, 1, &ticket).unwrap(),
                        find(&items, query, 1, &ticket).unwrap(),
                    )
                } else {
                    let parallel = find(&items, query, 1, &ticket).unwrap();
                    (find_serial(&items, query, 1, &ticket).unwrap(), parallel)
                };
                assert_eq!(serial.total, parallel.total);
                assert_eq!(
                    serial.hits.iter().map(|h| &h.path).collect::<Vec<_>>(),
                    parallel.hits.iter().map(|h| &h.path).collect::<Vec<_>>()
                );
                if round > 0 {
                    serial_times.push(serial.elapsed);
                    parallel_times.push(parallel.elapsed);
                }
            }
            serial_times.sort();
            parallel_times.sort();
            println!(
                "1M paths {query:?}: serial median={:?}, parallel median={:?}",
                serial_times[2], parallel_times[2]
            );
        }
        assert!(find(&items, ".meta", 0, &ticket).is_none());
    }

    #[test]
    fn parallel_scan_matches_serial_and_cancels() {
        let temp = tempfile::tempdir().unwrap();
        let roots: Vec<_> = (0..6)
            .map(|i| temp.path().join(format!("disk-{i}")))
            .collect();
        for root in &roots {
            fs::create_dir_all(root.join("nested/empty")).unwrap();
            for i in 0..40 {
                fs::write(root.join(format!("nested/文件-{i}.txt")), "test").unwrap();
            }
        }
        let stop = AtomicBool::new(false);
        let mut serial = Vec::new();
        let mut parallel = Vec::new();
        let serial_status = scan(&roots, &stop, |items| {
            serial.extend(items.into_iter().map(|item| item.path()))
        });
        let parallel_status = scan_parallel(&roots, &stop, |items| {
            parallel.extend(items.into_iter().map(|item| item.path()))
        });
        serial.sort();
        parallel.sort();
        assert_eq!(serial_status, parallel_status);
        assert_eq!(serial, parallel);
        assert_eq!(parallel.len(), 6 * 43);
        stop.store(true, Ordering::Relaxed);
        assert_eq!(
            scan_parallel(&roots, &stop, |_| panic!(
                "cancelled scan published results"
            )),
            (0, String::new())
        );
    }

    #[test]
    fn modal_focus_results_enter_and_escape() {
        let ctx = egui::Context::default();
        let mut search = GlobalSearch {
            open: true,
            focus: true,
            query: "config".into(),
            results: Results {
                total: 1,
                hits: vec![Hit::new(PathBuf::from("C:/config.rs"), false)],
                ..Results::default()
            },
            ..GlobalSearch::default()
        };
        let frame = |search: &mut GlobalSearch, events| {
            let mut action = None;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 800.0),
                    )),
                    events,
                    ..Default::default()
                },
                |_| {
                    action = search.show(&ctx);
                },
            );
            (output, action)
        };
        frame(&mut search, vec![]);
        let (output, _) = frame(&mut search, vec![]);
        assert!(ctx.memory(|memory| memory.has_focus(egui::Id::new("global-search-input"))));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.job.text.contains("C:/config.rs"))));
        let key = |key| {
            vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }]
        };
        let (_, action) = frame(&mut search, key(egui::Key::Enter));
        assert!(
            matches!(action, Some(Selection::Open(hit)) if hit.path == Path::new("C:/config.rs"))
        );
        assert!(!search.open);
        search.open = true;
        frame(&mut search, vec![]);
        frame(&mut search, key(egui::Key::Escape));
        assert!(!search.open);
    }

    #[test]
    fn index_search_cache_and_native_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        fs::create_dir_all(root.join("deep")).unwrap();
        fs::write(root.join("deep/配置 CONFIG.rs"), "data").unwrap();
        fs::write(root.join("other.txt"), "data").unwrap();
        let cache = dir.path().join("index.json");
        let engine = Engine::start(
            vec![root.clone()],
            Some(cache.clone()),
            egui::Context::default(),
        );
        let wait = |check: &dyn Fn(&Index) -> bool| {
            let start = Instant::now();
            loop {
                if check(&engine.index.read().unwrap()) {
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "index did not converge"
                );
                thread::sleep(Duration::from_millis(20));
            }
        };
        wait(&|index| !index.scanning && index.items.len() == 4);
        let ticket = engine.ticket.fetch_add(1, Ordering::Relaxed) + 1;
        engine
            .tx
            .send((ticket, "DEEP 配置 config.RS".into()))
            .unwrap();
        let result = engine.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.hits[0].path, root.join("deep/配置 CONFIG.rs"));
        fs::rename(root.join("deep"), root.join("renamed")).unwrap();
        wait(&|index| {
            index
                .items
                .iter()
                .any(|item| item.path() == root.join("renamed/配置 CONFIG.rs"))
                && !index
                    .items
                    .iter()
                    .any(|item| item.path().starts_with(root.join("deep")))
        });
        fs::write(root.join("fresh.txt"), "data").unwrap();
        wait(&|index| {
            index
                .items
                .iter()
                .any(|item| item.path() == root.join("fresh.txt"))
        });
        fs::remove_file(root.join("fresh.txt")).unwrap();
        wait(&|index| {
            !index
                .items
                .iter()
                .any(|item| item.path() == root.join("fresh.txt"))
        });
        save_cache(&cache, &engine.index.read().unwrap().items).unwrap();
        let file = fs::File::open(&cache).unwrap();
        let size = file.metadata().unwrap().len();
        let loaded = Snapshot::read_cache(BufReader::new(file), size).unwrap();
        assert_eq!(loaded.len(), 4);
        assert!(
            loaded
                .iter()
                .any(|item| item.path() == root.join("renamed/配置 CONFIG.rs"))
        );
        engine.rebuild.store(true, Ordering::Relaxed);
        let revision = engine.index.read().unwrap().revision;
        wait(&|index| index.revision > revision && !index.scanning);

        let mut parents = HashMap::new();
        let items: Vec<_> = (0..100_000)
            .map(|i| {
                Item::new(
                    PathBuf::from(format!("C:/项目/Assets/file-{i}.rs")),
                    false,
                    &mut parents,
                )
            })
            .collect();
        let current = AtomicU64::new(1);
        let result = find(&items, "assets .RS", 1, &current).unwrap();
        assert_eq!(result.total, 100_000);
        assert_eq!(result.hits.len(), LIMIT);
        assert_eq!(find(&items, "FILE-99999", 1, &current).unwrap().total, 1);
        assert_eq!(
            find(&items, "项目/assets/file-99999", 1, &current)
                .unwrap()
                .total,
            1
        );
        assert!(Arc::ptr_eq(&items[0].parent, &items[99_999].parent));
        assert_eq!(find(&items, "   ", 1, &current).unwrap().total, 0);
        assert!(find(&items, "assets", 0, &current).is_none());
        println!(
            "100k in-memory paths: {:.2} ms, {} hits ({} materialized)",
            result.elapsed.as_secs_f64() * 1000.0,
            result.total,
            result.hits.len()
        );
    }
}

#[derive(Default)]
pub struct GlobalSearch {
    pub open: bool,
    focus: bool,
    query: String,
    engine: Option<Engine>,
    results: Results,
    pending: bool,
    revision: u64,
    deadline: Option<Instant>,
    selected: usize,
    count: usize,
    scanning: bool,
    skipped: usize,
    warning: String,
}

pub enum Selection {
    Open(Hit),
    Locate(Hit),
}

impl GlobalSearch {
    pub fn warm(&mut self, cache: Option<PathBuf>, ctx: &egui::Context) {
        if self.engine.is_none() {
            self.engine = Some(Engine::start(local_roots(), cache, ctx.clone()));
        }
    }

    pub fn open(&mut self, cache: Option<PathBuf>, ctx: &egui::Context) {
        self.open = true;
        self.focus = true;
        self.warm(cache, ctx);
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<Selection> {
        if !self.open {
            return None;
        }
        let mut action = None;
        if let Some(engine) = &self.engine {
            while let Ok(results) = engine.rx.try_recv() {
                if results.ticket == engine.ticket.load(Ordering::Relaxed) {
                    self.revision = results.revision;
                    self.pending = !results.complete;
                    self.results = results;
                    self.selected = self.selected.min(self.results.hits.len().saturating_sub(1));
                }
            }
            if let Ok(index) = engine.index.try_read() {
                self.count = index.remote_count.unwrap_or_else(|| index.items.len());
                self.scanning = index.scanning;
                self.skipped = index.skipped;
                self.warning.clone_from(&index.warning);
                if index.failed {
                    self.pending = false;
                    self.deadline = None;
                }
                if !index.failed && index.revision != self.revision && !self.pending {
                    self.revision = index.revision;
                    if !self.query.trim().is_empty() {
                        self.deadline.get_or_insert_with(Instant::now);
                    }
                }
            }
        }
        let response = egui::Modal::new(egui::Id::new("global-file-search")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 64.0).clamp(280.0, 940.0));
            ui.horizontal(|ui| {
                ui.heading("全电脑搜索");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("关闭  Esc").clicked() {
                        self.open = false;
                    }
                    if ui
                        .add_enabled(!self.scanning, egui::Button::new("重建索引"))
                        .clicked()
                        && let Some(engine) = &self.engine
                    {
                        engine.rebuild.store(true, Ordering::Relaxed);
                        self.scanning = true;
                    }
                });
            });
            ui.add_space(8.0);
            let input = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .id(egui::Id::new("global-search-input"))
                    .hint_text("输入文件名或路径，空格分隔多个关键词…")
                    .desired_width(f32::INFINITY),
            );
            if std::mem::take(&mut self.focus) {
                input.request_focus();
            }
            if input.changed() {
                if let Some(engine) = &self.engine {
                    engine.ticket.fetch_add(1, Ordering::Relaxed);
                }
                self.results = Results::default();
                self.pending = false;
                self.selected = 0;
                self.deadline = (!self.query.trim().is_empty()).then(Instant::now);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if self.scanning {
                    ui.spinner();
                }
                ui.weak(format!(
                    "{} · 已索引 {} 项 · 跳过 {} 项",
                    if self.scanning {
                        "正在核对 / 建立索引，结果可能不完整"
                    } else {
                        "本地磁盘 · 文件名与路径"
                    },
                    self.count,
                    self.skipped
                ));
            });
            if !self.warning.is_empty() {
                ui.colored_label(ui.visuals().warn_fg_color, "部分位置不可用（悬停查看详情）")
                    .on_hover_text(&self.warning);
            }
            ui.separator();
            let height = (ctx.content_rect().height() - 260.0).clamp(100.0, 480.0);
            if self.query.trim().is_empty() {
                ui.allocate_ui(egui::vec2(ui.available_width(), height), |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.weak("输入关键词，查找本机文件和文件夹");
                    });
                });
            } else if self.results.hits.is_empty() {
                ui.allocate_ui(egui::vec2(ui.available_width(), height), |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.weak(if self.pending || self.deadline.is_some() {
                            "正在搜索…"
                        } else if self.scanning {
                            "已索引部分暂无匹配，索引仍在继续…"
                        } else {
                            "没有匹配的文件或文件夹"
                        });
                    });
                });
            } else {
                let up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
                let down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
                if up {
                    self.selected = self.selected.saturating_sub(1);
                }
                if down {
                    self.selected = (self.selected + 1).min(self.results.hits.len() - 1);
                }
                ui.scope(|ui| {
                    ui.set_min_height(height);
                    let mut scroll = egui::ScrollArea::vertical()
                        .id_salt("global-results")
                        .max_height(height)
                        .min_scrolled_height(height);
                    if up || down {
                        scroll = scroll.vertical_scroll_offset(
                            (self.selected as f32 * (46.0 + ui.spacing().item_spacing.y)
                                - height / 2.0)
                                .max(0.0),
                        );
                    }
                    scroll.show_rows(ui, 46.0, self.results.hits.len(), |ui, rows| {
                        for row in rows {
                            let hit = &self.results.hits[row];
                            let title = &hit.title;
                            let path = &hit.display;
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 46.0),
                                egui::Sense::click(),
                            );
                            if self.selected == row || response.hovered() {
                                ui.painter().rect_filled(
                                    rect,
                                    3.0,
                                    if self.selected == row {
                                        ui.visuals().selection.bg_fill
                                    } else {
                                        ui.visuals().widgets.hovered.weak_bg_fill
                                    },
                                );
                            }
                            response.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::SelectableLabel,
                                    ui.is_enabled(),
                                    self.selected == row,
                                    format!("{title} {path}"),
                                )
                            });
                            let mut labels = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(rect.shrink2(egui::vec2(8.0, 4.0)))
                                    .layout(egui::Layout::top_down(egui::Align::Min)),
                            );
                            labels.spacing_mut().item_spacing.y = 2.0;
                            labels.add(
                                egui::Label::new(egui::RichText::new(title).strong()).truncate(),
                            );
                            labels.add(
                                egui::Label::new(egui::RichText::new(path).small().weak())
                                    .truncate(),
                            );
                            let response = response.on_hover_text(path);
                            if response.clicked() {
                                self.selected = row;
                            }
                            if response.double_clicked() {
                                action = Some(Selection::Open(hit.clone()));
                            }
                            response.context_menu(|ui| {
                                if ui.button("打开").clicked() {
                                    action = Some(Selection::Open(hit.clone()));
                                    ui.close();
                                }
                                if ui.button("在面板中定位").clicked() {
                                    action = Some(Selection::Locate(hit.clone()));
                                    ui.close();
                                }
                                if ui.button("复制路径").clicked() {
                                    ui.ctx().copy_text(crate::model::display_path(&hit.path));
                                    ui.close();
                                }
                            });
                        }
                    });
                });
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    action = Some(Selection::Open(self.results.hits[self.selected].clone()));
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.weak(format!(
                    "{}{} 个匹配 · 显示前 {} 项 · {:.1} ms{}",
                    if self.pending { "至少 " } else { "" },
                    self.results.total,
                    self.results.hits.len(),
                    self.results.elapsed.as_secs_f64() * 1000.0,
                    if self.pending {
                        " · 总数统计中"
                    } else {
                        ""
                    }
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !self.results.hits.is_empty(),
                            egui::Button::new("在面板中定位"),
                        )
                        .clicked()
                    {
                        action = Some(Selection::Locate(self.results.hits[self.selected].clone()));
                    }
                });
            });
        });
        if response.should_close() || action.is_some() {
            self.open = false;
        }
        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                if let Some(engine) = &self.engine {
                    let ticket = engine.ticket.fetch_add(1, Ordering::Relaxed) + 1;
                    self.pending = engine.tx.send((ticket, self.query.clone())).is_ok();
                }
                self.deadline = None;
            } else {
                ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
            }
        }
        action
    }
}

#[cfg(test)]
pub(crate) fn verify_remote_client(client: &Arc<crate::backend::Client>, root: &Path) {
    let cache = root.join("remote-index.bin");
    let stream = client
        .stream(crate::backend::Command::Global {
            roots: vec![root.to_owned()],
            cache: Some(cache.clone()),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "remote indexing timeout");
        if let Some((
            RemoteEvent::Status {
                scanning: false,
                count,
                ..
            },
            _,
        )) = stream.recv_timeout(Duration::from_secs(1)).unwrap()
        {
            assert!(count >= 1);
            break;
        }
    }
    client.send(crate::backend::Command::Query {
        session: stream.id,
        ticket: 42,
        query: "renamed".into(),
    });
    loop {
        assert!(Instant::now() < deadline, "remote query timeout");
        if let Some((RemoteEvent::Result(result), _)) =
            stream.recv_timeout(Duration::from_secs(1)).unwrap()
            && result.ticket == 42
            && result.complete
        {
            assert_eq!(result.total, 1);
            assert_eq!(result.hits[0].path, root.join("renamed.txt"));
            break;
        }
    }
    while !cache.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(cache.exists(), "cache persistence failed");
    let file = fs::File::open(cache).unwrap();
    let len = file.metadata().unwrap().len();
    assert!(
        !Snapshot::read_cache(BufReader::new(file), len)
            .unwrap()
            .is_empty()
    );
}
