use egui_dock::{DockState, NodeIndex};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum Kind {
    Folder(Vec<Node>),
    File(Vec<PathBuf>),
    Link { path: PathBuf, directory: bool },
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Node {
    pub id: u64,
    pub name: String,
    pub kind: Kind,
}

impl Node {
    pub fn real_paths(&self) -> Vec<PathBuf> {
        match &self.kind {
            Kind::Folder(children) => children.iter().flat_map(Node::real_paths).collect(),
            Kind::File(paths) => paths.clone(),
            Kind::Link { path, .. } => vec![path.clone()],
        }
    }
    pub fn find(&self, id: u64) -> Option<&Node> {
        if self.id == id {
            return Some(self);
        }
        if let Kind::Folder(children) = &self.kind {
            return children.iter().find_map(|n| n.find(id));
        }
        None
    }

    pub fn find_mut(&mut self, id: u64) -> Option<&mut Node> {
        if self.id == id {
            return Some(self);
        }
        if let Kind::Folder(children) = &mut self.kind {
            return children.iter_mut().find_map(|n| n.find_mut(id));
        }
        None
    }

    pub fn remove(&mut self, id: u64) -> bool {
        self.take(id).is_some()
    }

    fn take(&mut self, id: u64) -> Option<Node> {
        if let Kind::Folder(children) = &mut self.kind {
            if let Some(index) = children.iter().position(|n| n.id == id) {
                return Some(children.remove(index));
            }
            return children.iter_mut().find_map(|n| n.take(id));
        }
        None
    }

    pub fn reparent(&mut self, id: u64, destination: u64) -> Result<(), String> {
        if id == self.id {
            return Err("不能移动工作区根节点。".into());
        }
        let source = self.find(id).ok_or("待移动的虚拟节点已不存在。")?;
        if source.find(destination).is_some() {
            return Err("不能把虚拟节点放入自身或子目录。".into());
        }
        if !matches!(
            self.find(destination).map(|n| &n.kind),
            Some(Kind::Folder(_))
        ) {
            return Err("请把虚拟节点拖到虚拟文件夹上。".into());
        }
        let node = self.take(id).ok_or("待移动的虚拟节点已不存在。")?;
        if let Some(Node {
            kind: Kind::Folder(children),
            ..
        }) = self.find_mut(destination)
        {
            children.push(node);
        }
        Ok(())
    }

    pub fn add_paths(
        &mut self,
        paths: Vec<(PathBuf, bool)>,
        next_id: &mut u64,
    ) -> Result<usize, String> {
        let mut count = 0;
        match &mut self.kind {
            Kind::Folder(children) => {
                for (path, directory) in paths {
                    if children.iter().any(
                        |n| matches!(&n.kind, Kind::Link { path: p, .. } if same_path(p, &path)),
                    ) {
                        continue;
                    }
                    children.push(Node {
                        id: *next_id,
                        name: path_name(&path),
                        kind: Kind::Link { path, directory },
                    });
                    *next_id += 1;
                    count += 1;
                }
            }
            Kind::File(targets) => {
                if paths.iter().any(|(_, directory)| *directory) {
                    return Err("虚拟文件只能映射文件；文件夹请放入虚拟文件夹。".into());
                }
                for (path, _) in paths {
                    if !targets.iter().any(|p| same_path(p, &path)) {
                        targets.push(path);
                        count += 1;
                    }
                }
            }
            Kind::Link { .. } => return Err("请拖到虚拟文件夹或虚拟文件上。".into()),
        }
        Ok(count)
    }
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    // Windows aliases are also canonicalized by the background import worker.
    a == b
}

pub fn path_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| display_path(path))
}

/// Human-readable paths only; native file operations keep the original PathBuf.
pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if let Some(unc) = text.strip_prefix("//?/UNC/") {
        format!("//{unc}")
    } else if let Some(drive) = text.strip_prefix("//?/")
        && drive.as_bytes().get(1..3) == Some(b":/")
    {
        drive.to_owned()
    } else {
        text
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub enum Location {
    Disk(PathBuf),
    Virtual(u64),
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: String,
    pub name: String,
    pub depth: usize,
    pub target: Location,
    pub directory: bool,
    pub virtual_file: bool,
    pub detail: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Pane {
    pub id: u64,
    pub location: Location,
    pub address: String,
    pub expanded: HashSet<String>,
    #[serde(skip)]
    pub filter: String,
    #[serde(skip)]
    pub search_deadline: Option<std::time::Instant>,
    #[serde(skip)]
    pub rows: Vec<Row>,
    #[serde(skip)]
    pub row_stamp: String,
    #[serde(skip)]
    pub selected: HashSet<String>,
    #[serde(skip)]
    pub anchor: Option<usize>,
    #[serde(skip)]
    pub history: Vec<Location>,
    #[serde(skip)]
    pub forward: Vec<Location>,
}

impl Pane {
    pub fn new(id: u64, location: Location) -> Self {
        let mut pane = Self {
            id,
            location: location.clone(),
            address: String::new(),
            expanded: HashSet::new(),
            filter: String::new(),
            search_deadline: None,
            rows: Vec::new(),
            row_stamp: String::new(),
            selected: HashSet::new(),
            anchor: None,
            history: Vec::new(),
            forward: Vec::new(),
        };
        pane.navigate(location, false);
        pane
    }
    pub fn navigate(&mut self, location: Location, remember: bool) {
        if remember && location != self.location {
            self.history.push(self.location.clone());
            self.forward.clear();
        }
        self.address = match &location {
            Location::Disk(p) => display_path(p),
            Location::Virtual(_) => String::new(),
        };
        self.location = location;
        self.selected.clear();
        self.anchor = None;
        self.row_stamp.clear();
        self.filter.clear();
        self.search_deadline = None;
    }
}

#[derive(Serialize, Deserialize)]
pub struct Workspace {
    pub version: u32,
    #[serde(default)]
    pub theme: crate::theme::Theme,
    pub root: Node,
    pub next_id: u64,
    #[serde(serialize_with = "serialize_dock")]
    pub dock: DockState<Pane>,
}

fn serialize_dock<S: serde::Serializer>(
    dock: &DockState<Pane>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // Dock nodes contain transient Rect::NOTHING (infinite coordinates). JSON cannot
    // round-trip those numbers. Reset only layout rectangles; docking recomputes them.
    let mut value = serde_json::to_value(dock).map_err(serde::ser::Error::custom)?;
    fn reset(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    if key == "rect" || key == "viewport" {
                        *value = serde_json::to_value(eframe::egui::Rect::ZERO).unwrap();
                    } else {
                        reset(value);
                    }
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    reset(value);
                }
            }
            _ => {}
        }
    }
    reset(&mut value);
    value.serialize(serializer)
}

impl Default for Workspace {
    fn default() -> Self {
        let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:\\"));
        let home = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| current.clone());
        let mut dock = DockState::new(vec![Pane::new(1, Location::Virtual(0))]);
        let [left, right] = dock.main_surface_mut().split_right(
            NodeIndex::root(),
            0.5,
            vec![Pane::new(2, Location::Disk(current.clone()))],
        );
        dock.main_surface_mut().split_below(
            left,
            0.5,
            vec![Pane::new(3, Location::Disk(home.clone()))],
        );
        dock.main_surface_mut().split_below(
            right,
            0.5,
            vec![Pane::new(4, Location::Disk(current))],
        );
        Self {
            version: 1,
            theme: crate::theme::Theme::default(),
            root: Node {
                id: 0,
                name: "虚拟工作区".into(),
                kind: Kind::Folder(vec![]),
            },
            next_id: 5,
            dock,
        }
    }
}

impl Workspace {
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read(path) {
            Ok(bytes) => Self::from_bytes(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let mut state: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if state.version != 1 {
            return Err("不支持的工作区版本".into());
        }
        let mut ids = HashSet::new();
        fn check(n: &Node, ids: &mut HashSet<u64>, next: u64) -> bool {
            if !ids.insert(n.id) || n.id >= next {
                return false;
            }
            if let Kind::Folder(c) = &n.kind {
                c.iter().all(|n| check(n, ids, next))
            } else {
                true
            }
        }
        if state.root.id != 0
            || !matches!(state.root.kind, Kind::Folder(_))
            || !check(&state.root, &mut ids, state.next_id)
        {
            return Err("虚拟工作区结构无效".into());
        }
        let mut pane_ids = HashSet::new();
        if !state
            .dock
            .iter_all_tabs()
            .all(|(_, p)| p.id < state.next_id && pane_ids.insert(p.id))
        {
            return Err("面板标识无效".into());
        }
        // Refresh old saved address text without rewriting native paths or expanded IDs.
        for (_, pane) in state.dock.iter_all_tabs_mut() {
            pane.address = match &pane.location {
                Location::Disk(path) => display_path(path),
                Location::Virtual(_) => String::new(),
            };
        }
        Ok(state)
    }

    pub fn restore(source: &Path, destination: &Path) -> Result<(Self, Option<PathBuf>), String> {
        // Validate before touching the current workspace; a missing source must never mean empty.
        let bytes = fs::read(source).map_err(|e| e.to_string())?;
        let restored = Self::from_bytes(&bytes)?;
        let backup = match fs::read(destination) {
            Ok(previous) => {
                let parent = destination.parent().ok_or("工作区目录无效")?;
                let mut backup = tempfile::Builder::new()
                    .prefix("workspace-before-restore-")
                    .suffix(".json")
                    .tempfile_in(parent)
                    .map_err(|e| e.to_string())?;
                backup.write_all(&previous).map_err(|e| e.to_string())?;
                backup.as_file().sync_all().map_err(|e| e.to_string())?;
                let (_, path) = backup.keep().map_err(|e| e.to_string())?;
                Some(path)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        restored.save(destination)?;
        Ok((restored, backup))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let parent = path.parent().ok_or("工作区目录无效")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        serde_json::to_writer_pretty(&mut tmp, self).map_err(|e| e.to_string())?;
        tmp.flush().map_err(|e| e.to_string())?;
        tmp.as_file().sync_all().map_err(|e| e.to_string())?;
        tmp.persist(path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theme_roundtrip_and_legacy_workspace_default() {
        let mut workspace = Workspace::default();
        for theme in crate::theme::Theme::ALL {
            workspace.theme = theme;
            let bytes = serde_json::to_vec(&workspace).unwrap();
            assert!(Workspace::from_bytes(&bytes).unwrap().theme == theme);
        }
        let mut legacy = serde_json::to_value(&workspace).unwrap();
        legacy.as_object_mut().unwrap().remove("theme");
        assert!(
            Workspace::from_bytes(&serde_json::to_vec(&legacy).unwrap())
                .unwrap()
                .theme
                == crate::theme::Theme::Vscode
        );
    }

    #[test]
    fn restore_preserves_previous_workspace_and_rejects_missing_or_invalid_sources() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("backup.json");
        let destination = dir.path().join("workspace.json");
        let mut imported = Workspace::default();
        imported
            .root
            .add_paths(
                vec![(PathBuf::from("C:/项目"), true)],
                &mut imported.next_id,
            )
            .unwrap();
        imported.save(&source).unwrap();
        Workspace::default().save(&destination).unwrap();
        let previous = fs::read(&destination).unwrap();
        let (restored, backup) = Workspace::restore(&source, &destination).unwrap();
        assert_eq!(restored.root.real_paths(), vec![PathBuf::from("C:/项目")]);
        assert_eq!(fs::read(backup.unwrap()).unwrap(), previous);
        let expected = fs::read(&destination).unwrap();
        assert_eq!(
            Workspace::load(&destination).unwrap().root.real_paths(),
            restored.root.real_paths()
        );
        assert!(Workspace::restore(&dir.path().join("missing.json"), &destination).is_err());
        fs::write(&source, "invalid json").unwrap();
        assert!(Workspace::restore(&source, &destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), expected);
    }
    #[test]
    fn display_paths_hide_windows_prefixes_and_restore_old_addresses() {
        for (input, expected) in [
            (r"C:\项目\配置.json", "C:/项目/配置.json"),
            (r"\\?\C:\项目\配置.json", "C:/项目/配置.json"),
            (r"\\?\UNC\server\共享\配置.json", "//server/共享/配置.json"),
            (r"\\server\共享\配置.json", "//server/共享/配置.json"),
            (r"C:\项目/mixed\", "C:/项目/mixed/"),
            (r"\\?\C:\", "C:/"),
        ] {
            let native = PathBuf::from(input);
            assert_eq!(display_path(&native), expected);
            assert_eq!(native.as_os_str(), input);
            assert_eq!(Pane::new(1, Location::Disk(native)).address, expected);
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("配置.json");
        fs::write(&path, "中文内容").unwrap();
        let native = fs::canonicalize(&path).unwrap();
        assert_eq!(
            fs::read_to_string(display_path(&native)).unwrap(),
            "中文内容"
        );
        let mut workspace = Workspace::default();
        let (_, pane) = workspace.dock.iter_all_tabs_mut().next().unwrap();
        pane.location = Location::Disk(native.clone());
        pane.address = native.display().to_string();
        let config = directory.path().join("workspace.json");
        workspace.save(&config).unwrap();
        let loaded = Workspace::load(&config).unwrap();
        let (_, pane) = loaded.dock.iter_all_tabs().next().unwrap();
        assert_eq!(pane.address, display_path(&native));
        assert_eq!(pane.location, Location::Disk(native));
    }

    #[test]
    fn virtual_links_are_non_destructive_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("配置.json");
        fs::write(&real, "original").unwrap();
        let mut state = Workspace::default();
        let mut mapping = Node {
            id: 5,
            name: "配置".into(),
            kind: Kind::File(vec![]),
        };
        assert!(
            mapping
                .add_paths(vec![(dir.path().into(), true)], &mut state.next_id)
                .is_err()
        );
        assert_eq!(
            mapping
                .add_paths(
                    vec![(real.clone(), false), (real.clone(), false)],
                    &mut state.next_id
                )
                .unwrap(),
            1
        );
        state.next_id = 6;
        if let Kind::Folder(c) = &mut state.root.kind {
            c.push(mapping);
        }
        let config = dir.path().join("workspace.json");
        state.save(&config).unwrap();
        let mut loaded = Workspace::load(&config).unwrap();
        assert!(
            matches!(&loaded.root.find(5).unwrap().kind, Kind::File(p) if p == &vec![real.clone()])
        );
        assert_eq!(loaded.dock.iter_all_tabs().count(), 4);
        assert!(loaded.root.remove(5));
        loaded.save(&config).unwrap();
        assert!(Workspace::load(&config).unwrap().root.find(5).is_none());
        assert_eq!(fs::read_to_string(real).unwrap(), "original");
        fs::write(&config, "broken").unwrap();
        assert!(Workspace::load(&config).is_err());
    }

    #[test]
    fn virtual_reparent_preserves_nodes_and_rejects_cycles() {
        let file = Node {
            id: 3,
            name: "shared".into(),
            kind: Kind::File(vec![PathBuf::from("C:/project/config.json")]),
        };
        let mut root = Node {
            id: 0,
            name: "root".into(),
            kind: Kind::Folder(vec![
                Node {
                    id: 1,
                    name: "A".into(),
                    kind: Kind::Folder(vec![file]),
                },
                Node {
                    id: 2,
                    name: "B".into(),
                    kind: Kind::Folder(vec![]),
                },
            ]),
        };
        root.reparent(3, 2).unwrap();
        assert!(root.find(1).unwrap().find(3).is_none());
        assert!(
            matches!(&root.find(2).unwrap().find(3).unwrap().kind, Kind::File(p) if p.len() == 1)
        );
        root.reparent(2, 1).unwrap();
        let before = serde_json::to_string(&root).unwrap();
        assert!(root.reparent(1, 2).is_err());
        assert!(root.reparent(2, 2).is_err());
        assert!(root.reparent(0, 1).is_err());
        assert!(root.reparent(2, 3).is_err());
        assert_eq!(serde_json::to_string(&root).unwrap(), before);
    }
}
