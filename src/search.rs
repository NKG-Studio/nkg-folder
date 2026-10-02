use crate::model::display_path;
use crate::{files::Entry, model::Location};
use eframe::egui;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(serde::Serialize, serde::Deserialize)]
enum Update {
    End,
    Batch(Vec<(Entry, Option<crate::files::FileIdentity>)>, usize),
    Changed,
    WatchError(String),
    Done {
        scanned: usize,
        skipped: usize,
        error: Option<String>,
        limited: bool,
    },
}

pub struct Search {
    pub identities: HashMap<PathBuf, Option<crate::files::FileIdentity>>,
    pub changed: bool,
    pub watch_error: Option<String>,
    pub location: Location,
    pub query: String,
    pub scanned: usize,
    pub skipped: usize,
    pub error: Option<String>,
    pub done: bool,
    pub limited: bool,
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Update>,
}

impl Search {
    pub fn start(
        location: Location,
        roots: Vec<PathBuf>,
        query: String,
        ctx: egui::Context,
    ) -> Self {
        if let Some(client) = crate::backend::client() {
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = cancel.clone();
            let client = client.clone();
            let (sender, receiver) = mpsc::sync_channel(4);
            let request = crate::backend::Command::Search {
                roots,
                query: query.clone(),
            };
            thread::spawn(move || {
                let result = (|| -> Result<(), String> {
                    let stream = client.stream(request)?;
                    while !stop.load(Ordering::Relaxed) {
                        if let Some((update, done)) =
                            stream.recv_timeout::<Update>(Duration::from_millis(100))?
                        {
                            if sender.send(update).is_err() || done {
                                break;
                            }
                            ctx.request_repaint();
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = sender.send(Update::Done {
                        scanned: 0,
                        skipped: 0,
                        error: Some(error),
                        limited: false,
                    });
                    ctx.request_repaint();
                }
            });
            return Self {
                identities: HashMap::new(),
                changed: false,
                watch_error: None,
                location,
                query,
                scanned: 0,
                skipped: 0,
                error: None,
                done: false,
                limited: false,
                cancel,
                receiver,
            };
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let (sender, receiver) = mpsc::sync_channel(4);
        let needle = query.to_lowercase();
        thread::spawn(move || {
            // Install notifications before scanning so changes during the scan are not lost.
            let mut watches = Vec::new();
            let mut watched = HashSet::new();
            for root in &roots {
                let directory = if root.is_dir() {
                    root.as_path()
                } else {
                    root.parent().unwrap_or(root)
                };
                // Watch the parent non-recursively too: deleting/renaming the root itself
                // does not signal a notification installed on that root.
                for (path, recursive) in [(Some(directory), true), (directory.parent(), false)] {
                    if let Some(path) =
                        path.filter(|p| watched.insert((p.to_path_buf(), recursive)))
                    {
                        match DirectoryWatch::new(path, recursive) {
                            Ok(watch) => watches.push(watch),
                            Err(error) => {
                                let _ = sender.send(Update::WatchError(error));
                            }
                        }
                    }
                }
            }
            let mut stack: Vec<(PathBuf, Option<fs::FileType>)> =
                roots.into_iter().map(|path| (path, None)).collect();
            let mut seen = HashSet::new();
            let mut found = HashSet::new();
            let mut batch = Vec::new();
            let mut scanned = 0;
            let mut skipped = 0;
            let mut error = None;
            let mut matches = 0;
            let mut limited = false;
            let mut last_update = Instant::now();
            while let Some((path, cached_type)) = stack.pop() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let file_type = match cached_type
                    .map(Ok)
                    .unwrap_or_else(|| fs::symlink_metadata(&path).map(|m| m.file_type()))
                {
                    Ok(file_type) => file_type,
                    Err(e) => {
                        skipped += 1;
                        if error.is_none() {
                            error = Some(format!("{}：{e}", display_path(&path)));
                        }
                        continue;
                    }
                };
                let canonical = match if file_type.is_symlink() {
                    std::path::absolute(&path)
                } else if cached_type.is_none() || file_type.is_dir() {
                    fs::canonicalize(&path)
                } else {
                    Ok(path.clone())
                } {
                    Ok(path) => path,
                    Err(e) => {
                        skipped += 1;
                        if error.is_none() {
                            error = Some(format!("{}：{e}", display_path(&path)));
                        }
                        continue;
                    }
                };
                if file_type.is_dir() && !seen.insert(canonical.clone()) {
                    continue;
                }
                scanned += 1;
                let name = crate::model::path_name(&path);
                if name.to_lowercase().contains(&needle) && found.insert(canonical.clone()) {
                    let identity = crate::files::file_identity(&canonical).ok();
                    batch.push((
                        Entry {
                            modified: identity.as_ref().map(|i| i.modified),
                            path: canonical.clone(),
                            name,
                            directory: file_type.is_dir(),
                            link: file_type.is_symlink(),
                        },
                        identity,
                    ));
                    matches += 1;
                    // ponytail: cap materialized hits at 100k; add paged result storage if needed.
                    if matches == 100_000 {
                        limited = true;
                    }
                }
                let mut traverse = file_type.is_dir() && !file_type.is_symlink();
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if traverse {
                        traverse = fs::symlink_metadata(&path)
                            .is_ok_and(|m| m.file_attributes() & 0x400 == 0);
                    }
                }
                if traverse && !limited {
                    match fs::read_dir(&canonical) {
                        Ok(entries) => {
                            for entry in entries {
                                if stop.load(Ordering::Relaxed) {
                                    break;
                                }
                                match entry {
                                    Ok(entry) => match entry.file_type() {
                                        Ok(file_type) => {
                                            stack.push((entry.path(), Some(file_type)))
                                        }
                                        Err(e) => {
                                            skipped += 1;
                                            if error.is_none() {
                                                error = Some(e.to_string());
                                            }
                                        }
                                    },
                                    Err(e) => {
                                        skipped += 1;
                                        if error.is_none() {
                                            error = Some(e.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            skipped += 1;
                            if error.is_none() {
                                error = Some(format!("{}：{e}", display_path(&path)));
                            }
                        }
                    }
                }
                if batch.len() >= 256
                    || last_update.elapsed() >= Duration::from_millis(100)
                    || limited
                {
                    if sender
                        .send(Update::Batch(std::mem::take(&mut batch), scanned))
                        .is_err()
                    {
                        return;
                    }
                    ctx.request_repaint();
                    last_update = Instant::now();
                }
                if limited {
                    break;
                }
            }
            if !batch.is_empty() && sender.send(Update::Batch(batch, scanned)).is_err() {
                return;
            }
            let _ = sender.send(Update::Done {
                scanned,
                skipped,
                error,
                limited,
            });
            drop(stack);
            drop(seen);
            drop(found);
            ctx.request_repaint();
            let mut changed_at = None;
            let mut first_change = None;
            while !watches.is_empty() && !stop.load(Ordering::Relaxed) {
                for watch in &mut watches {
                    match watch.changed() {
                        Ok(true) => {
                            changed_at = Some(Instant::now());
                            first_change.get_or_insert_with(Instant::now);
                        }
                        Ok(false) => {}
                        Err(error) => {
                            let _ = sender.send(Update::WatchError(error));
                            let _ = sender.send(Update::Changed);
                            ctx.request_repaint();
                            return;
                        }
                    }
                }
                // ponytail: coalesce events then rescan; use incremental results if large,
                // frequently changing trees make these event-driven scans expensive.
                if changed_at.is_some_and(|t| t.elapsed() >= Duration::from_millis(300))
                    || first_change.is_some_and(|t| t.elapsed() >= Duration::from_secs(2))
                {
                    let _ = sender.send(Update::Changed);
                    ctx.request_repaint();
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
        Self {
            identities: HashMap::new(),
            changed: false,
            watch_error: None,
            location,
            query,
            scanned: 0,
            skipped: 0,
            error: None,
            done: false,
            limited: false,
            cancel,
            receiver,
        }
    }

    pub fn poll(&mut self) -> Vec<Entry> {
        let mut entries = Vec::new();
        while let Ok(update) = self.receiver.try_recv() {
            match update {
                Update::Batch(batch, scanned) => {
                    for (entry, identity) in batch {
                        self.identities.insert(entry.path.clone(), identity);
                        entries.push(entry);
                    }
                    self.scanned = scanned;
                }
                Update::End => {}
                Update::Changed => self.changed = true,
                Update::WatchError(error) => self.watch_error = Some(error),
                Update::Done {
                    scanned,
                    skipped,
                    error,
                    limited,
                } => {
                    self.scanned = scanned;
                    self.skipped = skipped;
                    self.error = error;
                    self.limited = limited;
                    self.done = true;
                }
            }
        }
        entries
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub(crate) fn serve_remote(roots: Vec<PathBuf>, query: String, sink: &crate::backend::Sink) {
    let search = Search::start(
        Location::Disk(roots.first().cloned().unwrap_or_default()),
        roots,
        query,
        egui::Context::default(),
    );
    while !sink.stop.load(Ordering::Relaxed) {
        match search.receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(update) => {
                if !sink.emit(&update, false) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                sink.emit(&Update::End, true);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(windows)]
struct DirectoryWatch(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl DirectoryWatch {
    fn new(path: &std::path::Path, recursive: bool) -> Result<Self, String> {
        use std::os::windows::ffi::OsStrExt;
        use windows::{Win32::Storage::FileSystem::*, core::PCWSTR};
        let path = fs::canonicalize(path).map_err(|e| e.to_string())?;
        let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            FindFirstChangeNotificationW(
                PCWSTR(wide.as_ptr()),
                recursive,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES
                    | FILE_NOTIFY_CHANGE_SECURITY,
            )
            .map(Self)
            .map_err(|e| e.to_string())
        }
    }

    fn changed(&mut self) -> Result<bool, String> {
        use windows::Win32::{
            Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
            Storage::FileSystem::FindNextChangeNotification,
            System::Threading::WaitForSingleObject,
        };
        unsafe {
            match WaitForSingleObject(self.0, 0) {
                WAIT_OBJECT_0 => {
                    FindNextChangeNotification(self.0).map_err(|e| e.to_string())?;
                    Ok(true)
                }
                WAIT_TIMEOUT => Ok(false),
                _ => Err(std::io::Error::last_os_error().to_string()),
            }
        }
    }
}

#[cfg(windows)]
impl Drop for DirectoryWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Storage::FileSystem::FindCloseChangeNotification(self.0);
        }
    }
}

#[cfg(not(windows))]
struct DirectoryWatch;
#[cfg(not(windows))]
impl DirectoryWatch {
    fn new(_: &std::path::Path, _: bool) -> Result<Self, String> {
        Err("自动刷新仅支持 Windows".into())
    }
    fn changed(&mut self) -> Result<bool, String> {
        Ok(false)
    }
}

#[cfg(test)]
pub(crate) fn verify_remote_client(client: &Arc<crate::backend::Client>, root: &std::path::Path) {
    let stream = client
        .stream(crate::backend::Command::Search {
            roots: vec![root.to_owned()],
            query: "renamed".into(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut found = false;
    loop {
        assert!(Instant::now() < deadline, "remote recursive search timeout");
        if let Some((update, _)) = stream
            .recv_timeout::<Update>(Duration::from_secs(1))
            .unwrap()
        {
            match update {
                Update::Batch(batch, _) => {
                    found |= batch
                        .iter()
                        .any(|(e, id)| e.path == root.join("renamed.txt") && id.is_some());
                }
                Update::Done { error, .. } => {
                    assert!(error.is_none());
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(found);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recursive_search_finds_nested_files_deduplicates_and_reports_errors() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("deep/nested")).unwrap();
        fs::write(root.path().join("deep/nested/CONFIG.json"), "data").unwrap();
        fs::write(root.path().join("other.txt"), "data").unwrap();
        let mut search = Search::start(
            Location::Disk(root.path().into()),
            vec![
                root.path().into(),
                root.path().join("deep"),
                root.path().join("missing"),
            ],
            "config".into(),
            egui::Context::default(),
        );
        let start = Instant::now();
        let mut found = Vec::new();
        while !search.done && start.elapsed() < Duration::from_secs(5) {
            found.extend(search.poll());
            thread::sleep(Duration::from_millis(5));
        }
        assert!(search.done);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "CONFIG.json");
        assert_eq!(search.skipped, 1);
        assert!(search.error.is_some());
    }
}
