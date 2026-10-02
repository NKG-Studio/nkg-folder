//! Versioned, checksummed cache of shared paths and ready-to-query posting blocks.
use super::*;
use std::io::{self, Read, Write};

const MAGIC: &[u8; 8] = b"NKGIDX03";
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid search cache")
}

struct Output<W> {
    inner: W,
    hash: crc32fast::Hasher,
}
impl<W: Write> Output<W> {
    fn bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_all(bytes)?;
        self.hash.update(bytes);
        Ok(())
    }
    fn number(&mut self, value: usize) -> io::Result<()> {
        self.bytes(&u32::try_from(value).map_err(|_| invalid())?.to_le_bytes())
    }
    fn blob(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.number(bytes.len())?;
        self.bytes(bytes)
    }
}

struct Input<R> {
    inner: R,
    remaining: u64,
    hash: crc32fast::Hasher,
}
impl<R: Read> Input<R> {
    fn bytes(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len() as u64)
            .ok_or_else(invalid)?;
        self.inner.read_exact(bytes)?;
        self.hash.update(bytes);
        Ok(())
    }
    fn number(&mut self) -> io::Result<usize> {
        let mut bytes = [0; 4];
        self.bytes(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes) as usize)
    }
    fn blob(&mut self) -> io::Result<Vec<u8>> {
        let len = self.number()?;
        if len > 1024 * 1024 || len as u64 > self.remaining {
            return Err(invalid());
        }
        let mut bytes = vec![0; len];
        self.bytes(&mut bytes)?;
        Ok(bytes)
    }
    fn text(&mut self) -> io::Result<String> {
        String::from_utf8(self.blob()?).map_err(|_| invalid())
    }
}

#[cfg(windows)]
fn native(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}
#[cfg(windows)]
fn os_string(bytes: &[u8]) -> io::Result<OsString> {
    use std::os::windows::ffi::OsStringExt;
    if !bytes.len().is_multiple_of(2) {
        return Err(invalid());
    }
    Ok(OsString::from_wide(
        &bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect::<Vec<_>>(),
    ))
}
#[cfg(not(windows))]
fn native(value: &std::ffi::OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}
#[cfg(not(windows))]
fn os_string(bytes: &[u8]) -> io::Result<OsString> {
    Ok(OsString::from(
        std::str::from_utf8(bytes).map_err(|_| invalid())?,
    ))
}

impl Snapshot {
    pub fn write_cache(&self, writer: impl Write) -> io::Result<()> {
        let mut out = Output {
            inner: writer,
            hash: crc32fast::Hasher::new(),
        };
        out.bytes(MAGIC)?;
        out.number(if cfg!(windows) { 1 } else { 2 })?;
        out.blob(&serde_json::to_vec(&self.checkpoints).map_err(|_| invalid())?)?;
        let mut ids = HashMap::new();
        let mut parents = Vec::new();
        for item in self.iter() {
            if !ids.contains_key(&item.parent.path) {
                ids.insert(item.parent.path.clone(), parents.len());
                parents.push(&item.parent);
            }
        }
        out.number(parents.len())?;
        for parent in parents {
            out.blob(&native(parent.path.as_os_str()))?;
            out.blob(parent.folded.as_bytes())?;
        }
        out.number(self.blocks.len())?;
        for block in &self.blocks {
            out.number(block.items.len())?;
            for item in &block.items {
                if let Some(item) = item {
                    out.number(if item.directory { 2 } else { 1 })?;
                    out.number(ids[&item.parent.path])?;
                    out.blob(&native(&item.name))?;
                    out.blob(item.folded.as_bytes())?;
                } else {
                    out.number(0)?;
                }
            }
            out.number(block.postings.len())?;
            for (gram, range) in &block.postings {
                out.number(*gram as usize)?;
                out.number(range.len())?;
                let bytes: Vec<_> = block.ids[range.clone()]
                    .iter()
                    .flat_map(|id| id.to_le_bytes())
                    .collect();
                out.bytes(&bytes)?;
            }
        }
        out.inner.write_all(&out.hash.finalize().to_le_bytes())
    }

    pub fn read_cache(reader: impl Read, length: u64) -> io::Result<Self> {
        let mut input = Input {
            inner: reader,
            remaining: length.checked_sub(4).ok_or_else(invalid)?,
            hash: crc32fast::Hasher::new(),
        };
        let mut magic = [0; 8];
        input.bytes(&mut magic)?;
        if &magic != MAGIC || input.number()? != if cfg!(windows) { 1 } else { 2 } {
            return Err(invalid());
        }
        let checkpoints: Vec<Checkpoint> =
            serde_json::from_slice(&input.blob()?).map_err(|_| invalid())?;
        if checkpoints
            .iter()
            .any(|c| !c.root.is_absolute() || c.next < 0)
        {
            return Err(invalid());
        }
        let count = input.number()?;
        if count as u64 > input.remaining / 8 {
            return Err(invalid());
        }
        let mut parents = Vec::new();
        for _ in 0..count {
            let path = PathBuf::from(os_string(&input.blob()?)?);
            if !path.is_absolute() {
                return Err(invalid());
            }
            parents.push(Arc::new(Parent {
                path,
                folded: input.text()?,
            }));
        }
        let count = input.number()?;
        if count as u64 > input.remaining / 8 {
            return Err(invalid());
        }
        let mut snapshot = Snapshot {
            checkpoints,
            ..Snapshot::default()
        };
        let mut paths = std::collections::HashSet::new();
        for block_id in 0..count {
            let slots = input.number()?;
            if slots > BLOCK || (block_id + 1 < count && slots != BLOCK) {
                return Err(invalid());
            }
            let mut items = Vec::with_capacity(slots);
            for _ in 0..slots {
                let kind = input.number()?;
                if kind == 0 {
                    items.push(None);
                    continue;
                }
                if kind > 2 {
                    return Err(invalid());
                }
                let parent = parents.get(input.number()?).ok_or_else(invalid)?.clone();
                let name = os_string(&input.blob()?)?;
                if !name.is_empty()
                    && (Path::new(&name).components().count() != 1
                        || !matches!(
                            Path::new(&name).components().next(),
                            Some(std::path::Component::Normal(_))
                        ))
                {
                    return Err(invalid());
                }
                let folded = input.text()?;
                let full_folded = format!("{}{folded}", parent.folded);
                let item = Item {
                    parent,
                    name,
                    directory: kind == 2,
                    folded,
                    full_folded,
                };
                if !paths.insert(item.path()) {
                    return Err(invalid());
                }
                items.push(Some(Arc::new(item)));
                snapshot.count += 1;
            }
            let postings_count = input.number()?;
            if postings_count as u64 > input.remaining / 8 {
                return Err(invalid());
            }
            let mut block = Block {
                items,
                ..Block::default()
            };
            for _ in 0..postings_count {
                let gram = input.number()?;
                let len = input.number()?;
                if gram > 0xff_ffff || len > BLOCK || len as u64 > input.remaining / 2 {
                    return Err(invalid());
                }
                let start = block.ids.len();
                let mut previous = None;
                let mut bytes = vec![0u8; len * 2];
                input.bytes(&mut bytes)?;
                for bytes in bytes.chunks_exact(2) {
                    let id = u16::from_le_bytes([bytes[0], bytes[1]]);
                    if block.items.get(usize::from(id)).is_none_or(Option::is_none)
                        || previous.is_some_and(|p| p >= id)
                    {
                        return Err(invalid());
                    }
                    previous = Some(id);
                    block.ids.push(id);
                }
                if block
                    .postings
                    .insert(gram as u32, start..block.ids.len())
                    .is_some()
                {
                    return Err(invalid());
                }
            }
            block.common_text();
            snapshot.blocks.push(Arc::new(block));
        }
        if input.remaining != 0 {
            return Err(invalid());
        }
        let mut checksum = [0; 4];
        input.inner.read_exact(&mut checksum)?;
        if u32::from_le_bytes(checksum) != input.hash.finalize() {
            return Err(invalid());
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_and_corruption() {
        let mut parents = HashMap::new();
        let snapshot = Builder::from_items((0..30).map(|i| {
            Item::new(
                PathBuf::from(format!("C:/目录/{i}.txt")),
                false,
                &mut parents,
            )
        }))
        .publish();
        let mut bytes = Vec::new();
        snapshot.write_cache(&mut bytes).unwrap();
        let loaded = Snapshot::read_cache(bytes.as_slice(), bytes.len() as u64).unwrap();
        assert_eq!(
            loaded.iter().map(Item::path).collect::<Vec<_>>(),
            snapshot.iter().map(Item::path).collect::<Vec<_>>()
        );
        for block in 0..loaded.blocks.len() {
            assert_eq!(
                loaded.blocks[block].ids.len(),
                snapshot.blocks[block].ids.len()
            );
        }
        let mut builder = Builder::from_snapshot(loaded);
        builder.remove_subtrees(&[PathBuf::from("C:/目录")]);
        assert_eq!(builder.publish().count, 0);
        for cut in [0, 1, 8, bytes.len() / 2, bytes.len() - 1] {
            assert!(Snapshot::read_cache(&bytes[..cut], cut as u64).is_err());
        }
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(Snapshot::read_cache(bytes.as_slice(), bytes.len() as u64).is_err());
    }
}
