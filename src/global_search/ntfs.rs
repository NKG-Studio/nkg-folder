//! Optional, read-only NTFS acceleration. Unsupported/denied volumes use directory enumeration.
use super::*;
use std::{
    collections::HashSet,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
};
use windows::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{IO::DeviceIoControl, Ioctl::*},
};
use windows::core::PCWSTR;

fn open(root: &Path, read: bool) -> Option<fs::File> {
    let text = root.as_os_str().encode_wide().collect::<Vec<_>>();
    if text.len() != 3 || text[1] != b':' as u16 || ![b'/' as u16, b'\\' as u16].contains(&text[2])
    {
        return None;
    }
    let volume = format!("\\\\.\\{}:", char::from_u32(text[0] as u32)?);
    fs::OpenOptions::new()
        .access_mode(if read { GENERIC_READ.0 } else { 0 })
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .open(volume)
        .ok()
}

fn control<T>(
    file: &fs::File,
    code: u32,
    input: &T,
    output: &mut [u8],
) -> windows::core::Result<usize> {
    let mut bytes = 0;
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            code,
            Some(std::ptr::from_ref(input).cast()),
            std::mem::size_of::<T>() as u32,
            Some(output.as_mut_ptr().cast()),
            output.len() as u32,
            Some(&mut bytes),
            None,
        )?;
    }
    Ok(bytes as usize)
}

pub(super) fn concurrency(root: &Path) -> usize {
    let Some(file) = open(root, false) else {
        return 1;
    };
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut result = [0u8; 12];
    if control(&file, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut result).is_ok_and(|len| len >= 9)
        && result[8] == 0
    {
        4
    } else {
        1
    }
}

fn state(root: &Path, file: &fs::File) -> Option<(Checkpoint, i64)> {
    let mut result = [0u8; 80];
    let size = control(file, FSCTL_QUERY_USN_JOURNAL, &[0u8; 0], &mut result).ok()?;
    if size < 56 {
        return None;
    }
    let mut volume = 0;
    let wide: Vec<_> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            Some(&mut volume),
            None,
            None,
            None,
        )
        .ok()?;
    }
    let number = |start| i64::from_le_bytes(result[start..start + 8].try_into().unwrap());
    Some((
        Checkpoint {
            root: root.to_owned(),
            volume,
            journal: u64::from_le_bytes(result[..8].try_into().unwrap()),
            next: number(16),
        },
        number(8).max(number(24)),
    ))
}

pub(super) fn checkpoint(root: &Path) -> Option<Checkpoint> {
    state(root, &open(root, true)?).map(|s| s.0)
}

struct Record {
    #[cfg(test)]
    id: u64,
    parent: u64,
    reason: u32,
    #[cfg(test)]
    attributes: u32,
    #[cfg(test)]
    name: OsString,
}

fn records(bytes: &[u8]) -> Option<Vec<Record>> {
    let mut offset = 0usize;
    let mut records = Vec::new();
    while offset < bytes.len() {
        let header = bytes.get(offset..offset.checked_add(60)?)?;
        let length = u32::from_le_bytes(header[..4].try_into().ok()?) as usize;
        let version = u16::from_le_bytes(header[4..6].try_into().ok()?);
        if version != 2 || length < 60 || !length.is_multiple_of(8) {
            return None;
        }
        let record = bytes.get(offset..offset.checked_add(length)?)?;
        let len = u16::from_le_bytes(header[56..58].try_into().ok()?) as usize;
        let start = u16::from_le_bytes(header[58..60].try_into().ok()?) as usize;
        if start < 60 || !len.is_multiple_of(2) || len == 0 || !start.is_multiple_of(2) {
            return None;
        }
        let chars: Vec<_> = record
            .get(start..start.checked_add(len)?)?
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        if chars.contains(&0) {
            return None;
        }
        let name = OsString::from_wide(&chars);
        if name != "."
            && (Path::new(&name).components().count() != 1
                || !matches!(
                    Path::new(&name).components().next(),
                    Some(std::path::Component::Normal(_))
                ))
        {
            return None;
        }
        records.push(Record {
            #[cfg(test)]
            id: u64::from_le_bytes(header[8..16].try_into().ok()?),
            parent: u64::from_le_bytes(header[16..24].try_into().ok()?),
            reason: u32::from_le_bytes(header[40..44].try_into().ok()?),
            #[cfg(test)]
            attributes: u32::from_le_bytes(header[52..56].try_into().ok()?),
            #[cfg(test)]
            name,
        });
        offset += length;
    }
    Some(records)
}

fn directory_path(file: &fs::File, id: u64, root: &Path) -> Option<PathBuf> {
    let descriptor = FILE_ID_DESCRIPTOR {
        dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: id as i64 },
    };
    let handle = unsafe {
        OpenFileById(
            HANDLE(file.as_raw_handle()),
            &descriptor,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
        .ok()?
    };
    let file = unsafe { fs::File::from_raw_handle(handle.0) };
    let mut path = vec![0u16; 32768];
    let len = unsafe {
        GetFinalPathNameByHandleW(
            HANDLE(file.as_raw_handle()),
            &mut path,
            GETFINALPATHNAMEBYHANDLE_FLAGS(0),
        )
    } as usize;
    if len == 0 || len >= path.len() {
        return None;
    }
    path.truncate(len);
    let path = if path.starts_with(&[92, 92, 63, 92]) {
        &path[4..]
    } else {
        &path
    };
    let path = PathBuf::from(OsString::from_wide(path));
    path.starts_with(root).then_some(path)
}

/// Replay from the checkpoint taken BEFORE indexing. Rescan affected parent directories,
/// covering every hard-link name rather than guessing a file's one canonical name.
pub(super) fn changes(old: &Checkpoint, stop: &AtomicBool) -> Option<(Vec<PathBuf>, Checkpoint)> {
    let file = open(&old.root, true)?;
    let (now, first) = state(&old.root, &file)?;
    if !compatible(old, &now, first) {
        return None;
    }
    let mask = USN_REASON_FILE_CREATE
        | USN_REASON_FILE_DELETE
        | USN_REASON_RENAME_OLD_NAME
        | USN_REASON_RENAME_NEW_NAME
        | USN_REASON_HARD_LINK_CHANGE
        | USN_REASON_REPARSE_POINT_CHANGE
        | USN_REASON_SECURITY_CHANGE;
    let mut request = READ_USN_JOURNAL_DATA_V0 {
        StartUsn: old.next,
        ReasonMask: mask,
        UsnJournalID: old.journal,
        ..Default::default()
    };
    let mut buffer = vec![0; 1024 * 1024];
    let mut parents = HashSet::new();
    while request.StartUsn < now.next {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let len = control(&file, FSCTL_READ_USN_JOURNAL, &request, &mut buffer).ok()?;
        if len < 8 || len > buffer.len() {
            return None;
        }
        let next = i64::from_le_bytes(buffer[..8].try_into().ok()?);
        if next <= request.StartUsn {
            return None;
        }
        for record in records(&buffer[8..len])? {
            // A hard-link change can concern a different directory than the emitted name.
            // A security/reparse change can change reachability. Reconcile the volume safely.
            if record.reason
                & (USN_REASON_HARD_LINK_CHANGE
                    | USN_REASON_SECURITY_CHANGE
                    | USN_REASON_REPARSE_POINT_CHANGE)
                != 0
            {
                return Some((vec![old.root.clone()], now));
            }
            parents.insert(record.parent);
        }
        request.StartUsn = next;
    }
    let mut paths = Vec::new();
    for parent in parents {
        let Some(path) = directory_path(&file, parent, &old.root) else {
            return Some((vec![old.root.clone()], now));
        };
        paths.push(path);
    }
    // Resolve paths before recording progress; events since `now` are deliberately replayed again.
    Some((paths, now))
}

fn compatible(old: &Checkpoint, now: &Checkpoint, first: i64) -> bool {
    now.volume == old.volume
        && now.journal == old.journal
        && old.next >= first
        && old.next <= now.next
}

/// MFT accelerates directory discovery, but normal directory enumeration supplies entries:
/// USN_RECORD contains only one name and cannot by itself preserve all hard-link aliases.
#[cfg(test)]
pub(super) fn directories(root: &Path, stop: &AtomicBool) -> Option<Vec<PathBuf>> {
    let file = open(root, true)?;
    // Reserved MFT entries, including the volume root, may be absent from enumeration.
    let root_handle = fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(root)
        .ok()?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(root_handle.as_raw_handle()), &mut info).ok()?;
    }
    let root_id = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    let mut request = MFT_ENUM_DATA_V0 {
        HighUsn: i64::MAX,
        ..Default::default()
    };
    let mut buffer = vec![0; 1024 * 1024];
    let mut dirs = HashMap::new();
    loop {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let len = match control(&file, FSCTL_ENUM_USN_DATA, &request, &mut buffer) {
            Ok(len) => len,
            Err(error)
                if error.code() == windows::core::HRESULT::from_win32(ERROR_HANDLE_EOF.0) =>
            {
                break;
            }
            Err(_) => return None,
        };
        if len < 8 || len > buffer.len() {
            return None;
        }
        let next = u64::from_le_bytes(buffer[..8].try_into().ok()?);
        if next <= request.StartFileReferenceNumber {
            return None;
        }
        for record in records(&buffer[8..len])? {
            if record.attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                dirs.insert(record.id, record);
            }
        }
        request.StartFileReferenceNumber = next;
    }
    dirs.insert(
        root_id,
        Record {
            id: root_id,
            parent: root_id,
            reason: 0,
            attributes: FILE_ATTRIBUTE_DIRECTORY.0,
            name: ".".into(),
        },
    );
    resolve_directories(&dirs, root)
}

#[cfg(test)]
fn resolve_directories(dirs: &HashMap<u64, Record>, root: &Path) -> Option<Vec<PathBuf>> {
    let root_id = *dirs
        .iter()
        .find(|(_, record)| record.id == record.parent && record.name == ".")?
        .0;
    let mut paths = HashMap::from([(root_id, Some(root.to_owned()))]);
    for &id in dirs.keys() {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut current = id;
        while !paths.contains_key(&current) {
            if !seen.insert(current) {
                return None;
            }
            let Some(record) = dirs.get(&current) else {
                paths.insert(current, None);
                break;
            };
            if record.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                || (record.id & 0x0000_ffff_ffff_ffff) < 16
            {
                paths.insert(current, None);
                break;
            }
            chain.push(current);
            current = record.parent;
        }
        for child in chain.into_iter().rev() {
            let record = &dirs[&child];
            let path = paths
                .get(&record.parent)
                .and_then(|p| p.as_ref())
                .map(|parent| parent.join(&record.name));
            paths.insert(child, path);
        }
    }
    Some(paths.into_values().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an NTFS volume with MFT/USN read access; creates only temporary fixtures"]
    fn ntfs_live_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().ancestors().last().unwrap();
        let before = checkpoint(root).expect("NTFS live acceptance requires MFT/USN volume read access; the ordinary search fallback remains available");
        let directory = temp.path().join("visible");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("source.txt"), "fixture").unwrap();
        let discovered = directories(root, &AtomicBool::new(false))
            .expect("MFT directory enumeration unavailable");
        assert!(
            discovered.contains(&directory),
            "MFT omitted the new test directory"
        );
        let (paths, checkpoint) =
            changes(&before, &AtomicBool::new(false)).expect("USN replay unavailable");
        assert!(
            paths.iter().any(|path| directory.starts_with(path)),
            "USN replay did not cover the created directory"
        );
        let renamed = temp.path().join("renamed");
        fs::rename(directory, &renamed).unwrap();
        fs::hard_link(renamed.join("source.txt"), temp.path().join("alias.txt")).unwrap();
        let (paths, _) = changes(&checkpoint, &AtomicBool::new(false))
            .expect("USN replay after rename unavailable");
        assert!(
            paths.iter().any(|path| temp.path().starts_with(path)),
            "USN replay did not cover rename/hard-link changes"
        );
    }

    #[test]
    fn journal_reset_and_directory_graph_are_safe() {
        let old = Checkpoint {
            root: PathBuf::from("C:/"),
            volume: 10,
            journal: 20,
            next: 30,
        };
        let mut now = old.clone();
        now.next = 40;
        assert!(compatible(&old, &now, 30));
        assert!(!compatible(&old, &now, 31));
        now.next = 29;
        assert!(!compatible(&old, &now, 0));
        now.next = 40;
        now.journal += 1;
        assert!(!compatible(&old, &now, 0));
        now.journal -= 1;
        now.volume += 1;
        assert!(!compatible(&old, &now, 0));
        let record = |id, parent, name: &str, link| Record {
            id,
            parent,
            name: name.into(),
            reason: 0,
            attributes: FILE_ATTRIBUTE_DIRECTORY.0
                | if link {
                    FILE_ATTRIBUTE_REPARSE_POINT.0
                } else {
                    0
                },
        };
        let mut dirs: HashMap<_, _> = [
            record(5, 5, ".", false),
            record(16, 5, "a", false),
            record(17, 16, "b", false),
            record(18, 5, "link", true),
            record(19, 18, "outside", false),
            record(20, 999, "orphan", false),
        ]
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
        let mut paths = resolve_directories(&dirs, &old.root).unwrap();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("C:/"),
                PathBuf::from("C:/a"),
                PathBuf::from("C:/a/b")
            ]
        );
        dirs.insert(21, record(21, 22, "cycle1", false));
        dirs.insert(22, record(22, 21, "cycle2", false));
        assert!(resolve_directories(&dirs, &old.root).is_none());
    }
    #[test]
    fn record_validation_and_normal_privilege_fallback() {
        let mut record = vec![0u8; 72];
        record[..4].copy_from_slice(&72u32.to_le_bytes());
        record[4..6].copy_from_slice(&2u16.to_le_bytes());
        record[56..58].copy_from_slice(&2u16.to_le_bytes());
        record[58..60].copy_from_slice(&60u16.to_le_bytes());
        record[60..62].copy_from_slice(&(b'x' as u16).to_le_bytes());
        assert_eq!(records(&record).unwrap()[0].name, "x");
        for cut in 1..record.len() {
            assert!(records(&record[..cut]).is_none());
        }
        record[4] = 3;
        assert!(records(&record).is_none());
        record[4] = 2;
        record[58] = 70;
        record[56] = 8;
        assert!(records(&record).is_none());
        let temp = tempfile::tempdir().unwrap();
        assert!(checkpoint(temp.path()).is_none());
        assert!(directories(temp.path(), &AtomicBool::new(false)).is_none());
        for root in local_roots() {
            if let Some(file) = open(&root, false) {
                let mut output = vec![0u8; 1024 * 1024];
                let request = MFT_ENUM_DATA_V0 {
                    HighUsn: i64::MAX,
                    ..Default::default()
                };
                println!(
                    "{} least-access probe: journal={}, MFT={:?}",
                    root.display(),
                    state(&root, &file).is_some(),
                    control(&file, FSCTL_ENUM_USN_DATA, &request, &mut output)
                        .map_err(|e| e.code())
                );
            }
            println!(
                "{}: USN available={}, scan concurrency={}",
                root.display(),
                checkpoint(&root).is_some(),
                concurrency(&root)
            );
        }
    }
}
