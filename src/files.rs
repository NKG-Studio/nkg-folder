use crate::model::display_path;
use eframe::egui;
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub modified: Option<std::time::SystemTime>,
    #[serde(with = "crate::backend::native_path")]
    pub path: PathBuf,
    pub name: String,
    pub directory: bool,
    pub link: bool,
}

pub fn selection_details(paths: &[PathBuf]) -> String {
    if let Some(client) = crate::backend::client() {
        return client
            .call(crate::backend::Command::Details(paths.to_vec()))
            .unwrap_or_else(|e| e);
    }
    let mut bytes = 0u64;
    let mut directories = 0;
    let mut failed = 0;
    let mut latest = None;
    let mut seen = HashSet::new();
    for path in paths.iter().filter(|p| seen.insert(*p)) {
        match fs::metadata(path) {
            Ok(meta) => {
                if meta.is_dir() {
                    directories += 1;
                } else {
                    bytes = bytes.saturating_add(meta.len());
                }
                if let Ok(modified) = meta.modified() {
                    latest = Some(
                        latest.map_or(modified, |old: std::time::SystemTime| old.max(modified)),
                    );
                }
            }
            Err(_) => failed += 1,
        }
    }
    let size = if directories == 1 && paths.len() == 1 {
        "文件夹（未统计内容）".into()
    } else {
        let mut size = if bytes < 1024 {
            format!("{bytes} B")
        } else if bytes < 1024 * 1024 {
            format!("{:.1} KiB", bytes as f64 / 1024.0)
        } else if bytes < 1024 * 1024 * 1024 {
            format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        };
        if directories > 0 {
            size.push_str(&format!("，另有 {directories} 个文件夹"));
        }
        size
    };
    let date = latest
        .and_then(format_modified)
        .unwrap_or_else(|| "—".into());
    if failed > 0 {
        format!("已读取大小：{size} · 修改日期：{date} · {failed} 项无法读取")
    } else {
        format!(
            "大小：{size} · {}：{date}",
            if paths.len() > 1 {
                "最近修改"
            } else {
                "修改日期"
            }
        )
    }
}

fn format_modified(time: std::time::SystemTime) -> Option<String> {
    #[cfg(windows)]
    {
        use windows::Win32::{
            Foundation::{FILETIME, SYSTEMTIME},
            System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
        };
        let epoch =
            std::time::UNIX_EPOCH.checked_sub(std::time::Duration::from_secs(11_644_473_600))?;
        let ticks = u64::try_from(time.duration_since(epoch).ok()?.as_nanos() / 100).ok()?;
        let filetime = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        let mut utc = SYSTEMTIME::default();
        let mut local = SYSTEMTIME::default();
        unsafe {
            FileTimeToSystemTime(&filetime, &mut utc).ok()?;
            SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
        }
        Some(format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
        ))
    }
    #[cfg(not(windows))]
    Some(format!(
        "{} 秒（Unix 时间）",
        time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs()
    ))
}

pub fn resolve(path: &Path) -> Result<(PathBuf, bool), String> {
    if let Some(client) = crate::backend::client() {
        return client
            .call::<(crate::backend::NativePath, bool)>(crate::backend::Command::Resolve(
                path.to_owned(),
            ))
            .map(|(path, directory)| (path.0, directory));
    }
    let canonical =
        fs::canonicalize(path).map_err(|e| format!("无法打开 {}：{e}", display_path(path)))?;
    let directory = canonical.metadata().map_err(|e| e.to_string())?.is_dir();
    Ok((canonical, directory))
}

pub fn read_directory(path: &Path) -> Result<Vec<Entry>, String> {
    if let Some(client) = crate::backend::client() {
        return client.call(crate::backend::Command::List(path.to_owned()));
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(|e| format!("{}：{e}", display_path(path)))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let link = ty.is_symlink();
        entries.push(Entry {
            modified: entry.metadata().ok().and_then(|m| m.modified().ok()),
            path: entry.path(),
            name: entry.file_name().to_string_lossy().into_owned(),
            directory: ty.is_dir() || (link && entry.path().is_dir()),
            link,
        });
    }
    entries.sort_by_cached_key(|e| (!e.directory, e.name.to_lowercase()));
    Ok(entries)
}

pub enum Listing {
    Loading,
    Ready(Vec<Entry>),
    Error(String),
}
type ScanResult = (u64, PathBuf, Result<Vec<Entry>, String>);

pub struct DirectoryCache {
    modified: HashMap<PathBuf, Option<std::time::SystemTime>>,
    pub entries: HashMap<PathBuf, Listing>,
    pub revision: u64,
    generation: u64,
    tx: mpsc::Sender<(u64, PathBuf)>,
    rx: mpsc::Receiver<ScanResult>,
}

impl DirectoryCache {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, requests) = mpsc::channel::<(u64, PathBuf)>();
        let (results, rx) = mpsc::channel();
        let requests = Arc::new(Mutex::new(requests));
        for _ in 0..2 {
            let requests = requests.clone();
            let results = results.clone();
            let ctx = ctx.clone();
            thread::spawn(move || {
                loop {
                    let Ok((generation, path)) = requests.lock().unwrap().recv() else {
                        break;
                    };
                    let result = read_directory(&path);
                    if results.send((generation, path, result)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            });
        }
        Self {
            modified: HashMap::new(),
            entries: HashMap::new(),
            revision: 1,
            generation: 0,
            tx,
            rx,
        }
    }
    pub fn request(&mut self, path: &Path) {
        if !self.entries.contains_key(path) {
            self.entries.insert(path.to_owned(), Listing::Loading);
            let _ = self.tx.send((self.generation, path.to_owned()));
        }
    }
    pub fn poll(&mut self) {
        while let Ok((generation, path, result)) = self.rx.try_recv() {
            if generation == self.generation {
                self.entries.insert(
                    path,
                    match result {
                        Ok(v) => {
                            self.modified
                                .extend(v.iter().map(|e| (e.path.clone(), e.modified)));
                            Listing::Ready(v)
                        }
                        Err(e) => Listing::Error(e),
                    },
                );
                self.revision += 1;
            }
        }
    }
    pub fn refresh(&mut self) {
        self.generation += 1;
        self.entries.clear();
        self.modified.clear();
        self.revision += 1;
    }

    pub fn modified(&mut self, path: &Path) -> Option<std::time::SystemTime> {
        if let Some(parent) = path.parent() {
            self.request(parent);
        }
        self.modified.get(path).copied().flatten()
    }
}

pub fn validate_name(name: &str) -> Result<(), String> {
    let stem = name.split('.').next().unwrap_or("").to_uppercase();
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$",
        "CONOUT$",
    ];
    if name.trim().is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || reserved.contains(&stem.as_str())
    {
        return Err(
            "请输入有效的 Windows 文件名（不能含路径分隔符、保留名称或末尾空格/句点）。".into(),
        );
    }
    Ok(())
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.is_symlink()
    }
}

fn move_no_replace(source: &Path, target: &Path, copy_allowed: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(source: *const u16, target: *const u16, flags: u32) -> i32;
        }
        let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<_> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        // Never set REPLACE_EXISTING. COPY_ALLOWED supports cross-volume individual files.
        let flags = if copy_allowed { 2 } else { 0 };
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (source, target, copy_allowed);
        Err(io::Error::other("Moving is supported on Windows only"))
    }
}

fn copy_file_new(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CopyFileW(source: *const u16, target: *const u16, fail_if_exists: i32) -> i32;
        }
        let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<_> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        // Native copying preserves file attributes and alternate data streams.
        if unsafe { CopyFileW(source.as_ptr(), target.as_ptr(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let mut input = fs::File::open(source)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()
    }
}

fn move_entry(
    source: &Path,
    target: &Path,
    progress: &mut impl FnMut(String),
) -> Result<(), String> {
    match move_no_replace(source, target, false) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(17) => {
            // ERROR_NOT_SAME_DEVICE
            let metadata = fs::symlink_metadata(source).map_err(|e| e.to_string())?;
            if is_link(&metadata) {
                return Err(format!(
                    "暂不跨盘移动符号链接 / 目录联接：{}",
                    display_path(source)
                ));
            }
            if metadata.is_dir() {
                fs::create_dir(target).map_err(|e| e.to_string())?;
                for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
                    let entry = entry.map_err(|e| e.to_string())?;
                    move_entry(&entry.path(), &target.join(entry.file_name()), progress)?;
                }
                // Only remove an empty directory. Concurrently added files are retained.
                fs::remove_dir(source)
                    .map_err(|e| format!("目标内容已移动，但源目录仍有内容或无法移除：{e}"))?;
            } else {
                move_no_replace(source, target, true).map_err(|e| e.to_string())?;
                // MoveFileEx can report success when copying succeeded but deleting failed.
                if source.try_exists().map_err(|e| e.to_string())? {
                    return Err(format!("已复制，但源文件未移除：{}", display_path(source)));
                }
            }
        }
        Err(error) => return Err(error.to_string()),
    }
    progress(format!("已移动 {}", display_path(source)));
    Ok(())
}

fn copy_entry(
    source: &Path,
    target: &Path,
    progress: &mut impl FnMut(String),
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source).map_err(|e| e.to_string())?;
    if is_link(&metadata) {
        return Err(format!(
            "暂不复制符号链接或目录联接：{}",
            display_path(source)
        ));
    }
    if metadata.is_dir() {
        fs::create_dir(target).map_err(|e| format!("{}：{e}", display_path(target)))?;
        for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_entry(&entry.path(), &target.join(entry.file_name()), progress)?;
        }
    } else {
        copy_file_new(source, target).map_err(|e| format!("{}：{e}", display_path(target)))?;
    }
    progress(format!("已复制 {}", display_path(source)));
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileIdentity {
    id: u128,
    volume: u64,
    created: std::time::SystemTime,
    pub modified: std::time::SystemTime,
    len: u64,
    directory: bool,
}

pub fn file_identity(path: &Path) -> Result<FileIdentity, String> {
    if let Some(client) = crate::backend::client() {
        return client.call(crate::backend::Command::Identity(path.to_owned()));
    }
    let read = || -> io::Result<FileIdentity> {
        #[cfg(windows)]
        let (file, id, volume) = {
            use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
            use windows::Win32::{
                Foundation::HANDLE,
                Storage::FileSystem::{FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx},
            };
            let file = fs::OpenOptions::new()
                .access_mode(0)
                .share_mode(7)
                .custom_flags(0x02200000) // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
                .open(path)?;
            let mut info = FILE_ID_INFO::default();
            unsafe {
                GetFileInformationByHandleEx(
                    HANDLE(file.as_raw_handle()),
                    FileIdInfo,
                    (&mut info as *mut FILE_ID_INFO).cast(),
                    std::mem::size_of::<FILE_ID_INFO>() as u32,
                )
                .map_err(io::Error::other)?;
            }
            (
                file,
                u128::from_le_bytes(info.FileId.Identifier),
                info.VolumeSerialNumber,
            )
        };
        #[cfg(not(windows))]
        let (file, id, volume) = (fs::File::open(path)?, 0, 0);
        let meta = file.metadata()?;
        Ok(FileIdentity {
            id,
            volume,
            created: meta.created()?,
            modified: meta.modified()?,
            len: meta.len(),
            directory: meta.is_dir(),
        })
    };
    read().map_err(|e| format!("无法核验目标 {}：{e}；请刷新后重试", display_path(path)))
}

pub fn validate_targets(expected: &[(PathBuf, FileIdentity)]) -> Result<(), String> {
    for (path, identity) in expected {
        if &file_identity(path)? != identity {
            return Err(format!(
                "目标已变化，已取消操作：{}；请刷新后重新选择",
                display_path(path)
            ));
        }
    }
    Ok(())
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum Operation {
    Checked(
        #[serde(with = "crate::backend::native_guards")] Vec<(PathBuf, FileIdentity)>,
        Box<Operation>,
    ),
    PasteClipboard(
        #[serde(with = "crate::backend::native_path")] PathBuf,
        #[serde(with = "crate::backend::native_guards")] Vec<(PathBuf, FileIdentity)>,
    ),
    Copy {
        #[serde(with = "crate::backend::native_paths")]
        sources: Vec<PathBuf>,
        #[serde(with = "crate::backend::native_path")]
        destination: PathBuf,
        moving: bool,
    },
    Rename {
        #[serde(with = "crate::backend::native_path")]
        source: PathBuf,
        name: String,
    },
    Create {
        #[serde(with = "crate::backend::native_path")]
        parent: PathBuf,
        name: String,
        directory: bool,
    },
    Trash(#[serde(with = "crate::backend::native_paths")] Vec<PathBuf>),
    PermanentDelete(#[serde(with = "crate::backend::native_paths")] Vec<PathBuf>),
}

impl Operation {
    pub(crate) fn validate_remote(&self) -> Result<(), String> {
        let check = |p: &Path| {
            if p.is_absolute() {
                Ok(())
            } else {
                Err("后台文件操作要求绝对路径".to_string())
            }
        };
        match self {
            Self::Checked(guards, op) => {
                for (p, _) in guards {
                    check(p)?;
                }
                op.validate_remote()
            }
            Self::PasteClipboard(..) => Err("后台不接受剪贴板命令".into()),
            Self::Copy {
                sources,
                destination,
                ..
            } => {
                check(destination)?;
                for p in sources {
                    check(p)?;
                }
                Ok(())
            }
            Self::Rename { source, name } => {
                check(source)?;
                validate_name(name)
            }
            Self::Create { parent, name, .. } => {
                check(parent)?;
                validate_name(name)
            }
            Self::Trash(paths) | Self::PermanentDelete(paths) => {
                for p in paths {
                    check(p)?;
                    if p.parent().is_none() || p.file_name().is_none() {
                        return Err("不能删除根目录".into());
                    }
                }
                Ok(())
            }
        }
    }
}

fn permanent_delete(paths: Vec<PathBuf>) -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = paths;
        Err("永久删除仅支持 Windows".into())
    }
    #[cfg(windows)]
    {
        let mut targets = Vec::new();
        for path in paths {
            if !path.is_absolute()
                || is_link(&fs::symlink_metadata(&path).map_err(|e| e.to_string())?)
            {
                return Err("永久删除要求绝对路径，且不接受符号链接 / 目录联接作为目标".into());
            }
            let path = fs::canonicalize(path).map_err(|e| e.to_string())?;
            if path.parent().is_none() || path.file_name().is_none() {
                return Err("不能永久删除磁盘或共享根目录".into());
            }
            if !targets.contains(&path) {
                targets.push(path);
            }
        }
        let targets: Vec<_> = targets
            .iter()
            .filter(|p| {
                !targets
                    .iter()
                    .any(|other| *p != other && p.starts_with(other))
            })
            .collect();
        if targets.is_empty() {
            return Err("未选择删除目标".into());
        }
        for path in &targets {
            let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            let result = if meta.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            result.map_err(|e| format!("永久删除未全部完成，请刷新检查：{e}"))?;
        }
        for path in &targets {
            if fs::symlink_metadata(path).is_ok() {
                return Err(format!("删除后目标仍存在：{}", display_path(path)));
            }
        }
        Ok(format!("已永久删除 {} 项", targets.len()))
    }
}

pub fn operate(op: Operation, mut progress: impl FnMut(String)) -> Result<String, String> {
    if let Some(client) = crate::backend::client() {
        // Resolve nested Checked/PasteClipboard locally; forward the concrete operation once.
        if !matches!(&op, Operation::Checked(..) | Operation::PasteClipboard(..)) {
            let stream = client.stream(crate::backend::Command::Operate(op))?;
            loop {
                let (message, done): (String, bool) = stream.recv()?;
                if done {
                    return Ok(message);
                }
                progress(message);
            }
        }
    }
    match op {
        Operation::Checked(expected, operation) => {
            if let Some(client) = crate::backend::client()
                && !matches!(&*operation, Operation::PasteClipboard(..))
            {
                let stream = client.stream(crate::backend::Command::Operate(
                    Operation::Checked(expected, operation),
                ))?;
                loop {
                    let (message, done): (String, bool) = stream.recv()?;
                    if done {
                        return Ok(message);
                    }
                    progress(message);
                }
            }
            validate_targets(&expected)?;
            operate(*operation, progress)
        }
        Operation::PermanentDelete(paths) => permanent_delete(paths),
        Operation::PasteClipboard(destination, expected) => {
            let clipboard = crate::clipboard::read()?;
            let guards = if clipboard.moving {
                let guards: Vec<_> = expected
                    .into_iter()
                    .filter(|(path, _)| clipboard.paths.contains(path))
                    .collect();
                guards
            } else {
                Vec::new()
            };
            let result = operate(
                Operation::Checked(
                    guards,
                    Box::new(Operation::Copy {
                        sources: clipboard.paths,
                        destination,
                        moving: clipboard.moving,
                    }),
                ),
                progress,
            )?;
            if clipboard.moving
                && let Err(error) = crate::clipboard::finish_move(clipboard.sequence)
            {
                return Ok(format!("{result}；未能清理剪贴板：{error}"));
            }
            Ok(result)
        }
        Operation::Copy {
            sources,
            destination,
            moving,
        } => {
            let destination = fs::canonicalize(destination).map_err(|e| e.to_string())?;
            if !destination.is_dir() {
                return Err("目标不是目录".into());
            }
            let sources: Vec<PathBuf> = sources
                .into_iter()
                .map(|p| {
                    if is_link(&fs::symlink_metadata(&p)?) {
                        return Err(io::Error::other("暂不复制或移动符号链接 / 目录联接"));
                    }
                    fs::canonicalize(p)
                })
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            let all_sources: HashSet<_> = sources.iter().cloned().collect();
            let mut seen = HashSet::new();
            let sources: Vec<_> = sources
                .iter()
                .filter(|s| !s.ancestors().skip(1).any(|p| all_sources.contains(p)))
                .filter(|s| seen.insert((*s).clone()))
                .cloned()
                .collect();
            let mut targets = HashSet::new();
            for source in &sources {
                let name = source.file_name().ok_or("无法复制磁盘根目录")?;
                let target = destination.join(name);
                if destination.starts_with(source) {
                    return Err("不能将目录复制或移动到自身及其子目录。".into());
                }
                if target.try_exists().map_err(|e| e.to_string())?
                    || !targets.insert(name.to_string_lossy().to_lowercase())
                {
                    return Err(format!("目标名称已存在，未覆盖：{}", display_path(&target)));
                }
            }
            for (index, source) in sources.iter().enumerate() {
                let target = destination.join(source.file_name().unwrap());
                let result = if moving {
                    move_entry(source, &target, &mut progress)
                } else {
                    copy_entry(source, &target, &mut progress)
                };
                if let Err(e) = result {
                    return Err(format!(
                        "已完成 {index}/{} 项；{e}。失败目标可能保留部分内容，请检查 {}。",
                        sources.len(),
                        display_path(&target)
                    ));
                }
            }
            Ok(format!(
                "已{} {} 项",
                if moving { "移动" } else { "复制" },
                sources.len()
            ))
        }
        Operation::Rename { source, name } => {
            validate_name(&name)?;
            let target = source.parent().ok_or("不能重命名根目录")?.join(name);
            if target.try_exists().map_err(|e| e.to_string())? {
                return Err("目标名称已存在，未覆盖。".into());
            }
            move_no_replace(&source, &target, false).map_err(|e| e.to_string())?;
            Ok("已重命名；已有虚拟引用如指向旧路径，请重新添加。".into())
        }
        Operation::Create {
            parent,
            name,
            directory,
        } => {
            validate_name(&name)?;
            let path = parent.join(name);
            if directory {
                fs::create_dir(path).map_err(|e| e.to_string())?;
            } else {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map_err(|e| e.to_string())?;
            }
            Ok("已创建".into())
        }
        Operation::Trash(paths) => {
            trash::delete_all(&paths)
                .map_err(|e| format!("移入回收站未全部完成，请刷新检查：{e}"))?;
            Ok(format!("已将 {} 项移入回收站", paths.len()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_file_details_report_size_dates_folders_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sample.bin");
        fs::write(&file, vec![0; 2048]).unwrap();
        let details = selection_details(std::slice::from_ref(&file));
        assert!(details.contains("2.0 KiB"), "{details}");
        assert!(
            details.contains("修改日期：") && !details.contains("修改日期：—"),
            "{details}"
        );
        assert!(selection_details(&[dir.path().into()]).contains("文件夹（未统计内容）"));
        let details = selection_details(&[file.clone(), file, dir.path().join("missing")]);
        assert!(
            details.contains("2.0 KiB") && details.contains("1 项无法读取"),
            "{details}"
        );
    }
    #[cfg(windows)]
    #[test]
    fn permanent_delete_is_literal_and_keeps_junction_targets() {
        use std::{os::windows::process::CommandExt, process::Command};
        let workspace = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let temp = tempfile::tempdir_in(&workspace).unwrap();
        let dir = temp.path().join("删除 [a] & $x ' % !");
        let keep = temp.path().join("保留");
        assert!(dir.starts_with(&workspace) && keep.starts_with(&workspace));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&keep).unwrap();
        fs::write(keep.join("safe.txt"), "keep").unwrap();
        let child = dir.join("nested");
        fs::create_dir(&child).unwrap();
        fs::write(child.join("中文.log"), "delete").unwrap();
        let result = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:NKG_TEST_LINK -Target $env:NKG_TEST_KEEP | Out-Null"])
            .env("NKG_TEST_LINK", dir.join("junction"))
            .env("NKG_TEST_KEEP", &keep)
            .creation_flags(0x08000000).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(permanent_delete(vec![dir.join("junction")]).is_err());
        let root = workspace.ancestors().last().unwrap().to_path_buf();
        assert!(permanent_delete(vec![dir.clone(), root]).is_err());
        assert!(child.exists());
        // .NET Framework can refuse junction deletion; external contents must survive even on failure.
        let result = permanent_delete(vec![dir.clone(), child, dir.clone()]);
        assert_eq!(fs::read_to_string(keep.join("safe.txt")).unwrap(), "keep");
        if result.is_err() {
            let link = dir.join("junction");
            if fs::symlink_metadata(&link).is_ok() {
                fs::remove_dir(link).unwrap();
            }
            permanent_delete(vec![dir.clone()]).unwrap();
        }
        assert!(!dir.exists());
        assert_eq!(fs::read_to_string(keep.join("safe.txt")).unwrap(), "keep");
        permanent_delete(vec![keep.join("safe.txt")]).unwrap();
        assert!(keep.exists());
        assert!(!keep.join("safe.txt").exists());
    }

    #[test]
    fn file_operations_preserve_data_and_reject_invalid_destinations() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(src.join("nested/config.json"), "data").unwrap();
        let copy = |destination: PathBuf| Operation::Copy {
            sources: vec![src.clone()],
            destination,
            moving: false,
        };
        assert!(operate(copy(src.join("nested")), |_| {}).is_err());
        operate(copy(dst.clone()), |_| {}).unwrap();
        assert_eq!(
            fs::read_to_string(dst.join("src/nested/config.json")).unwrap(),
            "data"
        );
        assert!(operate(copy(dst.clone()), |_| {}).is_err());
        assert!(src.join("nested/config.json").exists());
        for name in ["..", "../escape", "a\\b", "CON.txt", "a.", "x ", "NUL"] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        operate(
            Operation::Create {
                parent: dst.clone(),
                name: "新文件.txt".into(),
                directory: false,
            },
            |_| {},
        )
        .unwrap();
        operate(
            Operation::Rename {
                source: dst.join("新文件.txt"),
                name: "改名.txt".into(),
            },
            |_| {},
        )
        .unwrap();
        assert!(dst.join("改名.txt").exists());
        let entries = read_directory(&dst).unwrap();
        assert!(entries.first().unwrap().directory);
    }

    #[cfg(windows)]
    #[test]
    fn native_copy_preserves_streams_and_cross_volume_move() {
        let source_root = tempfile::tempdir().unwrap();
        let destination_root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let source = source_root.path().join("project");
        fs::create_dir_all(source.join("nested")).unwrap();
        let file = source.join("nested/config.json");
        fs::write(&file, "branch content").unwrap();
        fs::write(file.with_file_name("config.json:notes"), "alternate stream").unwrap();
        let copy = source_root.path().join("copy.json");
        copy_file_new(&file, &copy).unwrap();
        assert_eq!(
            fs::read_to_string(copy.with_file_name("copy.json:notes")).unwrap(),
            "alternate stream"
        );
        assert!(copy_file_new(&file, &copy).is_err());
        let destination = destination_root.path().join("project");
        let cross_volume = fs::canonicalize(source_root.path())
            .unwrap()
            .components()
            .next()
            != fs::canonicalize(destination_root.path())
                .unwrap()
                .components()
                .next();
        move_entry(&source, &destination, &mut |_| {}).unwrap();
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(destination.join("nested/config.json")).unwrap(),
            "branch content"
        );
        assert_eq!(
            fs::read_to_string(destination.join("nested/config.json:notes")).unwrap(),
            "alternate stream"
        );
        println!("native move checked across different volume prefixes: {cross_volume}");
    }
}
