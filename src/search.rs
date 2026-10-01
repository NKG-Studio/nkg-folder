use crate::model::display_path;
use crate::{files::Entry, model::Location};
use eframe::egui;
use std::{
    collections::HashSet,
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

enum Update {
    Batch(Vec<Entry>, usize),
    Done {
        scanned: usize,
        skipped: usize,
        error: Option<String>,
        limited: bool,
    },
}

pub struct Search {
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
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let (sender, receiver) = mpsc::sync_channel(4);
        let needle = query.to_lowercase();
        thread::spawn(move || {
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
                    batch.push(Entry {
                        path: canonical.clone(),
                        name,
                        directory: file_type.is_dir(),
                        link: file_type.is_symlink(),
                    });
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
            ctx.request_repaint();
        });
        Self {
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
                Update::Batch(mut batch, scanned) => {
                    entries.append(&mut batch);
                    self.scanned = scanned;
                }
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

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel();
    }
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
