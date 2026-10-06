//! Disk-backed search. Only bounded scan batches and the first 500 hits enter Rust memory.
use super::*;
use rusqlite::{Connection, OpenFlags, params, params_from_iter};

const APPLICATION_ID: i64 = 0x4e4b4753;
const SCHEMA: &str = "
CREATE TABLE state(id INTEGER PRIMARY KEY CHECK(id=1), count INTEGER NOT NULL CHECK(count>=0),
 revision INTEGER NOT NULL, generation INTEGER NOT NULL, dirty INTEGER NOT NULL,
 roots TEXT NOT NULL, checkpoints TEXT NOT NULL);
INSERT INTO state VALUES(1,0,0,0,1,'[]','[]');
CREATE TABLE files(id INTEGER PRIMARY KEY, path BLOB NOT NULL UNIQUE, text TEXT NOT NULL,
 directory INTEGER NOT NULL, seen INTEGER NOT NULL);
CREATE VIRTUAL TABLE search USING fts5(text, content='files', content_rowid='id',
 tokenize='trigram case_sensitive 1');
CREATE TRIGGER files_ai AFTER INSERT ON files BEGIN
 INSERT INTO search(rowid,text) VALUES(new.id,new.text);
END;
CREATE TRIGGER files_ad AFTER DELETE ON files BEGIN
 INSERT INTO search(search,rowid,text) VALUES('delete',old.id,old.text);
END;
";

pub(super) struct Location {
    pub path: PathBuf,
    temporary: Option<tempfile::TempDir>,
}

#[cfg(test)]
impl Location {
    pub fn persistent(path: PathBuf) -> Self {
        Self {
            path,
            temporary: None,
        }
    }
}

impl Drop for Location {
    fn drop(&mut self) {
        if let Some(dir) = self.temporary.take() {
            let _ = crate::backend::user_io(|| dir.close().map_err(|e| e.to_string()));
        }
    }
}

// SQLite may open a WAL/journal or checkpoint on close, not just on Connection::open.
// Keep every SQLite operation (including destructors) at the GUI user's privilege level.
struct Db(Option<Connection>);

impl Db {
    fn open(path: &Path, reader: bool) -> Result<Self, String> {
        crate::backend::user_io(|| {
            let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
                | if reader {
                    OpenFlags::empty()
                } else {
                    OpenFlags::SQLITE_OPEN_CREATE
                };
            let conn = Connection::open_with_flags(path, flags).map_err(|e| e.to_string())?;
            conn.busy_timeout(Duration::from_secs(5))
                .map_err(|e| e.to_string())?;
            conn.execute_batch(
                "PRAGMA cache_size=-65536; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;",
            )
            .map_err(|e| e.to_string())?;
            if reader {
                conn.execute_batch("PRAGMA query_only=ON;")
                    .map_err(|e| e.to_string())?;
            }
            Ok(Self(Some(conn)))
        })
    }

    fn run<T>(
        &mut self,
        operation: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> Result<T, String> {
        crate::backend::user_io(|| operation(self.0.as_mut().unwrap()).map_err(|e| e.to_string()))
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Some(conn) = self.0.take() {
            let _ = crate::backend::user_io(|| {
                drop(conn);
                Ok(())
            });
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    pub count: usize,
    pub revision: u64,
    pub generation: i64,
    pub dirty: bool,
    pub roots: String,
    pub checkpoints: String,
}

fn state(conn: &Connection) -> rusqlite::Result<State> {
    conn.query_row(
        "SELECT count,revision,generation,dirty,roots,checkpoints FROM state WHERE id=1",
        [],
        |r| {
            Ok(State {
                count: r.get::<_, i64>(0)? as usize,
                revision: r.get::<_, i64>(1)? as u64,
                generation: r.get(2)?,
                dirty: r.get(3)?,
                roots: r.get(4)?,
                checkpoints: r.get(5)?,
            })
        },
    )
}

pub(super) fn key(path: &Path) -> Vec<u8> {
    let path: PathBuf = path.components().collect();
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
}

fn path(bytes: Vec<u8>) -> rusqlite::Result<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if !bytes.len().is_multiple_of(2) {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Blob,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid UTF-16 path",
                )),
            ));
        }
        Ok(OsString::from_wide(
            &bytes
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )
        .into())
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes).into())
    }
}

pub(super) fn roots_key(roots: &[PathBuf]) -> String {
    serde_json::to_string(&roots.iter().map(|p| key(p)).collect::<Vec<_>>()).unwrap()
}

pub(super) struct Writer {
    db: Db,
    pub location: Arc<Location>,
}

impl Writer {
    pub fn open(cache: Option<PathBuf>, stop: Arc<AtomicBool>) -> Result<Self, String> {
        let location = Arc::new(crate::backend::user_io(|| match cache {
            Some(path) => Ok(Location {
                path,
                temporary: None,
            }),
            None => {
                let dir = tempfile::Builder::new()
                    .prefix(".nkg-index-")
                    .tempdir()
                    .map_err(|e| e.to_string())?;
                Ok(Location {
                    path: dir.path().join("file-index.sqlite"),
                    temporary: Some(dir),
                })
            }
        })?);
        let mut db = Db::open(&location.path, false)?;
        db.run(|conn| {
            let id: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
            if tables == 0 && id == 0 && version == 0 {
                let tx = conn.transaction()?;
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "application_id", APPLICATION_ID)?;
                tx.pragma_update(None, "user_version", 1)?;
                tx.commit()?;
            } else if id != APPLICATION_ID || version != 1 {
                return Err(rusqlite::Error::InvalidParameterName("不支持的搜索数据库版本；请保留原文件并更换索引路径".into()));
            }
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=1000;")?;
            conn.progress_handler(10_000, Some(move || stop.load(Ordering::Relaxed)))?;
            Ok(())
        })?;
        Ok(Self { db, location })
    }

    pub fn state(&mut self) -> Result<State, String> {
        self.db.run(|conn| state(conn))
    }

    pub fn begin_scan(&mut self) -> Result<i64, String> {
        self.db.run(|conn| {
            conn.execute(
                "UPDATE state SET dirty=1,generation=generation+1 WHERE id=1",
                [],
            )?;
            Ok(state(conn)?.generation)
        })
    }

    pub fn insert(&mut self, items: Vec<Item>, generation: i64) -> Result<State, String> {
        self.db.run(|conn| {
            let tx = conn.transaction()?;
            let mut added = 0;
            {
                let mut insert = tx.prepare_cached("INSERT INTO files(path,text,directory,seen) VALUES(?1,?2,?3,?4) ON CONFLICT(path) DO NOTHING")?;
                let mut update = tx.prepare_cached("UPDATE files SET directory=?2,seen=?3 WHERE path=?1")?;
                for item in items {
                    let native = key(&item.path);
                    let text = item.path.to_string_lossy().replace('\\', "/").to_lowercase();
                    if insert.execute(params![native, text, item.directory, generation])? == 0 {
                        update.execute(params![native, item.directory, generation])?;
                    } else {
                        added += 1;
                    }
                }
            }
            tx.execute("UPDATE state SET count=count+?1,revision=revision+1 WHERE id=1", [added])?;
            let result = state(&tx)?;
            tx.commit()?;
            Ok(result)
        })
    }

    // Mark-and-sweep in disk transactions: keep existing search results during a rescan,
    // and remove stale entries only after enumeration finishes. A crash leaves dirty=1.
    pub fn prune(
        &mut self,
        roots: &[PathBuf],
        generation: i64,
        all: bool,
        mut publish: impl FnMut(State),
    ) -> Result<(), String> {
        let ranges = if all {
            vec![(None, None)]
        } else {
            let mut ranges = Vec::new();
            for root in roots {
                let exact = key(root);
                let mut exact_end = exact.clone();
                exact_end.push(0);
                ranges.push((Some(exact), Some(exact_end)));
                let mut prefix = key(root);
                #[cfg(windows)]
                let separator = [b'\\', 0].as_slice();
                #[cfg(not(windows))]
                let separator = [b'/'].as_slice();
                if !prefix.ends_with(separator) {
                    prefix.extend_from_slice(separator);
                }
                let mut end = prefix.clone();
                *end.last_mut().unwrap() += 1;
                ranges.push((Some(prefix), Some(end)));
            }
            ranges
        };
        for (lower, upper) in ranges {
            let mut cursor: Option<Vec<u8>> = None;
            loop {
                let (last, result) = self.db.run(|conn| {
                    let tx = conn.transaction()?;
                    let mut sql = String::from("SELECT id,path FROM files WHERE seen<>?1");
                    if lower.is_some() {
                        sql.push_str(" AND path>=?2 AND path<?3");
                    } else {
                        sql.push_str(" AND (?2 IS NULL AND ?3 IS NULL)");
                    }
                    if cursor.is_some() {
                        sql.push_str(" AND path>?4");
                    } else {
                        sql.push_str(" AND ?4 IS NULL");
                    }
                    sql.push_str(" ORDER BY path LIMIT 4096");
                    let rows = tx
                        .prepare(&sql)?
                        .query_map(params![generation, lower, upper, cursor], |r| {
                            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    if rows.is_empty() {
                        return Ok((None, None));
                    }
                    {
                        let mut remove = tx.prepare_cached("DELETE FROM files WHERE id=?1")?;
                        for (id, _) in &rows {
                            remove.execute([id])?;
                        }
                    }
                    tx.execute(
                        "UPDATE state SET count=count-?1,revision=revision+1 WHERE id=1",
                        [rows.len() as i64],
                    )?;
                    let result = state(&tx)?;
                    tx.commit()?;
                    Ok((rows.last().map(|r| r.1.clone()), Some(result)))
                })?;
                let Some(result) = result else {
                    break;
                };
                cursor = last;
                publish(result);
            }
        }
        Ok(())
    }

    pub fn finish(
        &mut self,
        roots: &[PathBuf],
        checkpoints: &[Checkpoint],
    ) -> Result<State, String> {
        let checkpoints = serde_json::to_string(checkpoints).map_err(|e| e.to_string())?;
        self.db.run(|conn| {
            conn.execute(
                "UPDATE state SET roots=?1,checkpoints=?2,dirty=0,revision=revision+1 WHERE id=1",
                params![roots_key(roots), checkpoints],
            )?;
            conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)")?;
            state(conn)
        })
    }
}

pub(super) struct Reader {
    db: Db,
    _location: Arc<Location>,
}

impl Reader {
    pub fn open(location: Arc<Location>) -> Result<Self, String> {
        Ok(Self {
            db: Db::open(&location.path, true)?,
            _location: location,
        })
    }

    pub fn find(
        &mut self,
        text: &str,
        ticket: u64,
        current: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        mut emit: impl FnMut(Results),
    ) -> Result<(), String> {
        let normalized = text.replace('\\', "/").to_lowercase();
        let mut words: Vec<_> = normalized.split_whitespace().collect();
        words.sort_unstable();
        words.dedup();
        let long: Vec<_> = words
            .iter()
            .filter(|w| w.chars().count() >= 3 && !w.contains('\0'))
            .collect();
        let mut args = Vec::new();
        let mut source = if long.is_empty() {
            String::from("files")
        } else {
            args.push(
                long.iter()
                    .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(" AND "),
            );
            String::from("search JOIN files ON files.id=search.rowid")
        };
        source.push_str(" WHERE ");
        if words.is_empty() {
            source.push('0');
        } else {
            if !long.is_empty() {
                source.push_str("search MATCH ? AND ");
            }
            source.push_str(&vec!["instr(files.text,?)>0"; words.len()].join(" AND "));
            args.extend(words.iter().map(|word| (*word).to_owned()));
        }
        let start = Instant::now();
        let is_current =
            || current.load(Ordering::Relaxed) == ticket && !stop.load(Ordering::Relaxed);
        let result = self.db.run(|conn| {
            let cancel = current.clone();
            let stopped = stop.clone();
            conn.progress_handler(
                10_000,
                Some(move || {
                    cancel.load(Ordering::Relaxed) != ticket || stopped.load(Ordering::Relaxed)
                }),
            )?;
            let result = (|| {
                let tx = conn.transaction()?;
                let revision = state(&tx)?.revision;
                let order = if long.is_empty() {
                    "files.id"
                } else {
                    "search.rowid"
                };
                let sql = format!(
                    "SELECT files.path,files.directory FROM {source} ORDER BY {order} LIMIT {LIMIT}"
                );
                let hits = tx
                    .prepare(&sql)?
                    .query_map(params_from_iter(args.iter()), |r| {
                        Ok(Hit::new(path(r.get(0)?)?, r.get(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut result = Results {
                    ticket,
                    revision,
                    total: hits.len(),
                    hits,
                    elapsed: start.elapsed(),
                    complete: true,
                };
                if result.hits.len() == LIMIT && is_current() {
                    result.complete = false;
                    emit(result.clone());
                    result.total = tx.query_row(
                        &format!("SELECT count(*) FROM {source}"),
                        params_from_iter(args.iter()),
                        |r| r.get::<_, i64>(0),
                    )? as usize;
                    result.complete = true;
                    result.elapsed = start.elapsed();
                }
                tx.commit()?;
                if is_current() {
                    emit(result);
                }
                Ok(())
            })();
            conn.progress_handler(0, None::<fn() -> bool>)?;
            result
        });
        if is_current() { result } else { Ok(()) }
    }

    #[cfg(test)]
    pub fn paths(&mut self) -> Vec<PathBuf> {
        self.db
            .run(|conn| {
                conn.prepare("SELECT path FROM files ORDER BY path")?
                    .query_map([], |r| path(r.get(0)?))?
                    .collect()
            })
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(reader: &mut Reader, text: &str) -> Results {
        let mut result = None;
        reader
            .find(
                text,
                1,
                Arc::new(AtomicU64::new(1)),
                Arc::new(AtomicBool::new(false)),
                |r| result = Some(r),
            )
            .unwrap();
        let result = result.unwrap();
        assert!(result.complete);
        result
    }

    #[test]
    fn sqlite_exact_queries_native_names_snapshot_and_cancellation() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut writer = Writer::open(None, stop.clone()).unwrap();
        let generation = writer.begin_scan().unwrap();
        let mut items: Vec<_> = (0..8000)
            .map(|i| {
                Item::new(
                    PathBuf::from(format!(
                        "C:/项目/Assets/group-{}/配置-file-{i}.meta",
                        i / 100
                    )),
                    false,
                )
            })
            .collect();
        for name in [
            "100%_done[1].txt",
            "a'bc.txt",
            "İSTANBUL.txt",
            "😀emoji.txt",
            "résumé.txt",
        ] {
            items.push(Item::new(PathBuf::from("C:/项目").join(name), false));
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let mut name: Vec<u16> = "C:/项目/native-".encode_utf16().collect();
            name.push(0xd800);
            name.extend(".txt".encode_utf16());
            items.push(Item::new(PathBuf::from(OsString::from_wide(&name)), false));
        }
        writer.insert(items.clone(), generation).unwrap();
        writer.finish(&[], &[]).unwrap();
        let mut reader = Reader::open(writer.location.clone()).unwrap();
        for text in [
            "",
            "  ",
            "配",
            "配置",
            "配置-file",
            ".meta",
            "ASSETS .META",
            "group-79/配置-file-7999",
            "C:\\项目\\Assets",
            "%_",
            "[1]",
            "a'bc",
            "İSTANBUL",
            "😀",
            "résumé",
            "native-",
            "不存在",
            "\" OR *",
        ] {
            let normalized = text.replace('\\', "/").to_lowercase();
            let words: Vec<_> = normalized.split_whitespace().collect();
            let expected: Vec<_> = items
                .iter()
                .filter(|item| {
                    !words.is_empty()
                        && words.iter().all(|word| {
                            item.path
                                .to_string_lossy()
                                .replace('\\', "/")
                                .to_lowercase()
                                .contains(word)
                        })
                })
                .map(|i| i.path.clone())
                .collect();
            let result = query(&mut reader, text);
            assert_eq!(result.total, expected.len(), "{text}");
            assert_eq!(
                result
                    .hits
                    .iter()
                    .map(|h| h.path.clone())
                    .collect::<Vec<_>>(),
                expected.into_iter().take(LIMIT).collect::<Vec<_>>(),
                "{text}"
            );
        }
        // A write between the first page and count must not mix two database revisions.
        let mut stages = Vec::new();
        reader
            .find(".meta", 1, Arc::new(AtomicU64::new(1)), stop.clone(), |r| {
                if !r.complete {
                    writer
                        .insert(vec![Item::new("C:/new.meta".into(), false)], generation)
                        .unwrap();
                }
                stages.push((r.complete, r.total));
            })
            .unwrap();
        assert_eq!(stages, vec![(false, LIMIT), (true, 8000)]);
        assert_eq!(query(&mut reader, ".meta").total, 8001);
        let ticket = Arc::new(AtomicU64::new(1));
        let mut emitted = 0;
        reader
            .find(".meta", 1, ticket.clone(), stop.clone(), |r| {
                assert!(!r.complete);
                emitted += 1;
                ticket.store(2, Ordering::Relaxed);
            })
            .unwrap();
        assert_eq!(emitted, 1);
        reader
            .find("配置", 1, ticket, stop, |_| {
                panic!("stale query published")
            })
            .unwrap();
        assert_eq!(query(&mut reader, "配置").total, 8000);
    }

    #[test]
    fn sqlite_bounded_sweep_dirty_restart_and_version_guard() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("index.sqlite");
        let stop = Arc::new(AtomicBool::new(false));
        let roots = vec![PathBuf::from("C:/root")];
        let mut writer = Writer::open(Some(cache.clone()), stop.clone()).unwrap();
        let generation = writer.begin_scan().unwrap();
        writer
            .insert(
                (0..9000)
                    .map(|i| Item::new(PathBuf::from(format!("C:/root/a/{i}.txt")), false))
                    .chain([Item::new("C:/root/ab/keep.txt".into(), false)])
                    .collect(),
                generation,
            )
            .unwrap();
        writer.finish(&roots, &[]).unwrap();
        let generation = writer.begin_scan().unwrap();
        writer
            .insert(
                vec![Item::new("C:/root/a/keep.txt".into(), false)],
                generation,
            )
            .unwrap();
        let mut batches = 0;
        writer
            .prune(&["C:/root/a".into()], generation, false, |_| batches += 1)
            .unwrap();
        assert!(batches >= 3);
        assert_eq!(writer.state().unwrap().count, 2);
        let location = writer.location.clone();
        drop(writer); // Interrupted before finish: next start must not trust the journal cursor.
        let mut reopened = Writer::open(Some(cache), stop).unwrap();
        assert!(reopened.state().unwrap().dirty);
        let mut reader = Reader::open(location).unwrap();
        assert_eq!(query(&mut reader, "ab/keep").total, 1);
        assert_eq!(query(&mut reader, "a/keep").total, 1);
        let generation = reopened.begin_scan().unwrap();
        reopened.prune(&[], generation, true, |_| {}).unwrap();
        reopened.finish(&roots, &[]).unwrap();
        assert_eq!(query(&mut reader, "keep").total, 0);
        assert!(!reopened.state().unwrap().dirty);
        let foreign = temp.path().join("foreign.sqlite");
        let db = Connection::open(&foreign).unwrap();
        db.execute_batch(
            "CREATE TABLE important(value TEXT); INSERT INTO important VALUES('keep');",
        )
        .unwrap();
        drop(db);
        assert!(Writer::open(Some(foreign.clone()), Arc::new(AtomicBool::new(false))).is_err());
        let db = Connection::open(foreign).unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM important", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "keep"
        );
    }

    #[test]
    #[ignore = "manual bounded-memory SQLite benchmark"]
    fn sqlite_million_entries() {
        let mut writer = Writer::open(None, Arc::new(AtomicBool::new(false))).unwrap();
        let generation = writer.begin_scan().unwrap();
        let started = Instant::now();
        for batch in 0..250 {
            writer
                .insert(
                    (batch * 4000..(batch + 1) * 4000)
                        .map(|i| {
                            Item::new(
                                PathBuf::from(format!(
                                    "C:/项目/Assets/group-{}/配置-file-{i}.meta",
                                    i / 100
                                )),
                                false,
                            )
                        })
                        .collect(),
                    generation,
                )
                .unwrap();
        }
        writer.finish(&[], &[]).unwrap();
        println!(
            "1M SQLite build: {:?}, count={}",
            started.elapsed(),
            writer.state().unwrap().count
        );
        let mut reader = Reader::open(writer.location.clone()).unwrap();
        for text in [
            "配置",
            ".meta",
            "file-999999",
            "group-9999/配置-file-999999",
        ] {
            let result = query(&mut reader, text);
            println!(
                "{text}: count={}, elapsed={:?}",
                result.total, result.elapsed
            );
            assert_eq!(
                result.total,
                if text == "配置" || text == ".meta" {
                    1_000_000
                } else {
                    1
                }
            );
        }
    }
}
