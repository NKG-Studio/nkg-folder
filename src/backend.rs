use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    fs::OpenOptionsExt,
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::*,
        Security::*,
        Storage::FileSystem::*,
        System::{Com::CoCreateGuid, IO::*, Pipes::*, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
    },
    core::{PCWSTR, PWSTR},
};

// Overlapped handles allow one reader and one writer to operate concurrently.
#[derive(Clone)]
struct Pipe(Arc<OwnedHandle>);
impl AsRawHandle for Pipe {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        self.0.as_raw_handle()
    }
}
impl Pipe {
    fn io(
        &self,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> std::io::Result<usize> {
        unsafe {
            let event =
                owned(CreateEventW(None, true, false, None).map_err(std::io::Error::other)?);
            let mut overlap = OVERLAPPED {
                hEvent: handle(&event),
                ..Default::default()
            };
            if let Err(error) = start(&mut overlap)
                && error.code() != windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0)
            {
                return Err(std::io::Error::other(error));
            }
            let mut count = 0;
            GetOverlappedResult(handle(self), &overlap, &mut count, true)
                .map_err(std::io::Error::other)?;
            Ok(count as usize)
        }
    }
}
impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.io(|overlap| unsafe { ReadFile(handle(self), Some(bytes), None, Some(overlap)) })
    }
}
impl Write for Pipe {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.io(|overlap| unsafe { WriteFile(handle(self), Some(bytes), None, Some(overlap)) })
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const MAX_FRAME: usize = 64 * 1024 * 1024;
static CLIENT: OnceLock<Arc<Client>> = OnceLock::new();
static USER_TOKEN: OnceLock<OwnedHandle> = OnceLock::new();
static STATUS: OnceLock<String> = OnceLock::new();

// Windows paths may contain unpaired UTF-16 surrogates: never turn them into lossy text.
#[derive(Serialize, Deserialize)]
pub(crate) struct NativePath(#[serde(with = "native_path")] pub PathBuf);
pub(crate) mod native_path {
    use super::*;
    pub fn serialize<S: serde::Serializer>(value: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        value
            .as_os_str()
            .encode_wide()
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<PathBuf, D::Error> {
        Ok(PathBuf::from(std::ffi::OsString::from_wide(
            &Vec::<u16>::deserialize(deserializer)?,
        )))
    }
}
pub(crate) mod native_paths {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &[PathBuf],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .iter()
            .cloned()
            .map(NativePath)
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Ok(Vec::<NativePath>::deserialize(deserializer)?
            .into_iter()
            .map(|v| v.0)
            .collect())
    }
}
pub(crate) mod native_optional {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.clone().map(NativePath).serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        Ok(Option::<NativePath>::deserialize(deserializer)?.map(|v| v.0))
    }
}
pub(crate) mod native_guards {
    use super::*;
    use crate::files::FileIdentity;
    pub fn serialize<S: serde::Serializer>(
        value: &[(PathBuf, FileIdentity)],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .iter()
            .map(|(p, id)| (NativePath(p.clone()), id))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<(PathBuf, FileIdentity)>, D::Error> {
        Ok(
            Vec::<(NativePath, FileIdentity)>::deserialize(deserializer)?
                .into_iter()
                .map(|(p, id)| (p.0, id))
                .collect(),
        )
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Command {
    List(#[serde(with = "native_path")] PathBuf),
    Details(#[serde(with = "native_paths")] Vec<PathBuf>),
    Resolve(#[serde(with = "native_path")] PathBuf),
    Identity(#[serde(with = "native_path")] PathBuf),
    Operate(crate::files::Operation),
    Global {
        #[serde(with = "native_paths")]
        roots: Vec<PathBuf>,
        #[serde(with = "native_optional")]
        cache: Option<PathBuf>,
    },
    Query {
        session: u64,
        ticket: u64,
        query: String,
    },
    Rebuild(u64),
    Cancel(u64),
    Search {
        #[serde(with = "native_paths")]
        roots: Vec<PathBuf>,
        query: String,
    },
}
#[derive(Serialize, Deserialize)]
struct Request {
    id: u64,
    command: Command,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    id: u64,
    value: Result<serde_json::Value, String>,
    done: bool,
}

fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FRAME {
        return Err("后台消息超过大小限制".into());
    }
    writer
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|_| writer.write_all(&bytes))
        .map_err(|e| format!("后台连接写入失败：{e}"))
}
fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, String> {
    let mut length = [0; 4];
    reader
        .read_exact(&mut length)
        .map_err(|e| format!("后台连接已断开：{e}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err("后台消息长度无效".into());
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("后台消息无效：{e}"))
}
fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}
fn owned(handle: HANDLE) -> OwnedHandle {
    unsafe { OwnedHandle::from_raw_handle(handle.0) }
}
fn handle(file: &impl AsRawHandle) -> HANDLE {
    HANDLE(file.as_raw_handle())
}
fn elevated(process: HANDLE) -> Result<bool, String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).map_err(|e| e.to_string())?;
        let token = owned(token);
        let mut value = TOKEN_ELEVATION::default();
        let mut size = 0;
        GetTokenInformation(
            handle(&token),
            TokenElevation,
            Some((&mut value as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of_val(&value) as u32,
            &mut size,
        )
        .map_err(|e| e.to_string())?;
        Ok(value.TokenIsElevated != 0)
    }
}

pub(crate) fn client() -> Option<&'static Arc<Client>> {
    CLIENT.get()
}
pub(crate) fn status() -> &'static str {
    if CLIENT
        .get()
        .is_some_and(|c| !c.alive.load(Ordering::Acquire))
    {
        "管理员访问已断开，请重启"
    } else {
        STATUS.get().map_or("普通权限", String::as_str)
    }
}

pub(crate) struct Client {
    writer: mpsc::Sender<Request>,
    pending: Mutex<HashMap<u64, mpsc::SyncSender<Reply>>>,
    next: AtomicU64,
    alive: AtomicBool,
    _process: OwnedHandle,
    pipe: Pipe,
}
pub(crate) struct Stream {
    pub id: u64,
    rx: mpsc::Receiver<Reply>,
    client: Arc<Client>,
}
impl Stream {
    pub fn recv_timeout<T: DeserializeOwned>(
        &self,
        timeout: Duration,
    ) -> Result<Option<(T, bool)>, String> {
        match self.rx.recv_timeout(timeout) {
            Ok(reply) => Ok(Some((
                serde_json::from_value(reply.value?).map_err(|e| e.to_string())?,
                reply.done,
            ))),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("管理员后台连接已断开".into()),
        }
    }
    pub fn recv<T: DeserializeOwned>(&self) -> Result<(T, bool), String> {
        let reply = self.rx.recv().map_err(|_| {
            "管理员后台已断开；操作结果可能未知，请刷新核对，勿直接重复操作".to_string()
        })?;
        Ok((
            serde_json::from_value(reply.value?).map_err(|e| e.to_string())?,
            reply.done,
        ))
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.client.pending.lock().unwrap().remove(&self.id);
        self.client.send(Command::Cancel(self.id));
    }
}
impl Client {
    pub fn stream(self: &Arc<Self>, command: Command) -> Result<Stream, String> {
        if !self.alive.load(Ordering::Acquire) {
            return Err("管理员后台已退出，请重新启动程序".into());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::sync_channel(256);
        self.pending.lock().unwrap().insert(id, tx);
        if self.writer.send(Request { id, command }).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err("管理员后台已断开".into());
        }
        Ok(Stream {
            id,
            rx,
            client: self.clone(),
        })
    }
    pub fn call<T: DeserializeOwned>(self: &Arc<Self>, command: Command) -> Result<T, String> {
        self.stream(command)?.recv().map(|v| v.0)
    }
    pub fn send(&self, command: Command) {
        let _ = self.writer.send(Request { id: 0, command });
    }
    fn disconnect(&self) {
        self.alive.store(false, Ordering::Release);
        self.pending.lock().unwrap().clear();
        let _ = unsafe { DisconnectNamedPipe(handle(&self.pipe)) };
    }
    fn launch(executable: &Path) -> Result<Arc<Self>, String> {
        unsafe {
            let name = format!(
                r"\\.\pipe\NkgFolder-{}-{:?}",
                std::process::id(),
                CoCreateGuid().map_err(|e| e.to_string())?
            );
            let name_w = wide(&name);
            let raw = CreateNamedPipeW(
                PCWSTR(name_w.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                65536,
                65536,
                0,
                None,
            );
            if raw == INVALID_HANDLE_VALUE {
                return Err(windows::core::Error::from_thread().to_string());
            }
            let mut pipe = Pipe(Arc::new(owned(raw)));
            let exe = wide(executable);
            #[cfg(not(test))]
            let args = wide(format!("--elevated-worker {name} {}", std::process::id()));
            #[cfg(test)]
            let args = wide(format!(
                "backend::tests::worker_host --exact --ignored --nocapture --skip {name} --skip {}",
                std::process::id()
            ));
            let verb = wide("runas");
            let mut launch = SHELLEXECUTEINFOW {
                cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
                lpVerb: PCWSTR(verb.as_ptr()),
                lpFile: PCWSTR(exe.as_ptr()),
                lpParameters: PCWSTR(args.as_ptr()),
                nShow: SW_HIDE.0,
                ..Default::default()
            };
            ShellExecuteExW(&mut launch).map_err(|e| format!("未获得管理员后台授权：{e}"))?;
            if launch.hProcess.is_invalid() {
                return Err("后台进程未启动".into());
            }
            let process = owned(launch.hProcess);
            let expected = GetProcessId(handle(&process));
            let event = owned(CreateEventW(None, true, false, None).map_err(|e| e.to_string())?);
            let mut overlap = OVERLAPPED {
                hEvent: handle(&event),
                ..Default::default()
            };
            match ConnectNamedPipe(handle(&pipe), Some(&mut overlap)) {
                Ok(()) => {}
                Err(error)
                    if error.code()
                        == windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => {}
                Err(error)
                    if error.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) =>
                {
                    if WaitForSingleObject(handle(&event), 30_000) != WAIT_OBJECT_0 {
                        let _ = CancelIoEx(handle(&pipe), Some(&overlap));
                        let mut count = 0;
                        let _ = GetOverlappedResult(handle(&pipe), &overlap, &mut count, true);
                        return Err("管理员后台连接超时".into());
                    }
                    let mut count = 0;
                    GetOverlappedResult(handle(&pipe), &overlap, &mut count, true)
                        .map_err(|e| e.to_string())?;
                }
                Err(error) => return Err(error.to_string()),
            }
            let mut peer = 0;
            GetNamedPipeClientProcessId(handle(&pipe), &mut peer).map_err(|e| e.to_string())?;
            if peer != expected || !elevated(handle(&process))? {
                return Err("后台进程身份核验失败".into());
            }
            SetNamedPipeHandleState(handle(&pipe), Some(&PIPE_WAIT), None, None)
                .map_err(|e| e.to_string())?;
            // Both peers authenticate before any privileged request is accepted.
            write_frame(&mut pipe, &1u32)?;
            if read_frame::<u32>(&mut pipe)? != 1 {
                return Err("后台协议版本不匹配".into());
            }
            let mut writer = pipe.clone();
            let (tx, requests) = mpsc::channel();
            let client = Arc::new(Self {
                writer: tx,
                pending: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
                alive: AtomicBool::new(true),
                _process: process,
                pipe: pipe.clone(),
            });
            let weak = Arc::downgrade(&client);
            thread::spawn(move || {
                while let Ok(request) = requests.recv() {
                    if write_frame(&mut writer, &request).is_err() {
                        break;
                    }
                }
                if let Some(c) = weak.upgrade() {
                    c.disconnect();
                }
            });
            let weak = Arc::downgrade(&client);
            thread::spawn(move || {
                while let Ok(reply) = read_frame::<Reply>(&mut pipe) {
                    let Some(client) = weak.upgrade() else {
                        return;
                    };
                    let sender = {
                        let mut pending = client.pending.lock().unwrap();
                        if reply.done {
                            pending.remove(&reply.id)
                        } else {
                            pending.get(&reply.id).cloned()
                        }
                    };
                    if let Some(sender) = sender {
                        let _ = sender.send(reply);
                    }
                }
                if let Some(c) = weak.upgrade() {
                    c.disconnect();
                }
            });
            Ok(client)
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = unsafe { DisconnectNamedPipe(handle(&self.pipe)) };
    }
}

pub(crate) fn initialize() {
    match std::env::current_exe()
        .map_err(|e| e.to_string())
        .and_then(|path| Client::launch(&path))
    {
        Ok(client) => {
            let _ = CLIENT.set(client);
            let _ = STATUS.set("管理员访问已启用".into());
        }
        Err(error) => {
            let _ = STATUS.set(format!("普通权限运行：{error}"));
            rfd::MessageDialog::new()
                .set_title("NKG Virtual Folder — 普通权限模式")
                .set_description(format!(
                    "{error}\n仍可使用普通权限能够访问的目录。重新启动程序可再次申请后台权限。"
                ))
                .set_level(rfd::MessageLevel::Warning)
                .show();
        }
    }
}

/// Cache I/O uses the GUI user's token, never an elevated write into a user-writable path.
pub(crate) fn user_io<T>(operation: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let Some(token) = USER_TOKEN.get() else {
        return operation();
    };
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            if unsafe { RevertToSelf() }.is_err() {
                std::process::abort();
            }
        }
    }
    unsafe {
        SetThreadToken(None, Some(handle(token))).map_err(|e| e.to_string())?;
    }
    let _restore = Restore;
    operation()
}

pub(crate) struct Sink {
    id: u64,
    writer: Arc<Mutex<Pipe>>,
    pub stop: Arc<AtomicBool>,
}
impl Sink {
    pub fn emit(&self, value: &impl Serialize, done: bool) -> bool {
        self.reply(serde_json::to_value(value).map_err(|e| e.to_string()), done)
    }
    pub fn error(&self, error: String) {
        self.reply(Err(error), true);
    }
    fn reply(&self, value: Result<serde_json::Value, String>, done: bool) -> bool {
        if self.stop.load(Ordering::Relaxed) {
            return false;
        }
        let mut writer = self.writer.lock().unwrap();
        let reply = Reply {
            id: self.id,
            value,
            done,
        };
        if let Err(error) = write_frame(&mut *writer, &reply) {
            let _ = write_frame(
                &mut *writer,
                &Reply {
                    id: self.id,
                    value: Err(error),
                    done: true,
                },
            );
            return false;
        }
        true
    }
}

pub(crate) fn worker_entry() -> bool {
    worker_args(std::env::args_os().collect())
}
fn worker_args(args: Vec<std::ffi::OsString>) -> bool {
    if args.get(1).is_none_or(|a| a != "--elevated-worker") {
        return false;
    }
    let result = (|| -> Result<(), String> {
        if args.len() != 4 || !unsafe { elevated(GetCurrentProcess()) }? {
            return Err("后台必须提权启动".into());
        }
        let parent: u32 = args[3]
            .to_str()
            .ok_or("父进程参数无效")?
            .parse()
            .map_err(|_| "父进程参数无效")?;
        let pipe_name = args[2].to_str().ok_or("管道名称无效")?;
        if !pipe_name.starts_with(&format!(r"\\.\pipe\NkgFolder-{parent}-")) {
            return Err("管道名称无效".into());
        }
        unsafe {
            let process = owned(
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    parent,
                )
                .map_err(|e| e.to_string())?,
            );
            let mut path = vec![0; 32768];
            let mut length = path.len() as u32;
            QueryFullProcessImageNameW(
                handle(&process),
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
            .map_err(|e| e.to_string())?;
            let parent_exe = PathBuf::from(std::ffi::OsString::from_wide(&path[..length as usize]));
            if fs::canonicalize(parent_exe).map_err(|e| e.to_string())?
                != fs::canonicalize(std::env::current_exe().map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?
            {
                return Err("父进程不是同一程序".into());
            }
            let mut token = HANDLE::default();
            OpenProcessToken(handle(&process), TOKEN_QUERY | TOKEN_DUPLICATE, &mut token)
                .map_err(|e| e.to_string())?;
            let token = owned(token);
            let mut impersonation = HANDLE::default();
            DuplicateTokenEx(
                handle(&token),
                TOKEN_IMPERSONATE | TOKEN_QUERY,
                None,
                SecurityImpersonation,
                TokenImpersonation,
                &mut impersonation,
            )
            .map_err(|e| e.to_string())?;
            let _ = USER_TOKEN.set(owned(impersonation));
            let pipe = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(
                    SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0 | FILE_FLAG_OVERLAPPED.0,
                )
                .open(pipe_name)
                .map_err(|e| e.to_string())?;
            let mut pipe = Pipe(Arc::new(pipe.into()));
            let mut peer = 0;
            GetNamedPipeServerProcessId(handle(&pipe), &mut peer).map_err(|e| e.to_string())?;
            if peer != parent {
                return Err("父进程身份核验失败".into());
            }
            if read_frame::<u32>(&mut pipe)? != 1 {
                return Err("协议版本不匹配".into());
            }
            write_frame(&mut pipe, &1u32)?;
            thread::spawn(move || {
                WaitForSingleObject(handle(&process), INFINITE);
                std::process::exit(0);
            });
            serve(pipe)
        }
    })();
    if result.is_err() {
        std::process::exit(1);
    }
    true
}

fn serve(mut pipe: Pipe) -> Result<(), String> {
    let writer = Arc::new(Mutex::new(pipe.clone()));
    let jobs = Arc::new(Mutex::new(HashMap::<u64, Arc<AtomicBool>>::new()));
    let globals = Arc::new(Mutex::new(HashMap::<
        u64,
        crate::global_search::RemoteControl,
    >::new()));
    while let Ok(request) = read_frame::<Request>(&mut pipe) {
        match request.command {
            Command::Cancel(id) => {
                if let Some(stop) = jobs.lock().unwrap().get(&id) {
                    stop.store(true, Ordering::Relaxed);
                }
                continue;
            }
            Command::Query {
                session,
                ticket,
                query,
            } => {
                if let Some(control) = globals.lock().unwrap().get(&session) {
                    control.query(ticket, query);
                }
                continue;
            }
            Command::Rebuild(id) => {
                if let Some(control) = globals.lock().unwrap().get(&id) {
                    control.rebuild();
                }
                continue;
            }
            _ => {}
        }
        let stop = Arc::new(AtomicBool::new(false));
        let sink = Sink {
            id: request.id,
            writer: writer.clone(),
            stop: stop.clone(),
        };
        if jobs.lock().unwrap().len() >= 64 {
            sink.error("后台请求过多，请稍后重试".into());
            continue;
        }
        jobs.lock().unwrap().insert(request.id, stop);
        let jobs = jobs.clone();
        let globals = globals.clone();
        thread::spawn(move || {
            let result = dispatch(request.command, &sink, &globals);
            if let Err(error) = result {
                sink.error(error);
            }
            let _ = write_frame(
                &mut *sink.writer.lock().unwrap(),
                &Reply {
                    id: request.id,
                    value: Ok(serde_json::Value::Null),
                    done: true,
                },
            );
            globals.lock().unwrap().remove(&request.id);
            jobs.lock().unwrap().remove(&request.id);
        });
    }
    for stop in jobs.lock().unwrap().values() {
        stop.store(true, Ordering::Relaxed);
    }
    Ok(())
}
fn dispatch(
    command: Command,
    sink: &Sink,
    globals: &Mutex<HashMap<u64, crate::global_search::RemoteControl>>,
) -> Result<(), String> {
    match command {
        Command::List(path) => {
            sink.emit(&crate::files::read_directory(&path)?, true);
        }
        Command::Details(paths) => {
            sink.emit(&crate::files::selection_details(&paths), true);
        }
        Command::Resolve(path) => {
            let (path, directory) = crate::files::resolve(&path)?;
            sink.emit(&(NativePath(path), directory), true);
        }
        Command::Identity(path) => {
            sink.emit(&crate::files::file_identity(&path)?, true);
        }
        Command::Operate(op) => {
            op.validate_remote()?;
            let result = crate::files::operate(op, |text| {
                sink.emit(&text, false);
            })?;
            sink.emit(&result, true);
        }
        Command::Global { roots, cache } => {
            crate::global_search::serve_remote(roots, cache, sink, |control| {
                globals.lock().unwrap().insert(sink.id, control);
            })
        }
        Command::Search { roots, query } => crate::search::serve_remote(roots, query, sink),
        _ => return Err("后台命令无效".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "internal elevated worker host, launched only by elevated_round_trip"]
    fn worker_host() {
        let args: Vec<_> = std::env::args_os().collect();
        let values: Vec<_> = args
            .windows(2)
            .filter(|w| w[0] == "--skip")
            .map(|w| w[1].clone())
            .collect();
        assert_eq!(values.len(), 2);
        assert!(worker_args(vec![
            args[0].clone(),
            "--elevated-worker".into(),
            values[0].clone(),
            values[1].clone()
        ]));
    }

    #[test]
    #[ignore = "requires accepting UAC; only modifies temporary fixtures"]
    fn elevated_round_trip() {
        assert!(
            !unsafe { elevated(GetCurrentProcess()) }.unwrap(),
            "run this acceptance from a normal terminal"
        );
        let client = Client::launch(&std::env::current_exe().unwrap()).unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(fixture.path()).unwrap();
        let file = root.join("中文 $ [] file.txt");
        fs::write(&file, "unchanged").unwrap();
        let native = root.join(std::ffi::OsString::from_wide(&[0xd800, 46, 116, 120, 116]));
        fs::write(&native, "native").unwrap();
        let entries: Vec<crate::files::Entry> = client.call(Command::List(root.clone())).unwrap();
        assert!(entries.iter().any(|e| e.path == native));
        let _: crate::files::FileIdentity = client.call(Command::Identity(native)).unwrap();
        assert!(entries.iter().any(|e| e.path == file));
        let identity: crate::files::FileIdentity =
            client.call(Command::Identity(file.clone())).unwrap();
        let details: String = client.call(Command::Details(vec![file.clone()])).unwrap();
        assert!(details.contains("9 B"));
        let _: String = client
            .call(Command::Operate(crate::files::Operation::Checked(
                vec![(file.clone(), identity)],
                Box::new(crate::files::Operation::Rename {
                    source: file.clone(),
                    name: "renamed.txt".into(),
                }),
            )))
            .unwrap();
        let renamed = root.join("renamed.txt");
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "unchanged");
        let stale: crate::files::FileIdentity =
            client.call(Command::Identity(renamed.clone())).unwrap();
        fs::write(&renamed, "new content").unwrap();
        assert!(
            client
                .call::<String>(Command::Operate(crate::files::Operation::Checked(
                    vec![(renamed.clone(), stale)],
                    Box::new(crate::files::Operation::PermanentDelete(vec![
                        renamed.clone()
                    ]))
                )))
                .is_err()
        );
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "new content");
        fs::write(&renamed, "unchanged").unwrap();
        assert!(
            client
                .call::<String>(Command::Operate(crate::files::Operation::Create {
                    parent: PathBuf::from("relative"),
                    name: "bad".into(),
                    directory: false
                }))
                .is_err()
        );
        let invalid = client.call::<Vec<crate::files::Entry>>(Command::List(root.join("missing")));
        assert!(invalid.is_err());
        protected_directory(&client, &root);
        crate::global_search::verify_remote_client(&client, &root);
        crate::search::verify_remote_client(&client, &root);
        // Cache writes impersonate the GUI user; ordinary file permissions stay intact.
        client.disconnect();
        assert!(
            client
                .call::<Vec<crate::files::Entry>>(Command::List(root))
                .is_err()
        );
        assert_eq!(fs::read_to_string(renamed).unwrap(), "unchanged");
    }

    fn protected_directory(client: &Arc<Client>, root: &Path) {
        use windows::Win32::Security::Authorization::*;
        let path = root.join("admin-only");
        fs::create_dir(&path).unwrap();
        let name = wide(&path);
        struct Restore {
            name: Vec<u16>,
            descriptor: PSECURITY_DESCRIPTOR,
            acl: *mut ACL,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    let _ = SetNamedSecurityInfoW(
                        PCWSTR(self.name.as_ptr()),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
                        None,
                        None,
                        Some(self.acl),
                        None,
                    );
                    LocalFree(Some(HLOCAL(self.descriptor.0)));
                }
            }
        }
        unsafe {
            let mut original = PSECURITY_DESCRIPTOR::default();
            let mut old_acl = std::ptr::null_mut();
            GetNamedSecurityInfoW(
                PCWSTR(name.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut old_acl),
                None,
                &mut original,
            )
            .ok()
            .unwrap();
            let _restore = Restore {
                name: name.clone(),
                descriptor: original,
                acl: old_acl,
            };
            let sddl = wide("D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)");
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                1,
                &mut descriptor,
                None,
            )
            .unwrap();
            let mut present = windows::core::BOOL::default();
            let mut defaulted = windows::core::BOOL::default();
            let mut acl = std::ptr::null_mut();
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted).unwrap();
            let result = SetNamedSecurityInfoW(
                PCWSTR(name.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(acl),
                None,
            );
            LocalFree(Some(HLOCAL(descriptor.0)));
            result.ok().unwrap();
            assert_eq!(
                fs::read_dir(&path).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            let _: String = client
                .call(Command::Operate(crate::files::Operation::Create {
                    parent: path.clone(),
                    name: "protected.txt".into(),
                    directory: false,
                }))
                .unwrap();
            let listed: Vec<crate::files::Entry> =
                client.call(Command::List(path.clone())).unwrap();
            assert!(listed.iter().any(|entry| entry.name == "protected.txt"));
            {
                use windows::Win32::System::{
                    Com::*,
                    Ole::{CF_HDROP, ReleaseStgMedium},
                };
                CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
                let data =
                    crate::shell_menu::data_object(&[path.join("protected.txt")], HWND::default())
                        .unwrap();
                let format = FORMATETC {
                    cfFormat: CF_HDROP.0,
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                    ..Default::default()
                };
                let mut medium = data.GetData(&format).unwrap();
                assert_eq!(DragQueryFileW(HDROP(medium.u.hGlobal.0), u32::MAX, None), 1);
                ReleaseStgMedium(&mut medium);
                drop(data);
                CoUninitialize();
            }

            let resolved: (NativePath, bool) = client.call(Command::Resolve(path.clone())).unwrap();
            assert!(resolved.1);
            let _: String = client
                .call(Command::Operate(crate::files::Operation::PermanentDelete(
                    vec![path.join("protected.txt")],
                )))
                .unwrap();
        }
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn native_paths_round_trip_without_loss() {
        let path = PathBuf::from(std::ffi::OsString::from_wide(&[
            67, 58, 92, 0xd800, 46, 116, 120, 116,
        ]));
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &Request {
                id: 1,
                command: Command::List(path.clone()),
            },
        )
        .unwrap();
        let request: Request = read_frame(&mut bytes.as_slice()).unwrap();
        assert!(matches!(request.command, Command::List(restored) if restored == path));
    }

    #[test]
    fn framing_rejects_oversize_and_truncated_messages() {
        assert!(read_frame::<Request>(&mut &[255u8; 4][..]).is_err());
        assert!(read_frame::<Request>(&mut &[3u8, 0, 0, 0, b'{'][..]).is_err());
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &Reply {
                id: 3,
                value: Ok(serde_json::json!({"identity": u128::MAX})),
                done: true,
            },
        )
        .unwrap();
        let reply: Reply = read_frame(&mut bytes.as_slice()).unwrap();
        assert_eq!(reply.id, 3);
        assert_eq!(
            serde_json::from_value::<HashMap<String, u128>>(reply.value.unwrap()).unwrap()["identity"],
            u128::MAX
        );
    }
}
