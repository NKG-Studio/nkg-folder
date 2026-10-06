//! Bounded depth-first enumeration: no all-directory queue or full-drive parent cache.
use super::*;

pub(super) fn minimal_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = roots.iter().map(|p| p.components().collect()).collect();
    roots.sort();
    roots.dedup();
    let mut minimal: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !minimal
            .last()
            .is_some_and(|parent| root.starts_with(parent))
        {
            minimal.push(root);
        }
    }
    minimal
}

pub(super) fn run(
    roots: &[PathBuf],
    stop: &AtomicBool,
    mut batch: impl FnMut(Vec<Item>) + Send,
) -> (usize, String) {
    let roots = minimal_roots(roots);
    let workers = roots.len().min(4);
    let pending = Mutex::new(roots.into_iter());
    let (sender, receiver) = mpsc::sync_channel::<Vec<Item>>(8);
    thread::scope(|scope| {
        // ponytail: parallelize roots, not individual subdirectories; add bounded work stealing only if scan I/O becomes the bottleneck.
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let (sender, pending) = (sender.clone(), &pending);
                scope.spawn(move || {
                    let mut skipped = 0;
                    let mut warning = String::new();
                    let mut error = |path: &Path, e: std::io::Error| {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            skipped += 1;
                            if warning.is_empty() {
                                warning = format!("{}：{e}", path.display());
                            }
                        }
                    };
                    let mut items = Vec::with_capacity(4096);
                    let mut last_flush = Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        let Some(root) = pending.lock().unwrap().next() else {
                            break;
                        };
                        let mut stack = Vec::<fs::ReadDir>::new();
                        let mut next = Some((root.clone(), fs::symlink_metadata(&root)));
                        loop {
                            if stop.load(Ordering::Relaxed) {
                                return (skipped, warning);
                            }
                            if let Some((path, metadata)) = next.take() {
                                match metadata {
                                    Err(e) => error(&path, e),
                                    Ok(metadata) => {
                                        items.push(Item::new(path.clone(), metadata.is_dir()));
                                        if traversable(&metadata) {
                                            match fs::read_dir(&path) {
                                                Ok(entries) => stack.push(entries),
                                                Err(e) => error(&path, e),
                                            }
                                        }
                                    }
                                }
                            }
                            if items.len() >= 4096
                                || (!items.is_empty()
                                    && last_flush.elapsed() >= Duration::from_millis(100))
                            {
                                if sender.send(std::mem::take(&mut items)).is_err() {
                                    return (skipped, warning);
                                }
                                last_flush = Instant::now();
                            }
                            while let Some(entries) = stack.last_mut() {
                                match entries.next() {
                                    Some(Ok(entry)) => {
                                        next = Some((entry.path(), entry.metadata()));
                                        break;
                                    }
                                    Some(Err(e)) => error(&root, e),
                                    None => {
                                        stack.pop();
                                    }
                                }
                                if stop.load(Ordering::Relaxed) {
                                    return (skipped, warning);
                                }
                            }
                            if next.is_none() {
                                break;
                            }
                        }
                    }
                    if !items.is_empty() && !stop.load(Ordering::Relaxed) {
                        let _ = sender.send(items);
                    }
                    (skipped, warning)
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
