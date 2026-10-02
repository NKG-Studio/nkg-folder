use super::*;
use std::{
    collections::{HashSet, VecDeque},
    sync::Condvar,
};

struct Task {
    root: usize,
    path: PathBuf,
    metadata: Option<fs::Metadata>,
}
struct Queue {
    pending: Vec<VecDeque<Task>>,
    next: usize,
    seen: HashSet<PathBuf>,
    active: Vec<usize>,
}

pub(super) fn run(
    roots: &[PathBuf],
    stop: &AtomicBool,
    mut batch: impl FnMut(Vec<Item>) + Send,
) -> (usize, String) {
    if stop.load(Ordering::Relaxed) || roots.is_empty() {
        return (0, String::new());
    }
    let mut queue = Queue {
        pending: Vec::new(),
        next: 0,
        seen: HashSet::new(),
        active: Vec::new(),
    };
    let mut budgets = Vec::new();
    let mut volumes = HashMap::new();
    for path in roots {
        let volume = path.ancestors().last().unwrap_or(path).to_owned();
        let root = *volumes.entry(volume.clone()).or_insert_with(|| {
            #[cfg(windows)]
            let budget = ntfs::concurrency(&volume);
            #[cfg(not(windows))]
            let budget = 1;
            let root = budgets.len();
            budgets.push(budget);
            queue.active.push(0);
            queue.pending.push(VecDeque::new());
            root
        });
        // Start reading directories immediately; a full MFT prepass delays every volume
        // and then repeats directory I/O anyway (needed to preserve all hard-link names).
        if queue.seen.insert(path.clone()) {
            queue.pending[root].push_back(Task {
                root,
                path: path.clone(),
                metadata: None,
            });
        }
    }
    let queue = Mutex::new(queue);
    let changed = Condvar::new();
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<Item>>(8);
    let workers = budgets.iter().sum::<usize>().min(4);
    thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let sender = sender.clone();
                let queue = &queue;
                let changed = &changed;
                let budgets = &budgets;
                scope.spawn(move || {
                    let mut parents = HashMap::new();
                    let mut items = Vec::with_capacity(4096);
                    let mut last_flush = Instant::now();
                    let mut skipped = 0;
                    let mut warning = String::new();
                    let mut error = |path: &Path, error: std::io::Error| {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            skipped += 1;
                            if warning.is_empty() {
                                warning = format!("{}：{error}", path.display());
                            }
                        }
                    };
                    loop {
                        let task = {
                            let mut state = queue.lock().unwrap();
                            loop {
                                if stop.load(Ordering::Relaxed) {
                                    return (skipped, warning);
                                }
                                let ready = (0..state.pending.len())
                                    .map(|n| (state.next + n) % state.pending.len())
                                    .find(|&root| {
                                        !state.pending[root].is_empty()
                                            && state.active[root] < budgets[root]
                                    });
                                if let Some(root) = ready {
                                    state.next = (root + 1) % state.pending.len();
                                    let task = state.pending[root].pop_front().unwrap();
                                    state.active[root] += 1;
                                    break task;
                                }
                                if state.pending.iter().all(VecDeque::is_empty)
                                    && state.active.iter().all(|n| *n == 0)
                                {
                                    drop(state);
                                    if !items.is_empty() {
                                        let _ = sender.send(std::mem::take(&mut items));
                                    }
                                    return (skipped, warning);
                                }
                                state = changed
                                    .wait_timeout(state, Duration::from_millis(100))
                                    .unwrap()
                                    .0;
                            }
                        };
                        let mut children = Vec::new();
                        match task
                            .metadata
                            .map(Ok)
                            .unwrap_or_else(|| fs::symlink_metadata(&task.path))
                        {
                            Err(e) => error(&task.path, e),
                            Ok(metadata) => {
                                items.push(Item::new(
                                    task.path.clone(),
                                    metadata.is_dir(),
                                    &mut parents,
                                ));
                                if traversable(&metadata) {
                                    match fs::read_dir(&task.path) {
                                        Err(e) => error(&task.path, e),
                                        Ok(entries) => {
                                            for entry in entries {
                                                if stop.load(Ordering::Relaxed) {
                                                    break;
                                                }
                                                match entry {
                                                    Err(e) => error(&task.path, e),
                                                    Ok(entry) => match entry.metadata() {
                                                        Err(e) => error(&entry.path(), e),
                                                        Ok(metadata) => {
                                                            if traversable(&metadata) {
                                                                children.push(Task {
                                                                    root: task.root,
                                                                    path: entry.path(),
                                                                    metadata: Some(metadata),
                                                                });
                                                            } else {
                                                                items.push(Item::new(
                                                                    entry.path(),
                                                                    metadata.is_dir(),
                                                                    &mut parents,
                                                                ));
                                                            }
                                                        }
                                                    },
                                                }
                                                if items.len() >= 4096
                                                    && sender
                                                        .send(std::mem::take(&mut items))
                                                        .is_err()
                                                {
                                                    return (skipped, warning);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if !items.is_empty() && last_flush.elapsed() >= Duration::from_millis(100) {
                            if sender.send(std::mem::take(&mut items)).is_err() {
                                return (skipped, warning);
                            }
                            last_flush = Instant::now();
                        }
                        let mut state = queue.lock().unwrap();
                        for task in children {
                            if state.seen.insert(task.path.clone()) {
                                state.pending[task.root].push_back(task);
                            }
                        }
                        state.active[task.root] -= 1;
                        changed.notify_all();
                    }
                })
            })
            .collect();
        drop(sender);
        while let Ok(items) = receiver.recv() {
            batch(items);
        }
        let mut skipped = 0;
        let mut warning = String::new();
        for handle in handles {
            let (count, error) = handle.join().expect("directory scanner panicked");
            skipped += count;
            if warning.is_empty() {
                warning = error;
            }
        }
        (skipped, warning)
    })
}

fn traversable(metadata: &fs::Metadata) -> bool {
    let mut result = metadata.is_dir() && !metadata.file_type().is_symlink();
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        result &= metadata.file_attributes() & 0x400 == 0;
    }
    result
}
