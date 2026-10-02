//! Name-only Windows notifications with explicit overflow recovery and owned I/O lifetimes.
use super::*;
use std::os::windows::{
    ffi::OsStringExt,
    fs::OpenOptionsExt,
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use windows::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{IO::*, Threading::*},
};

pub(super) struct Watches {
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl Drop for Watches {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Watches {
    pub fn start(
        roots: &[PathBuf],
        sender: mpsc::SyncSender<notify::Result<notify::Event>>,
        rebuild: Arc<AtomicBool>,
    ) -> (Self, String) {
        let mut watches = Self {
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        let mut warning = String::new();
        for root in roots {
            match Self::open(root) {
                Ok((file, event)) => {
                    let (root, stop, sender, rebuild) = (
                        root.clone(),
                        watches.stop.clone(),
                        sender.clone(),
                        rebuild.clone(),
                    );
                    let (ready, started) = mpsc::channel();
                    watches.threads.push(thread::spawn(move || {
                        Self::run(root, file, event, stop, sender, rebuild, ready)
                    }));
                    if let Ok(Err(error)) = started.recv() {
                        warning = error;
                    }
                }
                Err(error) => {
                    warning = format!(
                        "部分位置无法自动更新，请手动重建：{}：{error}",
                        root.display()
                    )
                }
            }
        }
        (watches, warning)
    }

    fn open(root: &Path) -> Result<(fs::File, OwnedHandle), String> {
        let file = fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY.0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OVERLAPPED.0)
            .open(root)
            .map_err(|e| e.to_string())?;
        let event = unsafe { CreateEventW(None, true, false, None) }.map_err(|e| e.to_string())?;
        // This OwnedHandle is the sole owner, retained until all pending I/O completes.
        Ok((file, unsafe { OwnedHandle::from_raw_handle(event.0) }))
    }

    fn run(
        root: PathBuf,
        file: fs::File,
        event: OwnedHandle,
        stop: Arc<AtomicBool>,
        sender: mpsc::SyncSender<notify::Result<notify::Event>>,
        rebuild: Arc<AtomicBool>,
        ready: mpsc::Sender<Result<(), String>>,
    ) {
        let handle = HANDLE(file.as_raw_handle());
        let event = HANDLE(event.as_raw_handle());
        let mut buffer = vec![0u32; 16 * 1024];
        let send = |value| {
            if sender.try_send(value).is_err() {
                rebuild.store(true, Ordering::Relaxed);
            }
        };
        let mut ready = Some(ready);
        while !stop.load(Ordering::Relaxed) {
            let mut overlapped = OVERLAPPED {
                hEvent: event,
                ..Default::default()
            };
            unsafe {
                let _ = ResetEvent(event);
            }
            let started = unsafe {
                ReadDirectoryChangesW(
                    handle,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 4) as u32,
                    true,
                    FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                    None,
                    Some(&mut overlapped),
                    None,
                )
            };
            if let Err(error) = started {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(error.to_string()));
                }
                send(Err(notify::Error::generic(&format!(
                    "{}：{error}",
                    root.display()
                ))));
                break;
            }
            if let Some(ready) = ready.take() {
                let _ = ready.send(Ok(()));
            }
            let mut bytes = 0;
            loop {
                if stop.load(Ordering::Relaxed) {
                    unsafe {
                        let _ = CancelIoEx(handle, Some(&overlapped));
                        // Cancel is asynchronous: wait before freeing the buffer/OVERLAPPED.
                        let _ = GetOverlappedResult(handle, &overlapped, &mut bytes, true);
                    }
                    return;
                }
                match unsafe { WaitForSingleObject(event, 100) } {
                    WAIT_OBJECT_0 => break,
                    WAIT_TIMEOUT => continue,
                    _ => {
                        unsafe {
                            let _ = CancelIoEx(handle, Some(&overlapped));
                            let _ = GetOverlappedResult(handle, &overlapped, &mut bytes, true);
                        }
                        send(Err(notify::Error::generic("等待目录通知失败")));
                        return;
                    }
                }
            }
            let result = unsafe { GetOverlappedResult(handle, &overlapped, &mut bytes, false) };
            if result.is_err() || bytes == 0 || bytes as usize > buffer.len() * 4 {
                // Reconcile just the affected root. A full application queue falls back to all roots.
                send(Ok(notify::Event::new(EventKind::Any).add_path(root.clone())));
                continue;
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), bytes as usize) };
            match decode(bytes, &root) {
                Some(events) => {
                    for event in events {
                        send(Ok(event));
                    }
                }
                None => send(Ok(notify::Event::new(EventKind::Any).add_path(root.clone()))),
            }
        }
    }
}

fn decode(bytes: &[u8], root: &Path) -> Option<Vec<notify::Event>> {
    let mut offset = 0usize;
    let mut events = Vec::new();
    loop {
        let header = bytes.get(offset..offset.checked_add(12)?)?;
        let number =
            |start| u32::from_le_bytes(header[start..start + 4].try_into().unwrap()) as usize;
        let (next, action, len) = (number(0), number(4), number(8));
        if len == 0 || len % 2 != 0 {
            return None;
        }
        let end = offset.checked_add(12)?.checked_add(len)?;
        let name: Vec<_> = bytes
            .get(offset + 12..end)?
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let name = PathBuf::from(OsString::from_wide(&name));
        if name
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
        let kind = match FILE_ACTION(action as u32) {
            FILE_ACTION_ADDED => EventKind::Create(CreateKind::Any),
            FILE_ACTION_REMOVED => EventKind::Remove(RemoveKind::Any),
            FILE_ACTION_RENAMED_OLD_NAME => EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            FILE_ACTION_RENAMED_NEW_NAME => EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            _ => return None,
        };
        events.push(notify::Event::new(kind).add_path(root.join(name)));
        if next == 0 {
            return Some(events);
        }
        if next % 4 != 0 || next < 12 + len {
            return None;
        }
        offset = offset.checked_add(next)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_application_queue_requests_reconciliation() {
        let temp = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::sync_channel(1);
        let rebuild = Arc::new(AtomicBool::new(false));
        let (_watch, warning) = Watches::start(&[temp.path().to_owned()], tx, rebuild.clone());
        assert!(warning.is_empty(), "{warning}");
        for i in 0..100 {
            fs::write(temp.path().join(format!("{i}.txt")), "x").unwrap();
        }
        let start = Instant::now();
        while !rebuild.load(Ordering::Relaxed) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "overflow was not signalled"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn notification_parser_rejects_truncation_and_escape() {
        let record = |name: &str| {
            let utf16: Vec<_> = name.encode_utf16().collect();
            let mut bytes = Vec::new();
            bytes.extend(0u32.to_le_bytes());
            bytes.extend(FILE_ACTION_ADDED.0.to_le_bytes());
            bytes.extend(((utf16.len() * 2) as u32).to_le_bytes());
            for c in utf16 {
                bytes.extend(c.to_le_bytes());
            }
            bytes
        };
        let root = Path::new("C:/test");
        let good = record("目录\\a.txt");
        assert_eq!(
            decode(&good, root).unwrap()[0].paths[0],
            root.join("目录\\a.txt")
        );
        for length in 0..good.len() {
            assert!(decode(&good[..length], root).is_none());
        }
        assert!(decode(&record("..\\outside"), root).is_none());
        assert!(decode(&record("C:\\outside"), root).is_none());
        let mut bad = good;
        bad[0..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(decode(&bad, root).is_none());
    }
}
