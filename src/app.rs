use crate::{
    files::{self, DirectoryCache, Listing, Operation},
    model::{Kind, Location, Node, Pane, Row, Workspace, display_path, path_name},
};
use eframe::egui::{self, Color32, Id, RichText, Sense, Vec2};
use egui_dock::{DockArea, NodePath, Split, TabViewer};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const ROW_HEIGHT: f32 = 25.0;

#[derive(Clone, Copy)]
enum Icon {
    Folder,
    NewFolder,
    NewFile,
    File,
    VirtualFile,
    Home,
    Drive,
    SplitRight,
    SplitBelow,
    Back,
    Forward,
    Up,
    Close,
    Minimize,
    Maximize,
    Restore,
    Code,
    Settings,
    Image,
    Archive,
    Asset,
}

fn paint_chevron(painter: &egui::Painter, center: egui::Pos2, expanded: bool) {
    let offsets = if expanded {
        [(-3.0, -2.0), (0.0, 2.0), (3.0, -2.0)]
    } else {
        [(-2.0, -3.0), (2.0, 0.0), (-2.0, 3.0)]
    };
    painter.add(egui::Shape::line(
        offsets.map(|(x, y)| center + Vec2::new(x, y)).to_vec(),
        egui::Stroke::new(
            1.25,
            painter
                .ctx()
                .style_of(painter.ctx().theme())
                .visuals
                .weak_text_color(),
        ),
    ));
}

fn paint_icon(painter: &egui::Painter, center: egui::Pos2, icon: Icon, color: Color32) {
    let rect = egui::Rect::from_center_size(center, Vec2::new(14.0, 12.0));
    let stroke = egui::Stroke::new(1.2, color);
    match icon {
        Icon::NewFolder | Icon::NewFile => {
            let outline = if matches!(icon, Icon::NewFolder) {
                vec![
                    (-1.0, -4.0),
                    (-3.0, -6.0),
                    (-8.0, -6.0),
                    (-8.0, 6.0),
                    (6.0, 6.0),
                    (6.0, 0.0),
                ]
            } else {
                vec![
                    (0.0, -7.0),
                    (-6.0, -7.0),
                    (-6.0, 7.0),
                    (5.0, 7.0),
                    (5.0, 1.0),
                ]
            };
            painter.add(egui::Shape::line(
                outline
                    .into_iter()
                    .map(|(x, y)| center + Vec2::new(x, y))
                    .collect(),
                stroke,
            ));
            let plus = center + Vec2::new(5.0, -5.0);
            painter.line_segment(
                [plus - Vec2::new(3.0, 0.0), plus + Vec2::new(3.0, 0.0)],
                stroke,
            );
            painter.line_segment(
                [plus - Vec2::new(0.0, 3.0), plus + Vec2::new(0.0, 3.0)],
                stroke,
            );
        }
        Icon::Folder => {
            painter.add(egui::Shape::closed_line(
                [
                    (-7.0, -5.0),
                    (-2.0, -5.0),
                    (0.0, -3.0),
                    (7.0, -3.0),
                    (7.0, 6.0),
                    (-7.0, 6.0),
                ]
                .map(|(x, y)| center + Vec2::new(x, y))
                .to_vec(),
                stroke,
            ));
        }
        Icon::Back | Icon::Forward | Icon::Up => {
            let transform = |x: f32, y: f32| {
                center
                    + match icon {
                        Icon::Forward => Vec2::new(-x, y),
                        Icon::Up => Vec2::new(y, x),
                        _ => Vec2::new(x, y),
                    }
            };
            painter.line_segment([transform(-6.0, 0.0), transform(6.0, 0.0)], stroke);
            painter.add(egui::Shape::line(
                [(-1.0, -5.0), (-6.0, 0.0), (-1.0, 5.0)]
                    .map(|(x, y)| transform(x, y))
                    .to_vec(),
                stroke,
            ));
        }
        Icon::Close => {
            painter.line_segment(
                [center + Vec2::splat(-4.0), center + Vec2::splat(4.0)],
                stroke,
            );
            painter.line_segment(
                [center + Vec2::new(-4.0, 4.0), center + Vec2::new(4.0, -4.0)],
                stroke,
            );
        }
        Icon::Minimize => {
            painter.line_segment(
                [center - Vec2::new(5.0, 0.0), center + Vec2::new(5.0, 0.0)],
                stroke,
            );
        }
        Icon::Maximize | Icon::Restore => {
            let r = egui::Rect::from_center_size(center, Vec2::splat(10.0));
            if matches!(icon, Icon::Restore) {
                painter.add(egui::Shape::line(
                    vec![
                        r.left_top() + Vec2::new(2.0, -2.0),
                        r.right_top() + Vec2::new(2.0, -2.0),
                        r.right_bottom() + Vec2::new(2.0, -2.0),
                    ],
                    stroke,
                ));
            }
            painter.rect_stroke(r, 0, stroke, egui::StrokeKind::Inside);
        }
        Icon::Code => {
            for sign in [-1.0, 1.0] {
                painter.add(egui::Shape::line(
                    [(3.0, -4.0), (7.0, 0.0), (3.0, 4.0)]
                        .map(|(x, y)| center + Vec2::new(x * sign, y))
                        .to_vec(),
                    stroke,
                ));
            }
            painter.line_segment(
                [center + Vec2::new(1.0, -6.0), center + Vec2::new(-1.0, 6.0)],
                stroke,
            );
        }
        Icon::Settings => {
            for (y, x) in [(-4.0, -2.0), (0.0, 3.0), (4.0, -3.0)] {
                painter.line_segment(
                    [center + Vec2::new(-7.0, y), center + Vec2::new(7.0, y)],
                    stroke,
                );
                painter.circle_filled(center + Vec2::new(x, y), 2.0, color);
            }
        }
        Icon::Asset => {
            painter.add(egui::Shape::closed_line(
                [
                    (0.0, -7.0),
                    (6.0, -3.0),
                    (6.0, 4.0),
                    (0.0, 7.0),
                    (-6.0, 4.0),
                    (-6.0, -3.0),
                ]
                .map(|(x, y)| center + Vec2::new(x, y))
                .to_vec(),
                stroke,
            ));
            for (x, y) in [(0.0, 7.0), (-6.0, -3.0), (6.0, -3.0)] {
                painter.line_segment([center, center + Vec2::new(x, y)], stroke);
            }
        }
        Icon::VirtualFile => {
            painter.add(egui::Shape::closed_line(
                vec![
                    center + Vec2::new(0.0, -6.0),
                    center + Vec2::new(6.0, 0.0),
                    center + Vec2::new(0.0, 6.0),
                    center + Vec2::new(-6.0, 0.0),
                ],
                stroke,
            ));
        }
        Icon::Home => {
            painter.add(egui::Shape::line(
                vec![
                    center + Vec2::new(-7.0, -1.0),
                    center + Vec2::new(0.0, -6.0),
                    center + Vec2::new(7.0, -1.0),
                ],
                stroke,
            ));
            painter.add(egui::Shape::line(
                vec![
                    center + Vec2::new(-5.0, 0.0),
                    center + Vec2::new(-5.0, 6.0),
                    center + Vec2::new(5.0, 6.0),
                    center + Vec2::new(5.0, 0.0),
                ],
                stroke,
            ));
        }
        _ => {
            painter.rect_stroke(rect, 1, stroke, egui::StrokeKind::Inside);
            match icon {
                Icon::Image => {
                    painter.circle_filled(center + Vec2::new(3.0, -3.0), 1.3, color);
                    painter.add(egui::Shape::line(
                        [(-6.0, 4.0), (-2.0, -1.0), (2.0, 4.0), (5.0, 1.0)]
                            .map(|(x, y)| center + Vec2::new(x, y))
                            .to_vec(),
                        stroke,
                    ));
                }
                Icon::Archive => {
                    for y in [-4.0, 0.0, 4.0] {
                        painter.line_segment(
                            [center + Vec2::new(-1.0, y), center + Vec2::new(2.0, y)],
                            stroke,
                        );
                    }
                }
                Icon::Drive => {
                    painter.line_segment(
                        [
                            rect.left_bottom() - Vec2::new(0.0, 4.0),
                            rect.right_bottom() - Vec2::new(0.0, 4.0),
                        ],
                        stroke,
                    );
                    painter.circle_filled(rect.right_bottom() - Vec2::new(3.0, 2.0), 0.8, color);
                }
                Icon::SplitRight => {
                    painter.line_segment([rect.center_top(), rect.center_bottom()], stroke);
                }
                Icon::SplitBelow => {
                    painter.line_segment([rect.left_center(), rect.right_center()], stroke);
                }
                _ => {
                    painter.line_segment(
                        [center + Vec2::new(-3.0, 0.0), center + Vec2::new(3.0, 0.0)],
                        stroke,
                    );
                }
            }
        }
    }
}

fn icon_button(ui: &mut egui::Ui, label: &str, icon: Icon, color: Color32) -> egui::Response {
    let galley = ui.painter().layout(
        label.to_owned(),
        egui::FontId::proportional(14.0),
        ui.visuals().text_color(),
        f32::INFINITY,
    );
    let width = (galley.size().x + 36.0).min(ui.available_width());
    let response = ui.add_sized(
        [width, 25.0],
        egui::Button::new("")
            .frame(false)
            .sense(Sense::click_and_drag()),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    paint_icon(
        ui.painter(),
        egui::pos2(response.rect.left() + 12.0, response.rect.center().y),
        icon,
        color,
    );
    ui.painter_at(response.rect).galley(
        egui::pos2(
            response.rect.left() + 28.0,
            response.rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        ui.visuals().text_color(),
    );
    response
}

fn tool_button(ui: &mut egui::Ui, icon: Icon, label: &str, size: [f32; 2]) -> egui::Response {
    let response = ui.add_sized(size, egui::Button::new("").frame(false));
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    paint_icon(
        ui.painter(),
        response.rect.center(),
        icon,
        ui.visuals().text_color(),
    );
    response.on_hover_text(label)
}

fn split_button(ui: &mut egui::Ui, vertical: bool) -> egui::Response {
    let label = if vertical {
        "上下分屏"
    } else {
        "左右分屏"
    };
    let response = ui.add_sized([26.0, 22.0], egui::Button::new("").frame(false));
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    paint_icon(
        ui.painter(),
        response.rect.center(),
        if vertical {
            Icon::SplitBelow
        } else {
            Icon::SplitRight
        },
        ui.visuals().weak_text_color(),
    );
    response.on_hover_text(label)
}

enum Action {
    Navigate(u64, Location),
    NewPane(Location),
    Split(u64, Split),
    Choose(u64),
    Open(PathBuf),
    SystemMenu(Vec<PathBuf>),
    Import(u64, Vec<PathBuf>),
    PickImport(u64, bool),
    PickDirectory,
    RestoreWorkspace,
    NewVirtual(u64, bool),
    RenameVirtual(u64),
    RemoveVirtual(u64),
    MoveVirtual(u64, u64),
    RenameReal(PathBuf),
    NewReal(PathBuf, bool),
    DeleteReal(Vec<PathBuf>),
    Copy(Vec<PathBuf>, bool),
    Paste(PathBuf),
    Transfer(Vec<PathBuf>, PathBuf, bool),
    Refresh,
    Search(u64, Location, String),
    StopSearch(u64),
    ExitSearch(u64),
}

#[derive(Clone)]
struct VirtualDrag(u64);

enum Event {
    Imported(u64, Result<Vec<(PathBuf, bool)>, String>),
    PickedDirectory(Option<PathBuf>),
    PickedWorkspace(Option<PathBuf>),
    PickedImport(u64, Vec<PathBuf>),
    Progress(String),
    Finished(Result<String, String>),
    Opened(Result<(), String>),
}

enum Edit {
    Virtual { parent: u64, file: bool },
    VirtualName(u64),
    RealName(PathBuf),
    RealNew { parent: PathBuf, directory: bool },
}
struct EditDialog {
    edit: Edit,
    name: String,
    error: String,
    focus: bool,
}

pub struct FolderApp {
    app_icon: egui::TextureHandle,
    workspace: Workspace,
    cache: DirectoryCache,
    searches: HashMap<u64, crate::search::Search>,
    actions: Vec<Action>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    status: String,
    error: bool,
    active: u64,
    sidebar_open: HashSet<u64>,
    revision: u64,
    chooser: Option<u64>,
    edit: Option<EditDialog>,
    delete: Option<Vec<PathBuf>>,
    busy: bool,
    config: PathBuf,
    writable: bool,
    _lock: Option<fs::File>,
    saved: Vec<u8>,
    save_error: Option<String>,
    last_save: Instant,
    help: bool,
    #[cfg(windows)]
    native_window: isize,
    #[cfg(windows)]
    release_native_drag: bool,
}

impl FolderApp {
    pub fn new(cc: &eframe::CreationContext<'_>, base: PathBuf) -> Self {
        configure(&cc.egui_ctx);
        #[cfg(windows)]
        let native_window = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
                _ => 0,
            }
        };
        let config = base.join("workspace.json");
        let lock = fs::create_dir_all(&base).and_then(|_| {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(base.join("workspace.lock"))
        });
        let (lock, lock_error) = match lock {
            Ok(file) => match file.try_lock() {
                Ok(()) => (Some(file), None),
                Err(e) => (
                    None,
                    Some(format!("工作区只读：另一实例正在使用，或无法锁定配置。{e}")),
                ),
            },
            Err(e) => (None, Some(format!("无法访问配置目录：{e}"))),
        };
        let loaded = Workspace::load(&config);
        let load_error = loaded.as_ref().err().map(|e| {
            format!(
                "配置读取失败，已禁用保存以保护原文件：{e}。配置：{}",
                display_path(&config)
            )
        });
        let workspace = loaded.unwrap_or_default();
        workspace.theme.apply(&cc.egui_ctx);
        let error = lock_error.or(load_error);
        let (tx, rx) = mpsc::channel();
        let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/app-icon.png"))
            .expect("embedded application icon");
        Self {
            app_icon: cc.egui_ctx.load_texture(
                "app-icon",
                egui::ColorImage::from_rgba_unmultiplied(
                    [icon.width as usize, icon.height as usize],
                    &icon.rgba,
                ),
                egui::TextureOptions::LINEAR,
            ),
            workspace,
            cache: DirectoryCache::new(cc.egui_ctx.clone()),
            searches: HashMap::new(),
            actions: vec![],
            tx,
            rx,
            status: error
                .clone()
                .unwrap_or_else(|| "就绪 · 将真实文件拖入虚拟节点，即可跨项目组织".into()),
            error: error.is_some(),
            active: 1,
            sidebar_open: HashSet::from([0]),
            revision: 1,
            chooser: None,
            edit: None,
            delete: None,
            busy: false,
            config,
            writable: error.is_none(),
            _lock: lock,
            saved: vec![],
            save_error: None,
            last_save: Instant::now(),
            help: false,
            #[cfg(windows)]
            native_window,
            #[cfg(windows)]
            release_native_drag: false,
        }
    }

    fn message(&mut self, result: Result<String, String>) {
        self.error = result.is_err();
        self.status = result.unwrap_or_else(|e| e);
    }

    fn poll(&mut self) {
        self.cache.poll();
        self.searches.retain(|id, search| {
            let Some((_, pane)) = self
                .workspace
                .dock
                .iter_all_tabs_mut()
                .find(|(_, p)| p.id == *id)
            else {
                return false;
            };
            if pane.location != search.location {
                return false;
            }
            for entry in search.poll() {
                pane.rows.push(Row {
                    key: format!("p:{}", entry.path.display()),
                    name: entry.name,
                    depth: 0,
                    target: Location::Disk(entry.path.clone()),
                    directory: entry.directory,
                    virtual_file: false,
                    detail: display_path(&entry.path),
                });
            }
            true
        });
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Imported(id, result) => {
                    let result = result.and_then(|paths| {
                        self.workspace
                            .root
                            .find_mut(id)
                            .ok_or_else(|| "虚拟节点已被移除".to_string())?
                            .add_paths(paths, &mut self.workspace.next_id)
                    });
                    self.revision += 1;
                    self.message(result.map(|n| format!("已添加 {n} 个路径引用；真实文件未移动")));
                }
                Event::PickedDirectory(Some(path)) => {
                    self.actions.push(Action::NewPane(Location::Disk(path)))
                }
                Event::PickedDirectory(None) => {}
                Event::PickedWorkspace(Some(path)) => {
                    if !self.writable || self.busy {
                        self.message(Err("当前工作区只读或有文件操作进行中，无法恢复。".into()));
                        continue;
                    }
                    match Workspace::restore(&path, &self.config) {
                        Ok((workspace, backup)) => {
                            self.workspace = workspace;
                            self.saved.clear();
                            self.save_error = None;
                            self.searches.clear();
                            self.cache.refresh();
                            self.sidebar_open = HashSet::from([0]);
                            self.revision += 1;
                            self.help = false;
                            self.chooser = None;
                            self.edit = None;
                            self.delete = None;
                            self.message(Ok(format!(
                                "工作区已恢复并保存。原配置备份：{}",
                                backup
                                    .as_deref()
                                    .map(display_path)
                                    .unwrap_or_else(|| "无原配置".into())
                            )));
                        }
                        Err(error) => {
                            self.message(Err(format!("恢复失败，当前工作区未切换：{error}")))
                        }
                    }
                }
                Event::PickedWorkspace(None) => {}
                Event::PickedImport(id, paths) => self.actions.push(Action::Import(id, paths)),
                Event::Progress(message) => self.status = message,
                Event::Finished(result) => {
                    self.busy = false;
                    self.cache.refresh();
                    self.message(result);
                }
                Event::Opened(result) => {
                    if let Err(e) = result {
                        self.message(Err(e));
                    }
                }
            }
        }
    }

    fn operation(&mut self, ctx: &egui::Context, operation: Operation) {
        if self.busy {
            self.message(Err("已有文件操作正在执行，请稍候。".into()));
            return;
        }
        self.busy = true;
        self.status = "正在处理文件…".into();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let mut last = Instant::now();
            let result = files::operate(operation, |message| {
                if last.elapsed() > Duration::from_millis(100) {
                    let _ = tx.send(Event::Progress(message));
                    ctx.request_repaint();
                    last = Instant::now();
                }
            });
            let _ = tx.send(Event::Finished(result));
            ctx.request_repaint();
        });
    }

    fn act(&mut self, ctx: &egui::Context) {
        for action in std::mem::take(&mut self.actions) {
            match action {
                Action::Navigate(id, mut location) => {
                    if let Location::Virtual(node_id) = location
                        && let Some(Node {
                            kind: Kind::Link { path, directory },
                            ..
                        }) = self.workspace.root.find(node_id)
                    {
                        if *directory {
                            location = Location::Disk(path.clone());
                        } else {
                            self.actions.push(Action::Open(path.clone()));
                            ctx.request_repaint();
                            continue;
                        }
                    }
                    if let Some((_, pane)) = self
                        .workspace
                        .dock
                        .iter_all_tabs_mut()
                        .find(|(_, p)| p.id == id)
                    {
                        pane.navigate(location, true);
                    }
                }
                Action::NewPane(location) => {
                    let id = self.workspace.next_id;
                    self.workspace.next_id += 1;
                    self.workspace
                        .dock
                        .push_to_focused_leaf(Pane::new(id, location));
                    self.active = id;
                }
                Action::Split(id, split) => {
                    let found = self
                        .workspace
                        .dock
                        .iter_all_tabs()
                        .find(|(_, p)| p.id == id)
                        .map(|(path, p)| (path, p.clone()));
                    if let Some((path, pane)) = found {
                        let next = self.workspace.next_id;
                        self.workspace.next_id += 1;
                        self.workspace.dock.split(
                            NodePath {
                                surface: path.surface,
                                node: path.node,
                            },
                            split,
                            0.5,
                            egui_dock::Node::leaf(Pane::new(next, pane.location)),
                        );
                    }
                }
                Action::Choose(id) => self.chooser = Some(id),
                Action::SystemMenu(paths) => {
                    if let Err(error) = crate::shell_menu::show(&paths) {
                        self.message(Err(error));
                    }
                    self.cache.refresh();
                }
                Action::Open(path) => {
                    let tx = self.tx.clone();
                    let ctx = ctx.clone();
                    thread::spawn(move || {
                        let result = crate::shell_menu::open_path(&path)
                            .map_err(|e| format!("无法打开 {}：{e}", display_path(&path)));
                        let _ = tx.send(Event::Opened(result));
                        ctx.request_repaint();
                    });
                }
                Action::Import(id, paths) => {
                    if paths.is_empty() {
                        continue;
                    }
                    let tx = self.tx.clone();
                    let ctx = ctx.clone();
                    thread::spawn(move || {
                        let result = paths
                            .into_iter()
                            .map(|path| {
                                let canonical = fs::canonicalize(&path).map_err(|e| {
                                    format!("无法添加 {}：{e}", display_path(&path))
                                })?;
                                let directory =
                                    canonical.metadata().map_err(|e| e.to_string())?.is_dir();
                                Ok((canonical, directory))
                            })
                            .collect();
                        let _ = tx.send(Event::Imported(id, result));
                        ctx.request_repaint();
                    });
                }
                Action::PickImport(id, folders) => {
                    let tx = self.tx.clone();
                    let ctx = ctx.clone();
                    thread::spawn(move || {
                        let dialog = rfd::FileDialog::new().set_title("添加真实路径引用");
                        let paths = if folders {
                            dialog.pick_folders()
                        } else {
                            dialog.pick_files()
                        }
                        .unwrap_or_default();
                        let _ = tx.send(Event::PickedImport(id, paths));
                        ctx.request_repaint();
                    });
                }
                Action::PickDirectory => {
                    let tx = self.tx.clone();
                    let ctx = ctx.clone();
                    thread::spawn(move || {
                        let path = rfd::FileDialog::new().set_title("打开目录").pick_folder();
                        let _ = tx.send(Event::PickedDirectory(path));
                        ctx.request_repaint();
                    });
                }
                Action::RestoreWorkspace => {
                    let tx = self.tx.clone();
                    let ctx = ctx.clone();
                    thread::spawn(move || {
                        let path = rfd::FileDialog::new()
                            .set_title("从备份恢复工作区")
                            .add_filter("工作区 JSON", &["json"])
                            .pick_file();
                        let _ = tx.send(Event::PickedWorkspace(path));
                        ctx.request_repaint();
                    });
                }
                Action::NewVirtual(parent, file) => {
                    self.edit = Some(EditDialog {
                        edit: Edit::Virtual { parent, file },
                        name: String::new(),
                        error: String::new(),
                        focus: true,
                    })
                }
                Action::RenameVirtual(id) => {
                    if let Some(node) = self.workspace.root.find(id) {
                        self.edit = Some(EditDialog {
                            edit: Edit::VirtualName(id),
                            name: node.name.clone(),
                            error: String::new(),
                            focus: true,
                        });
                    }
                }
                Action::RemoveVirtual(id) => {
                    self.workspace.root.remove(id);
                    self.revision += 1;
                    self.message(Ok("已移除虚拟引用；真实文件未删除".into()));
                }
                Action::MoveVirtual(id, destination) => {
                    if matches!(
                        self.workspace.root.find(destination).map(|n| &n.kind),
                        Some(Kind::File(_))
                    ) && let Some(Node {
                        kind: Kind::Link { path, .. },
                        ..
                    }) = self.workspace.root.find(id)
                    {
                        self.actions
                            .push(Action::Import(destination, vec![path.clone()]));
                        ctx.request_repaint();
                        continue;
                    }
                    let result = self.workspace.root.reparent(id, destination);
                    if result.is_ok() {
                        self.sidebar_open.insert(destination);
                        self.revision += 1;
                    }
                    self.message(result.map(|()| "已重新组织虚拟节点；真实文件未移动".into()));
                }
                Action::RenameReal(source) => {
                    self.edit = Some(EditDialog {
                        name: path_name(&source),
                        edit: Edit::RealName(source),
                        error: String::new(),
                        focus: true,
                    })
                }
                Action::NewReal(parent, directory) => {
                    self.edit = Some(EditDialog {
                        edit: Edit::RealNew { parent, directory },
                        name: String::new(),
                        error: String::new(),
                        focus: true,
                    })
                }
                Action::DeleteReal(paths) => {
                    if !paths.is_empty() {
                        self.delete = Some(paths);
                    }
                }
                Action::Copy(paths, moving) => {
                    self.message(crate::clipboard::write(&paths, moving).map(|()| {
                        format!(
                            "已{} {} 项，可在本程序或资源管理器中粘贴",
                            if moving { "剪切" } else { "复制" },
                            paths.len()
                        )
                    }));
                }
                Action::Paste(destination) => {
                    self.operation(ctx, Operation::PasteClipboard(destination));
                }
                Action::Transfer(sources, destination, moving) => {
                    self.operation(
                        ctx,
                        Operation::Copy {
                            sources,
                            destination,
                            moving,
                        },
                    );
                }
                Action::Refresh => self.cache.refresh(),
                Action::Search(id, location, query) => {
                    if query.trim().is_empty() {
                        self.message(Err("请输入需要递归查找的名称。".into()));
                        continue;
                    }
                    let roots = match &location {
                        Location::Disk(path) => vec![path.clone()],
                        Location::Virtual(id) => self
                            .workspace
                            .root
                            .find(*id)
                            .map(Node::real_paths)
                            .unwrap_or_default(),
                    };
                    if let Some((_, pane)) = self
                        .workspace
                        .dock
                        .iter_all_tabs_mut()
                        .find(|(_, p)| p.id == id)
                    {
                        pane.rows.clear();
                        pane.selected.clear();
                        pane.row_stamp.clear();
                    }
                    self.searches.insert(
                        id,
                        crate::search::Search::start(location, roots, query, ctx.clone()),
                    );
                }
                Action::StopSearch(id) => {
                    if let Some(search) = self.searches.get(&id) {
                        search.cancel();
                    }
                }
                Action::ExitSearch(id) => {
                    self.searches.remove(&id);
                    if let Some((_, pane)) = self
                        .workspace
                        .dock
                        .iter_all_tabs_mut()
                        .find(|(_, p)| p.id == id)
                    {
                        pane.row_stamp.clear();
                        pane.filter.clear();
                        pane.search_deadline = None;
                    }
                }
            }
        }
    }

    fn persist(&mut self) {
        if !self.writable {
            return;
        }
        let result = (|| {
            let bytes = serde_json::to_vec(&self.workspace).map_err(|e| e.to_string())?;
            if bytes != self.saved {
                self.workspace.save(&self.config)?;
                self.saved = bytes;
            }
            Ok::<_, String>(())
        })();
        self.save_error = result.err().map(|e| format!("工作区未保存：{e}"));
        self.last_save = Instant::now();
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(mut dialog) = self.edit.take() {
            let mut keep = true;
            let mut apply = false;
            egui::Window::new("名称")
                .id(Id::new("edit-name"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.set_min_width(370.0);
                    ui.label(match &dialog.edit {
                        Edit::Virtual { file: true, .. } => "新建虚拟文件 · 一个名字，多个项目版本",
                        Edit::Virtual { .. } => "新建虚拟文件夹 · 只组织引用",
                        Edit::VirtualName(_) => "重命名虚拟节点",
                        Edit::RealName(_) => "重命名真实文件 / 文件夹",
                        Edit::RealNew {
                            directory: true, ..
                        } => "新建真实文件夹",
                        Edit::RealNew { .. } => "新建空文件",
                    });
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut dialog.name).desired_width(f32::INFINITY),
                    );
                    if dialog.focus {
                        edit.request_focus();
                        dialog.focus = false;
                    }
                    if !dialog.error.is_empty() {
                        ui.colored_label(Color32::LIGHT_RED, &dialog.error);
                    }
                    ui.horizontal(|ui| {
                        apply = ui.button("确定").clicked()
                            || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                        if ui.button("取消").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Escape))
                        {
                            keep = false;
                        }
                    });
                });
            if apply {
                let virtual_name =
                    matches!(dialog.edit, Edit::Virtual { .. } | Edit::VirtualName(_));
                let validation = if virtual_name {
                    if dialog.name.trim().is_empty() {
                        Err("名称不能为空".into())
                    } else {
                        Ok(())
                    }
                } else {
                    files::validate_name(&dialog.name)
                };
                match validation {
                    Err(e) => dialog.error = e,
                    Ok(()) => {
                        match &dialog.edit {
                            Edit::Virtual { parent, file } => {
                                if let Some(Node {
                                    kind: Kind::Folder(children),
                                    ..
                                }) = self.workspace.root.find_mut(*parent)
                                {
                                    let id = self.workspace.next_id;
                                    self.workspace.next_id += 1;
                                    children.push(Node {
                                        id,
                                        name: dialog.name.trim().into(),
                                        kind: if *file {
                                            Kind::File(vec![])
                                        } else {
                                            Kind::Folder(vec![])
                                        },
                                    });
                                    self.sidebar_open.insert(*parent);
                                    self.revision += 1;
                                }
                            }
                            Edit::VirtualName(id) => {
                                if let Some(node) = self.workspace.root.find_mut(*id) {
                                    node.name = dialog.name.trim().into();
                                    self.revision += 1;
                                }
                            }
                            Edit::RealName(path) => self.operation(
                                ctx,
                                Operation::Rename {
                                    source: path.clone(),
                                    name: dialog.name.clone(),
                                },
                            ),
                            Edit::RealNew { parent, directory } => self.operation(
                                ctx,
                                Operation::Create {
                                    parent: parent.clone(),
                                    name: dialog.name.clone(),
                                    directory: *directory,
                                },
                            ),
                        }
                        keep = false;
                    }
                }
            }
            if keep {
                self.edit = Some(dialog);
            }
        }
        if let Some(paths) = self.delete.clone() {
            let mut open = true;
            let mut confirmed = false;
            egui::Window::new("移入回收站")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "将 {} 个真实文件 / 文件夹移入回收站？",
                        paths.len()
                    ));
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            for path in &paths {
                                ui.label(display_path(path));
                            }
                        });
                    confirmed = ui
                        .add_enabled(!self.busy, egui::Button::new("移入回收站"))
                        .clicked();
                });
            if confirmed {
                self.operation(ctx, Operation::Trash(paths));
                self.delete = None;
            } else if !open {
                self.delete = None;
            }
        }
        if let Some(id) = self.chooser {
            if let Some(Node {
                name,
                kind: Kind::File(paths),
                ..
            }) = self.workspace.root.find(id).cloned()
            {
                let mut open = true;
                let mut remove = None;
                egui::Window::new(format!("选择项目版本 · {name}"))
                    .id(Id::new("mapping-chooser"))
                    .open(&mut open)
                    .default_width(670.0)
                    .collapsible(false)
                    .show(ctx, |ui| {
                        ui.label(
                            RichText::new("每个路径是一个独立版本。选择打开文件，或进入所在目录。")
                                .color(ui.visuals().weak_text_color()),
                        );
                        if paths.is_empty() {
                            ui.add_space(15.0);
                            ui.label("还没有映射。拖入文件，或点击下面的“添加文件”。");
                        }
                        egui::ScrollArea::vertical()
                            .max_height(400.0)
                            .show(ui, |ui| {
                                for (index, path) in paths.iter().enumerate() {
                                    egui::Frame::group(ui.style()).show(ui, |ui| {
                                        ui.label(RichText::new(path_name(path)).strong());
                                        ui.label(
                                            RichText::new(display_path(path))
                                                .color(ui.visuals().weak_text_color()),
                                        );
                                        ui.horizontal(|ui| {
                                            if ui.button("打开文件").clicked() {
                                                self.actions.push(Action::Open(path.clone()));
                                            }
                                            if ui.button("所在目录").clicked()
                                                && let Some(parent) = path.parent()
                                            {
                                                self.actions.push(Action::NewPane(Location::Disk(
                                                    parent.to_owned(),
                                                )));
                                            }
                                            if ui.small_button("移除映射").clicked() {
                                                remove = Some(index);
                                            }
                                        });
                                    });
                                }
                            });
                        if ui.button("＋ 添加文件").clicked() {
                            self.actions.push(Action::PickImport(id, false));
                        }
                        drop_target(ui, id, &mut self.actions);
                    });
                if let Some(index) = remove
                    && let Some(Node {
                        kind: Kind::File(paths),
                        ..
                    }) = self.workspace.root.find_mut(id)
                {
                    paths.remove(index);
                    self.revision += 1;
                }
                if !open {
                    self.chooser = None;
                }
            } else {
                self.chooser = None;
            }
        }
        if self.help {
            egui::Window::new("使用说明")
                .open(&mut self.help)
                .default_width(580.0)
                .show(ctx, |ui| {
                    ui.heading("把项目放进工作区，而不是更多窗口");
                    for line in [
                        "1. 左侧新建虚拟文件夹，把不同分支的真实目录拖进去。",
                        "2. 新建虚拟文件，把多个分支的同名文件拖到该节点上。",
                        "3. 双击虚拟文件，选择打开哪个版本或进入其所在目录。",
                        "4. 拖动面板标签到边缘可停靠分屏；面板右上角也可继续拆分。",
                        "5. 点击箭头展开目录；双击目录进入。只加载已展开目录。",
                        "Ctrl+C / X / V：与 Windows 资源管理器互通的文件复制 / 剪切 / 粘贴",
                        "Ctrl+A：选中当前列表 · Ctrl/Shift+点击：多选",
                        "Enter：打开 · Alt+← / →：前进后退 · Backspace：上级",
                        "F2：重命名 · Delete：回收站确认 · F5：刷新",
                        "虚拟节点右键移除仅删除引用；不会删除真实文件。",
                        "输入名称后自动搜索子目录；Enter 立即搜索，清空搜索框返回列表。",
                        "右键真实项目 → Windows 系统右键菜单，可调用系统及扩展命令。",
                        "外部修改后按 F5 刷新目录。",
                        "文件复制和同盘 / 跨盘移动均不覆盖已有目标。",
                        "不加载 SVN/Git 状态、缩略图和预览；仅主动打开系统菜单时加载扩展。",
                    ] {
                        ui.label(line);
                    }
                    ui.separator();
                    ui.label(format!("工作区配置：{}", display_path(&self.config)));
                    if ui
                        .add_enabled(
                            self.writable && !self.busy,
                            egui::Button::new("从备份恢复工作区…"),
                        )
                        .clicked()
                    {
                        self.actions.push(Action::RestoreWorkspace);
                    }
                });
        }
    }
}

impl eframe::App for FolderApp {
    #[cfg(windows)]
    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        if std::mem::take(&mut self.release_native_drag) {
            input.events.push(egui::Event::PointerButton {
                pos: ctx.pointer_latest_pos().unwrap_or_default(),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: input.modifiers,
            });
            input.events.push(egui::Event::PointerGone);
        }
        if !input.hovered_files.is_empty() || !input.dropped_files.is_empty() {
            // winit's OLE handler supplies filenames but omits DragOver coordinates.
            if let Some((pos, inside)) =
                crate::drag_drop::cursor(self.native_window, ctx.pixels_per_point())
            {
                input.events.push(if inside {
                    egui::Event::PointerMoved(pos)
                } else {
                    egui::Event::PointerGone
                });
            }
            let (_, shift, ctrl) = crate::drag_drop::buttons();
            input.modifiers.shift = shift;
            input.modifiers.ctrl = ctrl;
            input.modifiers.command = ctrl;
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        ctx.data_mut(|d| d.remove::<String>(Id::new("disk-drop-hint")));
        if ctx.data(|d| d.get_temp::<crate::theme::Theme>(Id::new("applied-theme")))
            != Some(self.workspace.theme)
        {
            self.workspace.theme.apply(&ctx);
            ctx.data_mut(|d| d.insert_temp(Id::new("applied-theme"), self.workspace.theme));
        }
        if self.busy && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.status = "文件操作进行中，请完成后关闭窗口。".into();
        }
        egui::Panel::top("top").exact_size(36.0).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.image((self.app_icon.id(), Vec2::new(20.0, 20.0)));
                let title = ui.add(
                    egui::Label::new(RichText::new("NKG FOLDER").size(13.0))
                        .sense(Sense::click_and_drag()),
                );
                drag_title_bar(&title);
                ui.add_space(12.0);
                if ui.add(egui::Button::new("打开目录").frame(false)).clicked() {
                    self.actions.push(Action::PickDirectory);
                }
                if ui
                    .add(egui::Button::new("＋ 工作区面板").frame(false))
                    .clicked()
                {
                    self.actions.push(Action::NewPane(Location::Virtual(0)));
                }
                if ui.add(egui::Button::new("刷新  F5").frame(false)).clicked() {
                    self.actions.push(Action::Refresh);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let hover = ui.visuals().widgets.hovered.weak_bg_fill;
                    ui.visuals_mut().widgets.hovered.weak_bg_fill = Color32::from_rgb(196, 43, 28);
                    if tool_button(ui, Icon::Close, "关闭", [42.0, 28.0]).clicked() {
                        if self.busy {
                            self.status = "文件操作进行中，请完成后关闭窗口。".into();
                        } else {
                            self.persist();
                            if self.save_error.is_none() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                    }
                    ui.visuals_mut().widgets.hovered.weak_bg_fill = hover;
                    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    if tool_button(
                        ui,
                        if maximized {
                            Icon::Restore
                        } else {
                            Icon::Maximize
                        },
                        if maximized { "还原" } else { "最大化" },
                        [42.0, 28.0],
                    )
                    .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                    }
                    if tool_button(ui, Icon::Minimize, "最小化", [42.0, 28.0]).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                    if ui.add(egui::Button::new("帮助  ?").frame(false)).clicked() {
                        self.help = true;
                    }
                    ui.add_space(8.0);
                    let previous = self.workspace.theme;
                    egui::ComboBox::from_id_salt("theme")
                        .selected_text(self.workspace.theme.name())
                        .width(150.0)
                        .show_ui(ui, |ui| {
                            for theme in crate::theme::Theme::ALL {
                                ui.selectable_value(&mut self.workspace.theme, theme, theme.name());
                            }
                        });
                    if previous != self.workspace.theme {
                        self.workspace.theme.apply(&ctx);
                    }
                    let drag = ui.allocate_response(ui.available_size(), Sense::click_and_drag());
                    drag_title_bar(&drag);
                });
            });
        });
        if let Some(error) = &self.save_error {
            egui::Panel::top("save-error").show(ui, |ui| {
                ui.colored_label(Color32::LIGHT_RED, error);
                ui.label(format!("配置：{}", display_path(&self.config)));
            });
        }
        egui::Panel::bottom("status")
            .exact_size(27.0)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.busy {
                        ui.spinner();
                    }
                    ui.add(
                        egui::Label::new(RichText::new(&self.status).size(12.0).color(
                            if self.error {
                                Color32::LIGHT_RED
                            } else {
                                ui.visuals().weak_text_color()
                            },
                        ))
                        .truncate(),
                    )
                    .on_hover_text(&self.status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!(
                            "{} 面板",
                            self.workspace.dock.iter_all_tabs().count()
                        ));
                        if !self.writable {
                            ui.colored_label(Color32::YELLOW, "配置只读");
                        }
                    });
                });
            });
        egui::Panel::left("library")
            .default_size(250.0)
            .min_size(190.0)
            .resizable(true)
            .show(ui, |ui| {
                ui.add_space(9.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("虚拟资源").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if tool_button(ui, Icon::NewFile, "新建虚拟文件", [25.0, 25.0]).clicked()
                        {
                            self.actions.push(Action::NewVirtual(0, true));
                        }
                        if tool_button(ui, Icon::NewFolder, "新建虚拟文件夹", [25.0, 25.0])
                            .clicked()
                        {
                            self.actions.push(Action::NewVirtual(0, false));
                        }
                    });
                });
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .id_salt("library-scroll")
                    .show(ui, |ui| {
                        library_node(
                            ui,
                            &self.workspace.root,
                            0,
                            &mut self.sidebar_open,
                            &mut self.actions,
                        );
                    });
                ui.add_space(12.0);
                drop_target(ui, 0, &mut self.actions);
                ui.separator();
                ui.label(
                    RichText::new("磁盘与位置")
                        .size(12.0)
                        .color(ui.visuals().weak_text_color()),
                );
                if let Some(home) = std::env::var_os("USERPROFILE")
                    && icon_button(ui, "用户目录", Icon::Home, ui.visuals().weak_text_color())
                        .clicked()
                {
                    self.actions
                        .push(Action::NewPane(Location::Disk(home.into())));
                }
                // GetLogicalDrives avoids probing disconnected/network drives on the UI thread.
                #[cfg(windows)]
                for letter in drive_letters() {
                    if icon_button(
                        ui,
                        &format!("{letter}:/"),
                        Icon::Drive,
                        ui.visuals().weak_text_color(),
                    )
                    .clicked()
                    {
                        self.actions
                            .push(Action::NewPane(Location::Disk(PathBuf::from(format!(
                                "{letter}:/"
                            )))));
                    }
                }
            });
        let modal =
            self.edit.is_some() || self.delete.is_some() || self.chooser.is_some() || self.help;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let mut viewer = Viewer {
                    root: &self.workspace.root,
                    cache: &mut self.cache,
                    searches: &self.searches,
                    revision: self.revision,
                    actions: &mut self.actions,
                    active: &mut self.active,
                    modal,
                };
                let mut style = egui_dock::Style::from_egui(ui.style().as_ref());
                style.tab_bar.height = 32.0;
                style.tab_bar.bg_fill = ui.visuals().panel_fill;
                style.tab.tab_body.bg_fill = ui.visuals().window_fill;
                style.tab.tab_body.inner_margin = egui::Margin::same(8);
                style.separator.width = 4.0;
                style.separator.color_idle = ui.visuals().panel_fill;
                style.separator.color_hovered = ui.visuals().hyperlink_color;
                DockArea::new(&mut self.workspace.dock)
                    .style(style)
                    .show_inside(ui, &mut viewer);
            });
        if !modal && ctx.input(|i| i.key_pressed(egui::Key::F5)) {
            self.cache.refresh();
        }
        if ctx.input(|i| !i.raw.dropped_files.is_empty()) {
            ctx.input_mut(|i| i.raw.dropped_files.clear());
            self.message(Err(
                "未找到接收目录，请拖到文件列表或虚拟资源节点上。".into()
            ));
        }
        self.dialogs(&ctx);
        self.act(&ctx);
        let drag_label = ctx
            .data(|d| d.get_temp::<String>(Id::new("disk-drop-hint")))
            .or_else(|| {
                egui::DragAndDrop::payload::<Vec<PathBuf>>(&ctx).map(|paths| {
                    format!(
                        "拖动 {} 项 · 真实目录复制 / Shift 移动 · 虚拟目录添加引用",
                        paths.len()
                    )
                })
            })
            .or_else(|| {
                egui::DragAndDrop::payload::<VirtualDrag>(&ctx).map(|_| "移动虚拟节点".to_string())
            });
        if let Some(label) = drag_label
            && let Some(pointer) = ctx.pointer_latest_pos()
        {
            egui::Area::new(Id::new("drag-hint"))
                .order(egui::Order::Tooltip)
                .interactable(false)
                .fixed_pos(pointer + Vec2::new(16.0, 18.0))
                .show(&ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(label);
                    });
                });
        }
        #[cfg(windows)]
        if !self.busy {
            let paths = egui::DragAndDrop::payload::<Vec<PathBuf>>(&ctx)
                .map(|p| (*p).clone())
                .or_else(|| {
                    egui::DragAndDrop::payload::<VirtualDrag>(&ctx)
                        .and_then(|p| self.workspace.root.find(p.0))
                        .map(Node::real_paths)
                });
            if let Some(paths) = paths.filter(|p| !p.is_empty()) {
                ctx.request_repaint_after(Duration::from_millis(16));
                if let Some((_, false)) =
                    crate::drag_drop::cursor(self.native_window, ctx.pixels_per_point())
                    && crate::drag_drop::buttons().0
                {
                    egui::DragAndDrop::clear_payload(&ctx);
                    self.message(crate::drag_drop::drag_out(self.native_window, &paths));
                    self.release_native_drag = true;
                    self.cache.refresh();
                    ctx.request_repaint();
                }
            }
        }
        let closing = ctx.input(|i| i.viewport().close_requested());
        if closing || self.last_save.elapsed() > Duration::from_secs(2) {
            self.persist();
        }
        if closing && self.save_error.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        ctx.request_repaint_after(Duration::from_secs(2));
        resize_window_edges(ui);
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist();
    }
}

fn drag_title_bar(response: &egui::Response) {
    if response.double_clicked() {
        let maximized = response
            .ctx
            .input(|i| i.viewport().maximized.unwrap_or(false));
        response
            .ctx
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    } else if response.drag_started_by(egui::PointerButton::Primary) {
        response
            .ctx
            .send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
}

fn resize_window_edges(ui: &mut egui::Ui) {
    if ui.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }
    let rect = ui.ctx().viewport_rect();
    use egui::{CursorIcon as C, ResizeDirection as D};
    for (x, y, direction, cursor) in [
        (-1, -1, D::NorthWest, C::ResizeNwSe),
        (0, -1, D::North, C::ResizeVertical),
        (1, -1, D::NorthEast, C::ResizeNeSw),
        (-1, 0, D::West, C::ResizeHorizontal),
        (1, 0, D::East, C::ResizeHorizontal),
        (-1, 1, D::SouthWest, C::ResizeNeSw),
        (0, 1, D::South, C::ResizeVertical),
        (1, 1, D::SouthEast, C::ResizeNwSe),
    ] {
        let span = |min: f32, max: f32, side| match side {
            -1 => (min, min + 5.0),
            1 => (max - 5.0, max),
            _ => (min + 5.0, max - 5.0),
        };
        let (left, right) = span(rect.left(), rect.right(), x);
        let (top, bottom) = span(rect.top(), rect.bottom(), y);
        let response = ui
            .interact(
                egui::Rect::from_min_max(egui::pos2(left, top), egui::pos2(right, bottom)),
                Id::new(("window-resize", x, y)),
                Sense::drag(),
            )
            .on_hover_cursor(cursor);
        if response.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}

fn configure(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in ["C:/Windows/Fonts/msyh.ttc", "C:/Windows/Fonts/simhei.ttf"] {
        if let Ok(bytes) = fs::read(path) {
            fonts
                .font_data
                .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("cjk".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".into());
            break;
        }
    }
    if let Ok(bytes) = fs::read("C:/Windows/Fonts/segoeui.ttf") {
        fonts
            .font_data
            .insert("segoe".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "segoe".into());
    }
    ctx.set_fonts(fonts);
}

#[cfg(windows)]
fn drive_letters() -> Vec<char> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| (b'A' + i) as char)
        .collect()
}

fn accept_drop(response: &egui::Response, id: u64, actions: &mut Vec<Action>) {
    if response.contains_pointer() {
        let paths: Vec<_> = response.ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !paths.is_empty() {
            actions.push(Action::Import(id, paths));
            response.ctx.input_mut(|i| i.raw.dropped_files.clear());
        }
    }
    if response.dnd_hover_payload::<VirtualDrag>().is_some() {
        if let Some(node) = response.dnd_release_payload::<VirtualDrag>() {
            actions.push(Action::MoveVirtual(node.0, id));
        }
    } else if let Some(paths) = response.dnd_release_payload::<Vec<PathBuf>>() {
        actions.push(Action::Import(id, (*paths).clone()));
    }
    if response.dnd_hover_payload::<Vec<PathBuf>>().is_some()
        || response.dnd_hover_payload::<VirtualDrag>().is_some()
        || (response.contains_pointer() && response.ctx.input(|i| !i.raw.hovered_files.is_empty()))
    {
        response.ctx.layer_painter(response.layer_id).rect_stroke(
            response.rect,
            2,
            egui::Stroke::new(
                1.0,
                response
                    .ctx
                    .style_of(response.ctx.theme())
                    .visuals
                    .hyperlink_color,
            ),
            egui::StrokeKind::Inside,
        );
    }
}

fn accept_disk_drop(response: &egui::Response, destination: &Path, actions: &mut Vec<Action>) {
    if !response.contains_pointer() {
        return;
    }
    let moving = response
        .ctx
        .input(|i| i.modifiers.shift && !i.modifiers.ctrl);
    if response.dnd_hover_payload::<Vec<PathBuf>>().is_some()
        || response.ctx.input(|i| !i.raw.hovered_files.is_empty())
    {
        let hint = format!(
            "{}到 {}",
            if moving { "移动" } else { "复制" },
            display_path(destination)
        );
        response.ctx.data_mut(|d| {
            let id = Id::new("disk-drop-hint");
            if d.get_temp::<String>(id).is_none() {
                d.insert_temp(id, hint);
            }
        });
        response.ctx.layer_painter(response.layer_id).rect_stroke(
            response.rect,
            2,
            egui::Stroke::new(
                1.0,
                response
                    .ctx
                    .style_of(response.ctx.theme())
                    .visuals
                    .hyperlink_color,
            ),
            egui::StrokeKind::Inside,
        );
    }
    let paths: Vec<_> = response.ctx.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .filter_map(|f| f.path.clone())
            .collect()
    });
    if !paths.is_empty() {
        response.ctx.input_mut(|i| i.raw.dropped_files.clear());
        actions.push(Action::Transfer(paths, destination.into(), moving));
    } else if let Some(paths) = response.dnd_release_payload::<Vec<PathBuf>>() {
        actions.push(Action::Transfer(
            (*paths).clone(),
            destination.into(),
            moving,
        ));
    }
}

fn drop_target(ui: &mut egui::Ui, id: u64, actions: &mut Vec<Action>) {
    let response = ui.add_sized(
        [ui.available_width(), 48.0],
        egui::Button::new(
            RichText::new("＋ 拖入文件 / 文件夹\n仅添加路径引用")
                .size(12.0)
                .color(ui.visuals().weak_text_color()),
        )
        .sense(Sense::hover()),
    );
    accept_drop(&response, id, actions);
}

fn library_node(
    ui: &mut egui::Ui,
    node: &Node,
    depth: usize,
    expanded: &mut HashSet<u64>,
    actions: &mut Vec<Action>,
) {
    ui.push_id(node.id, |ui| {
        let folder = matches!(node.kind, Kind::Folder(_));
        let open = expanded.contains(&node.id);
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ROW_HEIGHT),
            Sense::click_and_drag(),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), &node.name)
        });
        let painter = ui.painter_at(rect);
        if response.hovered() {
            painter.rect_filled(rect, 0, ui.visuals().widgets.hovered.bg_fill);
        }
        // Reserve the same chevron/icon/text columns for folders and leaves.
        let x = rect.left() + 5.0 + depth as f32 * 16.0;
        let y = rect.center().y;
        let arrow_rect = egui::Rect::from_min_max(
            egui::pos2(x - 5.0, rect.top()),
            egui::pos2(x + 16.0, rect.bottom()),
        );
        if folder {
            paint_chevron(&painter, egui::pos2(x + 5.0, y), open);
            let arrow = ui.interact(arrow_rect, ui.id().with("expand"), Sense::click());
            arrow.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    ui.is_enabled(),
                    format!("{} {}", if open { "收起" } else { "展开" }, node.name),
                )
            });
            if arrow.clicked() && !expanded.insert(node.id) {
                expanded.remove(&node.id);
            }
        }
        let (icon, color) = match &node.kind {
            Kind::Folder(_)
            | Kind::Link {
                directory: true, ..
            } => (Icon::Folder, ui.visuals().text_color()),
            Kind::File(_) => (Icon::VirtualFile, ui.visuals().hyperlink_color),
            _ => (Icon::File, ui.visuals().weak_text_color()),
        };
        paint_icon(&painter, egui::pos2(x + 24.0, y), icon, color);
        painter.text(
            egui::pos2(x + 39.0, y),
            egui::Align2::LEFT_CENTER,
            &node.name,
            egui::FontId::proportional(14.0),
            ui.visuals().text_color(),
        );
        if response.double_clicked()
            && !response
                .interact_pointer_pos()
                .is_some_and(|p| folder && arrow_rect.contains(p))
        {
            actions.push(match &node.kind {
                Kind::File(_) => Action::Choose(node.id),
                Kind::Link {
                    path,
                    directory: false,
                } => Action::Open(path.clone()),
                Kind::Link {
                    path,
                    directory: true,
                } => Action::NewPane(Location::Disk(path.clone())),
                _ => Action::NewPane(Location::Virtual(node.id)),
            });
        }
        if node.id != 0 {
            response.dnd_set_drag_payload(VirtualDrag(node.id));
        }
        if !matches!(node.kind, Kind::Link { .. }) {
            accept_drop(&response, node.id, actions);
        }
        virtual_menu(&response, node, actions, || node.real_paths());
        if let Kind::Folder(children) = &node.kind
            && expanded.contains(&node.id)
        {
            for child in children {
                library_node(ui, child, depth + 1, expanded, actions);
            }
        }
    });
}

fn virtual_menu(
    response: &egui::Response,
    node: &Node,
    actions: &mut Vec<Action>,
    paths: impl FnOnce() -> Vec<PathBuf>,
) {
    response.context_menu(|ui| {
        copy_paths_menu(ui, &paths());
        if let Kind::Link { path, .. } = &node.kind
            && ui.button("Windows 系统右键菜单…").clicked()
        {
            actions.push(Action::SystemMenu(vec![path.clone()]));
            ui.close();
        }
        if matches!(node.kind, Kind::Folder(_)) {
            if ui.button("打开为面板").clicked() {
                actions.push(Action::NewPane(Location::Virtual(node.id)));
                ui.close();
            }
            if ui.button("新建虚拟文件夹").clicked() {
                actions.push(Action::NewVirtual(node.id, false));
                ui.close();
            }
            if ui.button("新建虚拟文件").clicked() {
                actions.push(Action::NewVirtual(node.id, true));
                ui.close();
            }
            if ui.button("添加真实文件夹…").clicked() {
                actions.push(Action::PickImport(node.id, true));
                ui.close();
            }
        }
        if !matches!(node.kind, Kind::Link { .. }) && ui.button("添加真实文件…").clicked() {
            actions.push(Action::PickImport(node.id, false));
            ui.close();
        }
        if matches!(node.kind, Kind::File(_)) && ui.button("选择映射版本…").clicked() {
            actions.push(Action::Choose(node.id));
            ui.close();
        }
        if node.id != 0 {
            if ui.button("重命名虚拟节点").clicked() {
                actions.push(Action::RenameVirtual(node.id));
                ui.close();
            }
            if ui.button("移除引用（保留真实文件）").clicked() {
                actions.push(Action::RemoveVirtual(node.id));
                ui.close();
            }
        }
    });
}

struct Viewer<'a> {
    root: &'a Node,
    cache: &'a mut DirectoryCache,
    searches: &'a HashMap<u64, crate::search::Search>,
    revision: u64,
    actions: &'a mut Vec<Action>,
    active: &'a mut u64,
    modal: bool,
}

impl TabViewer for Viewer<'_> {
    type Tab = Pane;
    fn title(&mut self, pane: &mut Pane) -> egui::WidgetText {
        match &pane.location {
            Location::Disk(p) => path_name(p),
            Location::Virtual(id) => self
                .root
                .find(*id)
                .map(|n| n.name.clone())
                .unwrap_or_else(|| "已移除的虚拟目录".into()),
        }
        .into()
    }
    fn id(&mut self, pane: &mut Pane) -> Id {
        Id::new(("pane", pane.id))
    }
    fn scroll_bars(&self, _: &Pane) -> [bool; 2] {
        [false, false]
    }
    fn on_tab_button(&mut self, pane: &mut Pane, response: &egui::Response) {
        if response.clicked() {
            *self.active = pane.id;
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, pane: &mut Pane) {
        let mut text_submitted = false;
        if ui.rect_contains_pointer(ui.max_rect()) && ui.input(|i| i.pointer.any_pressed()) {
            *self.active = pane.id;
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled_ui(!pane.history.is_empty(), |ui| {
                    tool_button(ui, Icon::Back, "后退 Alt+←", [26.0, 25.0])
                })
                .inner
                .clicked()
            {
                back(pane);
            }
            if ui
                .add_enabled_ui(!pane.forward.is_empty(), |ui| {
                    tool_button(ui, Icon::Forward, "前进 Alt+→", [26.0, 25.0])
                })
                .inner
                .clicked()
            {
                forward(pane);
            }
            if tool_button(ui, Icon::Up, "上一级", [26.0, 25.0]).clicked() {
                up(pane, self.root);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if split_button(ui, true).clicked() {
                    self.actions.push(Action::Split(pane.id, Split::Below));
                }
                if split_button(ui, false).clicked() {
                    self.actions.push(Action::Split(pane.id, Split::Right));
                }
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), 22.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| match &pane.location {
                        Location::Disk(_) => {
                            let address = ui.add(
                                egui::TextEdit::singleline(&mut pane.address)
                                    .desired_width(ui.available_width())
                                    .hint_text("目录路径 · Enter"),
                            );
                            if address.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                text_submitted = true;
                                let path = PathBuf::from(pane.address.trim().trim_matches('"'));
                                if path.is_absolute() {
                                    pane.navigate(Location::Disk(path), true);
                                }
                            }
                        }
                        Location::Virtual(id) => {
                            if ui.small_button("＋夹").clicked() {
                                self.actions.push(Action::NewVirtual(*id, false));
                            }
                            if ui.small_button("＋文件").clicked() {
                                self.actions.push(Action::NewVirtual(*id, true));
                            }
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!(
                                        "虚拟 / {}",
                                        self.root
                                            .find(*id)
                                            .map(|n| n.name.as_str())
                                            .unwrap_or("已移除")
                                    ))
                                    .color(ui.visuals().hyperlink_color),
                                )
                                .truncate(),
                            );
                        }
                    },
                );
            });
        });
        let response = ui.add(
            egui::TextEdit::singleline(&mut pane.filter)
                .hint_text("搜索名称…（含子目录）")
                .desired_width(f32::INFINITY),
        );
        if response.changed() {
            pane.search_deadline = Some(Instant::now() + Duration::from_millis(300));
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if enter
            || pane
                .search_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            pane.search_deadline = None;
            self.actions.push(if pane.filter.trim().is_empty() {
                Action::ExitSearch(pane.id)
            } else {
                Action::Search(pane.id, pane.location.clone(), pane.filter.clone())
            });
            text_submitted |= enter;
        }
        let searching = self
            .searches
            .get(&pane.id)
            .filter(|s| s.location == pane.location);
        if let Some(search) = searching {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{} · 已检查 {} · 跳过 {}",
                        if !search.done {
                            "搜索中"
                        } else if search.cancelled() {
                            "已停止"
                        } else {
                            "搜索完成"
                        },
                        search.scanned,
                        search.skipped
                    ))
                    .size(11.0),
                )
                .on_hover_text(format!(
                    "查询：{}\n{}",
                    search.query,
                    search.error.as_deref().unwrap_or("")
                ));
                if !search.done && ui.small_button("停止").clicked() {
                    self.actions.push(Action::StopSearch(pane.id));
                }
                if ui.small_button("返回列表").clicked() {
                    self.actions.push(Action::ExitSearch(pane.id));
                }
            });
            if search.limited {
                ui.colored_label(
                    Color32::YELLOW,
                    "已达到 100,000 条结果，请缩小搜索范围或关键词。",
                );
            }
        }
        ui.separator();
        let stamp = format!(
            "{}:{}:{:?}:{}",
            self.cache.revision, self.revision, pane.location, pane.filter
        );
        if searching.is_none() && pane.row_stamp != stamp {
            pane.rows.clear();
            collect_rows(
                &pane.location,
                0,
                self.root,
                self.cache,
                &pane.expanded,
                &mut pane.rows,
            );
            let live_keys: HashSet<_> = pane.rows.iter().map(|r| &r.key).collect();
            pane.selected.retain(|key| live_keys.contains(key));
            pane.row_stamp = stamp;
        }
        if let Location::Disk(path) = &pane.location {
            match self.cache.entries.get(path) {
                Some(Listing::Loading) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在读取目录…");
                    });
                }
                Some(Listing::Error(error)) => {
                    ui.colored_label(Color32::LIGHT_RED, error);
                }
                _ => {}
            }
        }
        let bottom = 32.0;
        let available_height = (ui.available_height() - bottom).max(30.0);
        let scroll = egui::ScrollArea::vertical()
            .id_salt(("tree", pane.id))
            .auto_shrink([false, false])
            .max_height(available_height);
        let list = scroll.show_rows(ui, ROW_HEIGHT, pane.rows.len(), |ui, range| {
            for index in range {
                let row = pane.rows[index].clone();
                let response = draw_row(
                    ui,
                    &row,
                    pane.selected.contains(&row.key),
                    pane.expanded.contains(&row.key),
                );
                if searching.is_some() && response.hovered() {
                    response.clone().on_hover_text(&row.detail);
                }
                if response.clicked() {
                    let modifiers = ui.input(|i| i.modifiers);
                    if row.directory
                        && response.interact_pointer_pos().is_some_and(|p| {
                            p.x < response.rect.left() + row.depth as f32 * 16.0 + 21.0
                        })
                    {
                        toggle(pane, &row.key);
                    } else if modifiers.shift {
                        let anchor = pane
                            .anchor
                            .unwrap_or(index)
                            .min(pane.rows.len().saturating_sub(1));
                        if !modifiers.ctrl {
                            pane.selected.clear();
                        }
                        for r in &pane.rows[anchor.min(index)..=anchor.max(index)] {
                            pane.selected.insert(r.key.clone());
                        }
                    } else {
                        if !modifiers.ctrl {
                            pane.selected.clear();
                        }
                        if !pane.selected.insert(row.key.clone()) && modifiers.ctrl {
                            pane.selected.remove(&row.key);
                        }
                        pane.anchor = Some(index);
                    }
                }
                if response.double_clicked() {
                    activate(&row, pane, self.actions);
                }
                if let Location::Virtual(id) = row.target {
                    if let Some(node) = self.root.find(id) {
                        virtual_menu(&response, node, self.actions, || {
                            if pane.selected.contains(&row.key) {
                                pane.rows
                                    .iter()
                                    .filter(|r| pane.selected.contains(&r.key))
                                    .flat_map(|r| match &r.target {
                                        Location::Disk(path) => vec![path.clone()],
                                        Location::Virtual(id) => self
                                            .root
                                            .find(*id)
                                            .map(Node::real_paths)
                                            .unwrap_or_default(),
                                    })
                                    .collect()
                            } else {
                                node.real_paths()
                            }
                        });
                        response.dnd_set_drag_payload(VirtualDrag(node.id));
                        if !matches!(node.kind, Kind::Link { .. }) {
                            accept_drop(&response, id, self.actions);
                        }
                    }
                } else if let Location::Disk(path) = &row.target {
                    if row.directory {
                        accept_disk_drop(&response, path, self.actions);
                    }
                    if response.drag_started() {
                        let paths = if pane.selected.contains(&row.key) {
                            selected_paths(pane, self.root)
                        } else {
                            vec![path.clone()]
                        };
                        response.dnd_set_drag_payload(paths);
                    }
                    response.context_menu(|ui| {
                        let paths = if pane.selected.contains(&row.key) {
                            selected_paths(pane, self.root)
                        } else {
                            vec![path.clone()]
                        };
                        real_menu(ui, path, row.directory, paths, self.actions);
                        if searching.is_some() && ui.button("打开所在目录").clicked() {
                            if let Some(parent) = path.parent() {
                                self.actions
                                    .push(Action::NewPane(Location::Disk(parent.into())));
                            }
                            ui.close();
                        }
                    });
                }
            }
        });
        if pane.rows.is_empty() {
            ui.painter().text(
                list.inner_rect.center(),
                egui::Align2::CENTER_CENTER,
                "此处为空，或没有匹配项",
                egui::FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
        }
        if let Location::Virtual(id) = pane.location {
            // A blank area is also a drop target; child rows get first chance.
            let background = ui.interact(
                list.inner_rect,
                Id::new(("virtual-drop", pane.id)),
                Sense::hover(),
            );
            accept_drop(&background, id, self.actions);
        } else if let Location::Disk(path) = &pane.location {
            let background = ui.interact(
                list.inner_rect,
                Id::new(("disk-drop", pane.id)),
                Sense::hover(),
            );
            accept_disk_drop(&background, path, self.actions);
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} 项  ·  选中 {}",
                    pane.rows.len(),
                    pane.selected.len()
                ))
                .size(11.0)
                .color(ui.visuals().weak_text_color()),
            );
            match &pane.location {
                Location::Virtual(id) => {
                    let response = ui.add(egui::Button::new(
                        RichText::new("拖入引用 / 添加…").size(11.0),
                    ));
                    if response.clicked() {
                        self.actions.push(Action::PickImport(*id, false));
                    }
                    accept_drop(&response, *id, self.actions);
                }
                Location::Disk(path) => {
                    if ui.small_button("新建夹").clicked() {
                        self.actions.push(Action::NewReal(path.clone(), true));
                    }
                    if ui.small_button("粘贴").clicked() {
                        self.actions.push(Action::Paste(path.clone()));
                    }
                }
            }
        });
        if *self.active == pane.id
            && !self.modal
            && !ui.ctx().egui_wants_keyboard_input()
            && !text_submitted
        {
            keyboard(ui, pane, self.root, self.actions);
        }
    }
}

fn collect_rows(
    location: &Location,
    depth: usize,
    root: &Node,
    cache: &mut DirectoryCache,
    expanded: &HashSet<String>,
    rows: &mut Vec<Row>,
) {
    match location {
        Location::Disk(path) => {
            cache.request(path);
            let entries = match cache.entries.get(path) {
                Some(Listing::Ready(entries)) => entries.clone(),
                _ => return,
            };
            for entry in entries {
                let key = format!("p:{}", entry.path.display());
                let target = Location::Disk(entry.path.clone());
                let detail = if entry.link {
                    format!("链接 · {}", display_path(&entry.path))
                } else {
                    display_path(&entry.path)
                };
                rows.push(Row {
                    key: key.clone(),
                    name: entry.name,
                    depth,
                    target: target.clone(),
                    directory: entry.directory,
                    virtual_file: false,
                    detail,
                });
                if entry.directory && expanded.contains(&key) {
                    collect_rows(&target, depth + 1, root, cache, expanded, rows);
                }
            }
        }
        Location::Virtual(id) => {
            if let Some(Node {
                kind: Kind::Folder(children),
                ..
            }) = root.find(*id)
            {
                for node in children {
                    let key = format!("v:{}", node.id);
                    let (directory, virtual_file, detail) = match &node.kind {
                        Kind::Folder(c) => (true, false, format!("虚拟文件夹 · {} 项", c.len())),
                        Kind::File(p) => (
                            false,
                            true,
                            format!("虚拟文件 · {} 个映射版本 · 双击选择", p.len()),
                        ),
                        Kind::Link { path, directory } => (
                            *directory,
                            false,
                            format!("路径引用 · {}", display_path(path)),
                        ),
                    };
                    rows.push(Row {
                        key: key.clone(),
                        name: node.name.clone(),
                        depth,
                        target: Location::Virtual(node.id),
                        directory,
                        virtual_file,
                        detail,
                    });
                    if directory && expanded.contains(&key) {
                        let target = if let Kind::Link { path, .. } = &node.kind {
                            Location::Disk(path.clone())
                        } else {
                            Location::Virtual(node.id)
                        };
                        collect_rows(&target, depth + 1, root, cache, expanded, rows);
                    }
                }
            }
        }
    }
}

fn draw_row(ui: &mut egui::Ui, row: &Row, selected: bool, expanded: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), ROW_HEIGHT),
        Sense::click_and_drag(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            &row.name,
        )
    });
    let painter = ui.painter_at(rect);
    if selected || response.hovered() {
        painter.rect_filled(
            rect,
            0,
            if selected {
                ui.visuals().selection.bg_fill
            } else {
                ui.visuals().widgets.hovered.bg_fill
            },
        );
    }
    let x = rect.left() + 5.0 + row.depth as f32 * 16.0;
    let y = rect.center().y;
    if row.directory {
        paint_chevron(&painter, egui::pos2(x + 5.0, y), expanded);
    }
    let icon = egui::Rect::from_min_size(egui::pos2(x + 17.0, y - 6.0), Vec2::new(14.0, 12.0));
    let file_icon = if row.directory {
        Icon::Folder
    } else if row.virtual_file {
        Icon::VirtualFile
    } else {
        match Path::new(&row.name)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "rs" | "cs" | "cpp" | "h" | "lua" | "py" | "js" | "ts" => Icon::Code,
            "json" | "xml" | "yaml" | "toml" | "ini" => Icon::Settings,
            "png" | "jpg" | "dds" | "tga" => Icon::Image,
            "prefab" | "unity" | "asset" => Icon::Asset,
            "zip" | "7z" | "rar" => Icon::Archive,
            _ => Icon::File,
        }
    };
    let color = if row.directory {
        ui.visuals().text_color()
    } else {
        ui.visuals().hyperlink_color
    };
    paint_icon(&painter, icon.center(), file_icon, color);
    painter.text(
        egui::pos2(x + 39.0, y),
        egui::Align2::LEFT_CENTER,
        &row.name,
        egui::FontId::proportional(14.0),
        ui.visuals().text_color(),
    );
    response.on_hover_text(&row.detail)
}

fn toggle(pane: &mut Pane, key: &str) {
    if !pane.expanded.insert(key.into()) {
        pane.expanded.remove(key);
    }
    pane.row_stamp.clear();
}

fn activate(row: &Row, pane: &mut Pane, actions: &mut Vec<Action>) {
    if row.virtual_file {
        if let Location::Virtual(id) = row.target {
            actions.push(Action::Choose(id));
        }
    } else if row.directory {
        actions.push(Action::Navigate(pane.id, row.target.clone()));
    } else if let Location::Disk(path) = &row.target {
        actions.push(Action::Open(path.clone()));
    } else if let Location::Virtual(id) = row.target {
        actions.push(Action::Navigate(pane.id, Location::Virtual(id)));
    }
}

fn selected_paths(pane: &Pane, root: &Node) -> Vec<PathBuf> {
    pane.rows
        .iter()
        .filter(|r| pane.selected.contains(&r.key))
        .filter_map(|r| match &r.target {
            Location::Disk(p) => Some(p.clone()),
            Location::Virtual(id) => match root.find(*id) {
                Some(Node {
                    kind: Kind::Link { path, .. },
                    ..
                }) => Some(path.clone()),
                _ => None,
            },
        })
        .collect()
}

fn paths_text(paths: &[PathBuf]) -> String {
    let mut seen = HashSet::new();
    paths
        .iter()
        .map(|path| display_path(path))
        .filter(|path| seen.insert(path.clone()))
        .collect::<Vec<_>>()
        .join("\r\n")
}

fn copy_paths_menu(ui: &mut egui::Ui, paths: &[PathBuf]) {
    if ui
        .add_enabled(!paths.is_empty(), egui::Button::new("复制路径"))
        .clicked()
    {
        ui.ctx().copy_text(paths_text(paths));
        ui.close();
    }
}

fn real_menu(
    ui: &mut egui::Ui,
    path: &Path,
    directory: bool,
    paths: Vec<PathBuf>,
    actions: &mut Vec<Action>,
) {
    if ui.button("Windows 系统右键菜单…").clicked() {
        actions.push(Action::SystemMenu(paths.clone()));
        ui.close();
    }
    if ui.button("打开").clicked() {
        actions.push(if directory {
            Action::NewPane(Location::Disk(path.into()))
        } else {
            Action::Open(path.into())
        });
        ui.close();
    }
    if ui.button("复制  Ctrl+C").clicked() {
        actions.push(Action::Copy(paths.clone(), false));
        ui.close();
    }
    if ui.button("剪切  Ctrl+X").clicked() {
        actions.push(Action::Copy(paths.clone(), true));
        ui.close();
    }
    copy_paths_menu(ui, &paths);
    if ui.button("重命名  F2").clicked() {
        actions.push(Action::RenameReal(path.into()));
        ui.close();
    }
    if directory {
        if ui.button("粘贴到此目录").clicked() {
            actions.push(Action::Paste(path.into()));
            ui.close();
        }
        if ui.button("新建文件夹").clicked() {
            actions.push(Action::NewReal(path.into(), true));
            ui.close();
        }
        if ui.button("新建空文件").clicked() {
            actions.push(Action::NewReal(path.into(), false));
            ui.close();
        }
    }
    ui.separator();
    if ui.button("移入回收站…  Delete").clicked() {
        actions.push(Action::DeleteReal(paths));
        ui.close();
    }
}

fn back(pane: &mut Pane) {
    if let Some(location) = pane.history.pop() {
        pane.forward.push(pane.location.clone());
        pane.navigate(location, false);
    }
}
fn forward(pane: &mut Pane) {
    if let Some(location) = pane.forward.pop() {
        pane.history.push(pane.location.clone());
        pane.navigate(location, false);
    }
}
fn up(pane: &mut Pane, root: &Node) {
    let parent = match &pane.location {
        Location::Disk(path) => path.parent().map(|p| Location::Disk(p.to_owned())),
        Location::Virtual(id) => {
            fn parent(node: &Node, id: u64) -> Option<u64> {
                if let Kind::Folder(c) = &node.kind {
                    if c.iter().any(|n| n.id == id) {
                        return Some(node.id);
                    }
                    c.iter().find_map(|n| parent(n, id))
                } else {
                    None
                }
            }
            parent(root, *id).map(Location::Virtual)
        }
    };
    if let Some(parent) = parent {
        pane.navigate(parent, true);
    }
}

fn keyboard(ui: &mut egui::Ui, pane: &mut Pane, root: &Node, actions: &mut Vec<Action>) {
    ui.input(|input| {
        if !input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::Key { pressed: true, .. }))
        {
            return;
        }
        let ctrl = input.modifiers.ctrl;
        if ctrl && input.key_pressed(egui::Key::A) {
            pane.selected = pane.rows.iter().map(|r| r.key.clone()).collect();
        }
        if ctrl && input.key_pressed(egui::Key::C) {
            actions.push(Action::Copy(selected_paths(pane, root), false));
        }
        if ctrl && input.key_pressed(egui::Key::X) {
            actions.push(Action::Copy(selected_paths(pane, root), true));
        }
        if ctrl
            && input.key_pressed(egui::Key::V)
            && let Location::Disk(path) = &pane.location
        {
            actions.push(Action::Paste(path.clone()));
        }
        if input.modifiers.alt && input.key_pressed(egui::Key::ArrowLeft) {
            back(pane);
        }
        if input.modifiers.alt && input.key_pressed(egui::Key::ArrowRight) {
            forward(pane);
        }
        if input.key_pressed(egui::Key::Backspace) {
            up(pane, root);
        }
        if (input.key_pressed(egui::Key::ArrowDown) || input.key_pressed(egui::Key::ArrowUp))
            && !pane.rows.is_empty()
        {
            let index = if input.key_pressed(egui::Key::ArrowDown) {
                pane.anchor
                    .map(|a| (a + 1).min(pane.rows.len() - 1))
                    .unwrap_or(0)
            } else {
                pane.anchor
                    .unwrap_or(0)
                    .saturating_sub(1)
                    .min(pane.rows.len() - 1)
            };
            pane.anchor = Some(index);
            pane.selected.clear();
            pane.selected.insert(pane.rows[index].key.clone());
        }
        if let Some(row) = pane
            .rows
            .iter()
            .find(|r| pane.selected.contains(&r.key))
            .cloned()
        {
            if input.key_pressed(egui::Key::Enter) {
                activate(&row, pane, actions);
            }
            if input.key_pressed(egui::Key::F2) {
                actions.push(match row.target.clone() {
                    Location::Disk(p) => Action::RenameReal(p),
                    Location::Virtual(id) => Action::RenameVirtual(id),
                });
            }
            if !input.modifiers.alt && row.directory {
                if input.key_pressed(egui::Key::ArrowRight) {
                    pane.expanded.insert(row.key.clone());
                    pane.row_stamp.clear();
                }
                if input.key_pressed(egui::Key::ArrowLeft) {
                    pane.expanded.remove(&row.key);
                    pane.row_stamp.clear();
                }
            }
        }
        if input.key_pressed(egui::Key::Delete) {
            // Delete on virtual rows removes references; real rows use the recycle bin.
            let paths = pane
                .rows
                .iter()
                .filter(|r| pane.selected.contains(&r.key))
                .filter_map(|r| match &r.target {
                    Location::Disk(p) => Some(p.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if !paths.is_empty() {
                actions.push(Action::DeleteReal(paths));
            } else {
                for row in &pane.rows {
                    if pane.selected.contains(&row.key)
                        && let Location::Virtual(id) = row.target
                    {
                        actions.push(Action::RemoveVirtual(id));
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_paths_are_normalized_deduplicated_and_line_separated() {
        assert_eq!(
            paths_text(&[
                PathBuf::from(r"\\?\C:\项目\日志.txt"),
                PathBuf::from("C:/项目/日志.txt"),
                PathBuf::from(r"\\?\UNC\server\共享\配置.json"),
            ]),
            "C:/项目/日志.txt\r\n//server/共享/配置.json"
        );
        assert!(paths_text(&[]).is_empty());
    }

    #[test]
    fn search_input_starts_recursive_search_and_clearing_exits() {
        let ctx = egui::Context::default();
        let root = Workspace::default().root;
        let mut cache = DirectoryCache::new(ctx.clone());
        let mut pane = Pane::new(1, Location::Virtual(0));
        let mut active = 1;
        for query in ["config", ""] {
            pane.filter = query.into();
            pane.search_deadline = Some(Instant::now());
            let mut actions = vec![];
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                Viewer {
                    root: &root,
                    cache: &mut cache,
                    searches: &HashMap::new(),
                    revision: 1,
                    actions: &mut actions,
                    active: &mut active,
                    modal: false,
                }
                .ui(ui, &mut pane);
            });
            if query.is_empty() {
                assert!(matches!(actions.as_slice(), [Action::ExitSearch(1)]));
            } else {
                assert!(
                    matches!(actions.as_slice(), [Action::Search(1, Location::Virtual(0), q)] if q == query)
                );
            }
            assert!(pane.search_deadline.is_none());
        }
    }

    #[test]
    fn external_drop_is_consumed_once_by_virtual_target() {
        let ctx = egui::Context::default();
        let mut actions = Vec::new();
        let mut target = egui::Pos2::ZERO;
        for dropped in [false, true] {
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(400.0, 200.0),
                    )),
                    events: vec![egui::Event::PointerMoved(target)],
                    dropped_files: if dropped {
                        vec![egui::DroppedFile {
                            path: Some(PathBuf::from("C:/source.rs")),
                            ..Default::default()
                        }]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                },
                |ui| {
                    let response = ui.button("virtual target");
                    target = response.rect.center();
                    accept_drop(&response, 9, &mut actions);
                    accept_drop(&response, 0, &mut actions);
                },
            );
        }
        assert!(
            matches!(actions.as_slice(), [Action::Import(9, paths)] if paths == &vec![PathBuf::from("C:/source.rs")])
        );
    }

    #[test]
    fn drag_payload_reaches_virtual_and_real_targets() {
        for (real_target, folder_target, moving) in [
            (false, false, false),
            (true, false, false),
            (true, false, true),
            (true, true, false),
        ] {
            let ctx = egui::Context::default();
            let root = Node {
                id: 9,
                name: "TestDrop".into(),
                kind: Kind::Folder(vec![]),
            };
            let mut cache = DirectoryCache::new(ctx.clone());
            cache.entries.insert(
                PathBuf::from("C:/"),
                Listing::Ready(vec![files::Entry {
                    path: "C:/source.rs".into(),
                    name: "source.rs".into(),
                    directory: false,
                    link: false,
                }]),
            );
            cache.entries.insert(
                PathBuf::from("C:/target"),
                Listing::Ready(vec![files::Entry {
                    path: "C:/target/drop-target".into(),
                    name: "drop-target".into(),
                    directory: folder_target,
                    link: false,
                }]),
            );
            let mut dock = Workspace::default().dock;
            for (_, pane) in dock.iter_all_tabs_mut() {
                pane.location = if pane.id == 1 {
                    if real_target {
                        Location::Disk("C:/target".into())
                    } else {
                        Location::Virtual(9)
                    }
                } else {
                    Location::Disk("C:/".into())
                };
            }
            let mut expanded = HashSet::new();
            let mut active = 1;
            let mut actions = Vec::new();
            let mut frame = |events: Vec<egui::Event>| {
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(1400.0, 900.0),
                        )),
                        modifiers: egui::Modifiers {
                            shift: moving,
                            ..Default::default()
                        },
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        egui::Panel::left("test-library")
                            .exact_size(180.0)
                            .show(ui, |ui| {
                                library_node(ui, &root, 0, &mut expanded, &mut actions)
                            });
                        egui::CentralPanel::default().show(ui, |ui| {
                            let mut viewer = Viewer {
                                root: &root,
                                cache: &mut cache,
                                searches: &HashMap::new(),
                                revision: 1,
                                actions: &mut actions,
                                active: &mut active,
                                modal: false,
                            };
                            DockArea::new(&mut dock).show_inside(ui, &mut viewer);
                        });
                    },
                );
                let find_text = |needle: &str| {
                    output
                        .shapes
                        .iter()
                        .find_map(|s| {
                            if let egui::Shape::Text(text) = &s.shape
                                && text.galley.job.text.contains(needle)
                            {
                                return Some(text.pos + Vec2::new(6.0, 6.0));
                            }
                            None
                        })
                        .expect("test label missing")
                };
                (
                    find_text("source.rs"),
                    find_text(if real_target {
                        "drop-target"
                    } else {
                        "TestDrop"
                    }),
                )
            };
            let (source, target) = frame(vec![]);
            frame(vec![
                egui::Event::PointerMoved(source),
                egui::Event::PointerButton {
                    pos: source,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers {
                        shift: moving,
                        ..Default::default()
                    },
                },
            ]);
            frame(vec![egui::Event::PointerMoved(
                source + Vec2::new(15.0, 0.0),
            )]);
            frame(vec![egui::Event::PointerMoved(target)]);
            frame(vec![egui::Event::PointerButton {
                pos: target,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers {
                    shift: moving,
                    ..Default::default()
                },
            }]);
            if real_target {
                let expected = PathBuf::from(if folder_target {
                    "C:/target/drop-target"
                } else {
                    "C:/target"
                });
                assert!(
                    matches!(actions.as_slice(), [Action::Transfer(paths, destination, m)] if paths == &vec![PathBuf::from("C:/source.rs")] && destination == &expected && *m == moving)
                );
            } else {
                assert!(
                    matches!(actions.as_slice(), [Action::Import(9, paths)] if paths == &vec![PathBuf::from("C:/source.rs")])
                );
            }
        }
    }

    #[test]
    fn large_directory_draws_only_visible_rows() {
        let ctx = egui::Context::default();
        let mut cache = DirectoryCache::new(ctx.clone());
        let path = PathBuf::from("C:/synthetic-large-directory");
        cache.entries.insert(
            path.clone(),
            Listing::Ready(
                (0..100_000)
                    .map(|i| files::Entry {
                        path: path.join(format!("file-{i}.rs")),
                        name: format!("file-{i}.rs"),
                        directory: false,
                        link: false,
                    })
                    .collect(),
            ),
        );
        let root = Node {
            id: 0,
            name: "root".into(),
            kind: Kind::Folder(vec![]),
        };
        let mut pane = Pane::new(1, Location::Disk(path));
        let mut actions = vec![];
        let mut active = 1;
        let mut viewer = Viewer {
            root: &root,
            cache: &mut cache,
            searches: &HashMap::new(),
            revision: 1,
            actions: &mut actions,
            active: &mut active,
            modal: false,
        };
        let started = Instant::now();
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(900.0, 600.0),
                )),
                ..Default::default()
            },
            |ui| viewer.ui(ui, &mut pane),
        );
        assert_eq!(pane.rows.len(), 100_000);
        assert!(
            output.shapes.len() < 1000,
            "offscreen rows were rendered: {}",
            output.shapes.len()
        );
        let rebuild = started.elapsed();
        let started = Instant::now();
        for _ in 0..20 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| viewer.ui(ui, &mut pane));
        }
        println!(
            "100k cached entries: rebuild={rebuild:?}; warm frame average={:?}; shapes={}",
            started.elapsed() / 20,
            output.shapes.len()
        );
    }
}
