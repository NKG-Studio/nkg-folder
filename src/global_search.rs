//! Local filename/path index. Disk I/O and matching never run on the UI thread.
use eframe::egui;
use notify::EventKind;
#[cfg(not(windows))]
use notify::{RecursiveMode, Watcher};
use serde::Serialize;
#[cfg(test)]
use std::collections::HashMap;
use std::{
    ffi::OsString,
    fs,
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
mod database;
#[cfg(windows)]
mod ntfs;
mod scanner;
#[cfg(windows)]
mod watch;
use database::{Reader, Writer};

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
struct Checkpoint {
    root: PathBuf,
    volume: u32,
    journal: u64,
    next: i64,
}

#[derive(Clone)]
struct Item {
    path: PathBuf,
    directory: bool,
}

impl Item {
    fn new(path: PathBuf, directory: bool) -> Self {
        Self { path, directory }
    }
    #[cfg(test)]
    fn path(&self) -> PathBuf {
        self.path.clone()
    }
}

#[derive(Default)]
struct Index {
    count: usize,
    database: Option<Arc<database::Location>>,
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

#[derive(Default, Clone, Serialize, serde::Deserialize)]
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
        let roots = scanner::minimal_roots(&roots);
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
        let rebuild = engine.rebuild.clone();
        let current = engine.ticket.clone();
        thread::spawn(move || {
            let run = || -> Result<(), String> {
                let mut writer = Writer::open(cache, stop.clone())?;
                let state = writer.state()?;
                {
                    let mut index = data.write().unwrap();
                    index.count = state.count;
                    index.revision = state.revision;
                    index.database = Some(writer.location.clone());
                }
                let mut reader = Reader::open(writer.location.clone())?;
                let (query_stop, query_data, repaint) = (stop.clone(), data.clone(), ctx.clone());
                thread::spawn(move || {
                    while !query_stop.load(Ordering::Relaxed) {
                        let Ok(mut query) = queries.recv_timeout(Duration::from_millis(200)) else {
                            continue;
                        };
                        while let Ok(newer) = queries.try_recv() {
                            query = newer;
                        }
                        if current.load(Ordering::Relaxed) != query.0 {
                            continue;
                        }
                        if let Err(error) = reader.find(
                            &query.1,
                            query.0,
                            current.clone(),
                            query_stop.clone(),
                            |result| {
                                let _ = results.send(result);
                                repaint.request_repaint();
                            },
                        ) {
                            fail_index(&query_data, &query_stop, &repaint, error);
                            break;
                        }
                    }
                });
                maintain(
                    data.clone(),
                    roots,
                    &mut writer,
                    stop.clone(),
                    rebuild,
                    ctx.clone(),
                )
            };
            if let Err(error) = run() {
                fail_index(&data, &stop, &ctx, error);
            }
        });
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
        failed: bool,
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
                    count: index.count,
                    revision: index.revision,
                    scanning: index.scanning,
                    skipped: index.skipped,
                    failed: index.failed,
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
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let index = engine.index.read().unwrap();
                if index.failed {
                    sink.error(index.warning.clone());
                }
                break;
            }
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
                            failed,
                            warning,
                        } => {
                            let mut index = index.write().unwrap();
                            index.count = count;
                            index.failed = failed;
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
fn scan(roots: &[PathBuf], stop: &AtomicBool, mut batch: impl FnMut(Vec<Item>)) -> (usize, String) {
    let mut pending: Vec<_> = roots.iter().cloned().map(|path| (path, None)).collect();
    let mut items = Vec::new();
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
        items.push(Item::new(path.clone(), directory));
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

fn fail_index(data: &RwLock<Index>, stop: &AtomicBool, ctx: &egui::Context, error: String) {
    let mut index = data.write().unwrap();
    index.failed = true;
    index.scanning = false;
    index.warning = format!("搜索数据库不可用：{error}");
    stop.store(true, Ordering::Relaxed);
    ctx.request_repaint();
}

fn publish(data: &RwLock<Index>, ctx: &egui::Context, state: database::State) {
    let mut index = data.write().unwrap();
    index.count = state.count;
    index.revision = state.revision;
    ctx.request_repaint();
}

fn maintain(
    data: Arc<RwLock<Index>>,
    roots: Vec<PathBuf>,
    writer: &mut Writer,
    stop: Arc<AtomicBool>,
    rebuild: Arc<AtomicBool>,
    ctx: egui::Context,
) -> Result<(), String> {
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
    let initial = writer.state()?;
    let mut checkpoints: Vec<Checkpoint> =
        serde_json::from_str(&initial.checkpoints).map_err(|e| e.to_string())?;
    let mut full = initial.dirty || initial.roots != database::roots_key(&roots);
    let mut startup = true;
    let mut last_check = Instant::now();
    data.write().unwrap().warning.clone_from(&watch_warning);
    while !stop.load(Ordering::Relaxed) {
        let requested = rebuild.swap(false, Ordering::Relaxed);
        let mut changed = Vec::new();
        let mut next_checkpoints = checkpoints.clone();
        let mut replayed = false;
        if startup || full || requested || last_check.elapsed() >= Duration::from_secs(30) {
            next_checkpoints.clear();
            for root in &roots {
                #[cfg(windows)]
                if !full
                    && !requested
                    && let Some(old) = checkpoints.iter().find(|c| &c.root == root)
                    && let Some((paths, checkpoint)) = ntfs::changes(old, &stop)
                {
                    changed.extend(paths);
                    next_checkpoints.push(checkpoint);
                    continue;
                }
                if startup || full || requested || checkpoints.iter().any(|c| &c.root == root) {
                    changed.push(root.clone());
                }
                #[cfg(windows)]
                if let Some(checkpoint) = ntfs::checkpoint(root) {
                    next_checkpoints.push(checkpoint);
                }
            }
            last_check = Instant::now();
            replayed = true;
        }
        if !startup && !full && !requested {
            let until = Instant::now() + Duration::from_millis(40);
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                match receiver.recv_timeout(Duration::from_millis(40)) {
                    Ok(Ok(event)) => {
                        if event.need_rescan() {
                            full = true;
                        }
                        let renamed = matches!(
                            event.kind,
                            EventKind::Modify(notify::event::ModifyKind::Name(_))
                        );
                        for path in event.paths {
                            if !roots.iter().any(|root| path.starts_with(root))
                                || cache_file(&path, Some(&writer.location.path))
                            {
                                continue;
                            }
                            // A case-only rename leaves the old spelling accessible on Windows.
                            // Enumerate its parent to recover the real names (also covers rapid reuse).
                            if renamed
                                && path.try_exists().unwrap_or(true)
                                && let Some(parent) = path.parent()
                                && roots.iter().any(|root| parent.starts_with(root))
                            {
                                changed.push(parent.to_owned());
                            } else {
                                changed.push(path);
                            }
                        }
                        if changed.len() > 65_536 {
                            full = true;
                            break;
                        }
                    }
                    Ok(Err(error)) => {
                        watch_warning =
                            format!("目录通知失败，已安排核对；后续请手动重建：{error}");
                        data.write().unwrap().warning.clone_from(&watch_warning);
                        full = true;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        thread::sleep(Duration::from_millis(40))
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            if full {
                continue;
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let changed = scanner::minimal_roots(&changed);
        if !changed.is_empty() || full || requested {
            data.write().unwrap().scanning = true;
            ctx.request_repaint();
            let generation = writer.begin_scan()?;
            let cache = writer.location.path.clone();
            let mut failure = None;
            let (skipped, warning) = scan_parallel(&changed, &stop, |mut batch| {
                batch.retain(|item| !cache_file(&item.path, Some(&cache)));
                if failure.is_some() {
                    return;
                }
                match writer.insert(batch, generation) {
                    Ok(state) => publish(&data, &ctx, state),
                    Err(error) => {
                        failure = Some(error);
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            });
            if let Some(error) = failure {
                return Err(error);
            }
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            writer.prune(&changed, generation, full || requested, |state| {
                publish(&data, &ctx, state)
            })?;
            publish(&data, &ctx, writer.finish(&roots, &next_checkpoints)?);
            let mut index = data.write().unwrap();
            index.scanning = false;
            index.skipped = skipped;
            if !warning.is_empty() {
                index.warning = warning;
            }
            ctx.request_repaint();
        } else if replayed {
            publish(&data, &ctx, writer.finish(&roots, &next_checkpoints)?);
            data.write().unwrap().scanning = false;
        }
        checkpoints = next_checkpoints;
        startup = false;
        full = false;
    }
    Ok(())
}

fn cache_file(path: &Path, cache: Option<&Path>) -> bool {
    cache.is_some_and(|cache| {
        let parent = cache.parent();
        let temporary = parent
            .and_then(Path::file_name)
            .is_some_and(|name| name.to_string_lossy().starts_with(".nkg-index-"));
        if temporary && parent.is_some_and(|parent| path.starts_with(parent)) {
            return true;
        }
        path.parent() == parent
            && path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                let database = cache.file_name().unwrap_or_default().to_string_lossy();
                name == database
                    || name.starts_with(&format!("{database}-"))
                    || matches!(name.as_ref(), "file-index.bin" | "file-index.json")
                    || name.starts_with(".nkg-index-")
            })
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

    fn wait(engine: &Engine, check: impl Fn(&Index) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let index = engine.index.read().unwrap();
            assert!(!index.failed, "{}", index.warning);
            if check(&index) {
                return;
            }
            drop(index);
            assert!(Instant::now() < deadline, "index did not converge");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn paths(engine: &Engine) -> Vec<PathBuf> {
        let location = engine.index.read().unwrap().database.clone().unwrap();
        let mut paths = Reader::open(location).unwrap().paths();
        paths.sort();
        paths
    }

    fn shutdown(engine: Engine) {
        engine.stop.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&engine.index) > 1 {
            assert!(Instant::now() < deadline, "index workers did not stop");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn sqlite_live_changes_rebuild_restart_and_scope() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        fs::create_dir_all(root.join("old/deep")).unwrap();
        fs::write(root.join("old/deep/配置 CONFIG.rs"), "data").unwrap();
        let cache = temp.path().join("index.sqlite");
        let engine = Engine::start(
            vec![root.clone()],
            Some(cache.clone()),
            egui::Context::default(),
        );
        wait(&engine, |i| !i.scanning && i.count == 4);
        let ticket = engine.ticket.fetch_add(1, Ordering::Relaxed) + 1;
        engine
            .tx
            .send((ticket, "DEEP 配置 config.RS".into()))
            .unwrap();
        let result = engine.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.hits[0].path, root.join("old/deep/配置 CONFIG.rs"));
        fs::rename(root.join("old"), root.join("OLD")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let actual = paths(&engine);
            if actual.contains(&root.join("OLD/deep/配置 CONFIG.rs"))
                && !actual.contains(&root.join("old/deep/配置 CONFIG.rs"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "case-only rename did not converge"
            );
            thread::sleep(Duration::from_millis(20));
        }
        for i in 0..1200 {
            fs::write(root.join(format!("OLD/deep/file-{i}.txt")), "data").unwrap();
        }
        fs::hard_link(root.join("OLD/deep/file-0.txt"), root.join("alias.txt")).unwrap();
        fs::rename(root.join("OLD"), root.join("renamed")).unwrap();
        for i in (0..1200).step_by(3) {
            fs::remove_file(root.join(format!("renamed/deep/file-{i}.txt"))).unwrap();
        }
        let mut expected = Vec::new();
        scan(
            std::slice::from_ref(&root),
            &AtomicBool::new(false),
            |batch| expected.extend(batch.into_iter().map(|i| i.path)),
        );
        expected.sort();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if paths(&engine) == expected && !engine.index.read().unwrap().scanning {
                break;
            }
            assert!(Instant::now() < deadline, "notifications did not converge");
            thread::sleep(Duration::from_millis(40));
        }
        let revision = engine.index.read().unwrap().revision;
        engine.rebuild.store(true, Ordering::Relaxed);
        wait(&engine, |i| i.revision > revision && !i.scanning);
        assert_eq!(paths(&engine), expected);
        shutdown(engine);
        fs::write(root.join("offline.txt"), "new while stopped").unwrap();
        expected.push(root.join("offline.txt"));
        expected.sort();
        let restarted = Engine::start(
            vec![root.clone()],
            Some(cache.clone()),
            egui::Context::default(),
        );
        wait(&restarted, |i| !i.scanning);
        assert_eq!(paths(&restarted), expected);
        shutdown(restarted);
        let narrowed = root.join("renamed");
        let engine = Engine::start(
            vec![narrowed.clone()],
            Some(cache),
            egui::Context::default(),
        );
        wait(&engine, |i| !i.scanning);
        expected.retain(|p| p.starts_with(&narrowed));
        assert_eq!(paths(&engine), expected);
        shutdown(engine);
    }

    #[test]
    fn sqlite_cache_and_sidecars_are_not_indexed() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("file-index.bin"),
            "old cache must not be loaded",
        )
        .unwrap();
        fs::write(temp.path().join("keep.txt"), "data").unwrap();
        let cache = temp.path().join("file-index.sqlite");
        let engine = Engine::start(
            vec![temp.path().to_owned()],
            Some(cache.clone()),
            egui::Context::default(),
        );
        wait(&engine, |i| !i.scanning);
        assert_eq!(
            paths(&engine),
            vec![temp.path().to_owned(), temp.path().join("keep.txt")]
        );
        for suffix in ["", "-wal", "-shm", "-journal"] {
            assert!(cache_file(
                &temp.path().join(format!("file-index.sqlite{suffix}")),
                Some(&cache)
            ));
        }
        shutdown(engine);
        assert_eq!(
            fs::read_to_string(temp.path().join("file-index.bin")).unwrap(),
            "old cache must not be loaded"
        );
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
                self.count = index.count;
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
    let cache = root.join("remote-index.sqlite");
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
    let location = Arc::new(database::Location::persistent(cache));
    assert!(!Reader::open(location).unwrap().paths().is_empty());
}
