//! Ubuntu's resident received-file cache. A clipboard/drag advertises metadata
//! first; URI readers materialize only after the source grants actual body reads.
use super::{
    ReaderGuard,
    namespace::{Namespace, NativeFiles, NativeLease, StoredFile},
    reason, unavailable,
};
use crate::model::{
    ContentSize, DIRECTORY_PAGE_ENTRIES, DirectoryCursor, DirectoryPage, FileNode, MAX_DISCOVERED_NODES, NodeId,
    NodeKind, Unavailable,
};
use crate::{FileEntry, FileMetadata, FileReader, NativeSpec};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, UNIX_EPOCH},
};

const MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Item {
    node: FileNode,
    path: String,
    size: u64,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Catalog {
    items: Vec<Item>,
    roots: Vec<FileEntry>,
    bytes: u64,
}

/// Held by the native MIME loader. Its lock prevents cache cleanup while a
/// clipboard owner, drag, network transfer or native reader still uses this batch.
pub struct Receiver {
    owner: Weak<Receiver>,
    namespace: Arc<dyn Namespace>,
    guard: ReaderGuard,
    root: PathBuf,
    _lock: File,
    serial: Mutex<()>,
    catalog: Mutex<Option<Catalog>>,
    failure: Mutex<Option<Unavailable>>,
    descriptor: Mutex<Option<super::linked::Descriptor>>,
    observed: Mutex<HashMap<NodeId, FileNode>>,
}
impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UbuntuReceivedFiles")
            .field("ready", &self.catalog.lock().unwrap().is_some())
            .finish()
    }
}
impl Receiver {
    pub fn for_spec(spec: &NativeSpec, guard: ReaderGuard) -> io::Result<Arc<Self>> {
        let namespace = spec
            .file_contents
            .as_ref()
            .and_then(|f| f.namespace())
            .or_else(|| spec.contents.namespace())
            .ok_or_else(|| unavailable(Unavailable::Unsupported))?;
        Self::new_at(
            namespace,
            guard,
            &oink_foundation::paths::cache_dir().join("ReceivedFiles"),
        )
    }
    fn new_at(namespace: Arc<dyn Namespace>, guard: ReaderGuard, cache: &Path) -> io::Result<Arc<Self>> {
        guard()?;
        private_dir(cache)?;
        cleanup(cache);
        let root = loop {
            let path = cache.join(crate::store::BatchId::random()?.name());
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => break path,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join(".lock"))?;
        // SAFETY: flock borrows this owned, live file descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let receiver = Arc::new_cyclic(|owner| Self {
            owner: owner.clone(),
            namespace: namespace.clone(),
            guard,
            root,
            _lock: lock,
            serial: Mutex::new(()),
            catalog: Mutex::new(None),
            failure: Mutex::new(None),
            descriptor: Mutex::new(None),
            observed: Mutex::new(namespace.roots().into_iter().map(|node| (node.id, node)).collect()),
        });
        let native: Arc<dyn NativeFiles> = receiver.clone();
        namespace.bind_native(Arc::downgrade(&native))?;
        Ok(receiver)
    }
    fn check(&self, extra: &ReaderGuard) -> io::Result<()> {
        (self.guard)()?;
        extra()
    }
    /// Idempotent across simultaneous selection requests and the delivery owner.
    /// Runs on ordinary workers; it must never be called from the X11 event loop.
    pub fn entries(&self) -> io::Result<Vec<FileEntry>> {
        let catalog = self.ensure(&(Arc::new(|| Ok(())) as ReaderGuard), &|_, _| {})?;
        Ok(catalog.roots)
    }
    pub fn entries_with_guard(&self, extra: ReaderGuard) -> io::Result<Vec<FileEntry>> {
        self.ensure(&extra, &|_, _| {}).map(|catalog| catalog.roots)
    }
    fn ensure(&self, extra: &ReaderGuard, progress: &(dyn Fn(u64, u64) + Send + Sync)) -> io::Result<Catalog> {
        self.check(extra)?;
        let _serial = loop {
            self.check(extra)?;
            match self.serial.try_lock() {
                Ok(lock) => break lock,
                Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(unavailable(Unavailable::Cancelled)),
            }
        };
        self.check(extra)?;
        if let Some(catalog) = self.catalog.lock().unwrap().clone() {
            self.check(extra)?;
            return Ok(catalog);
        }
        if let Some(failure) = *self.failure.lock().unwrap() {
            return Err(unavailable(failure));
        }
        let result = self.copy(extra, progress);
        match result {
            Ok(catalog) => {
                *self.catalog.lock().unwrap() = Some(catalog.clone());
                Ok(catalog)
            }
            Err(error) if reason(&error) == Unavailable::Permission && !self.root.join("files").exists() => {
                // A toolkit may inspect URI data during Hover. Preserve the
                // metadata-only receiver so a later authorized Drop can retry.
                Err(error)
            }
            Err(error) => {
                // No URI was published: removing this private incomplete batch
                // cannot invalidate files visible to an application.
                *self.failure.lock().unwrap() = Some(reason(&error));
                let _ = fs::remove_dir_all(&self.root);
                Err(error)
            }
        }
    }
    fn copy(&self, extra: &ReaderGuard, progress: &(dyn Fn(u64, u64) + Send + Sync)) -> io::Result<Catalog> {
        self.check(extra)?;
        self.namespace.prepare_while(&|| self.check(extra).is_ok())?;
        // Hover credentials grant metadata only. Never prefetch bodies merely
        // because a pointer crossed a screen boundary.
        if self.namespace.source_link().is_some_and(|d| d.provisional) {
            return Err(unavailable(Unavailable::Permission));
        }
        self.namespace.start_native_delivery();
        let advertised = self.namespace.roots();
        if advertised.is_empty() {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let mut names = super::namespace::Names::new(crate::sync::files::TargetOs::current());
        let mut queue: VecDeque<_> = advertised.into_iter().map(|n| (n, 0usize)).collect();
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        let mut roots = Vec::new();
        let mut bytes = 0u64;
        let files_root = self.root.join("files");
        private_dir(&files_root)?;
        let mut links = Vec::new();
        while let Some((advertised, depth)) = queue.pop_front() {
            self.check(extra)?;
            if depth > 256 || items.len() >= MAX_DISCOVERED_NODES || !seen.insert(advertised.id) || !advertised.valid()
            {
                return Err(unavailable(Unavailable::InvalidData));
            }
            let node = if advertised.kind == NodeKind::File {
                self.namespace.select(advertised.id)?
            } else {
                advertised.clone()
            };
            if node.id != advertised.id
                || node.parent != advertised.parent
                || node.name != advertised.name
                || node.kind != advertised.kind
            {
                return Err(unavailable(Unavailable::Superseded));
            }
            self.observed.lock().unwrap().insert(node.id, node.clone());
            let mut entry = names.entry(&node)?;
            let relative = Path::new(&entry.path);
            if relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(unavailable(Unavailable::InvalidData));
            }
            let destination = files_root.join(relative);
            let size = match &node.kind {
                NodeKind::Directory => {
                    fs::DirBuilder::new().mode(0o700).create(&destination)?;
                    let mut cursor = None;
                    loop {
                        self.check(extra)?;
                        let page = self.namespace.list(node.id, cursor)?;
                        page.validate(cursor).map_err(unavailable)?;
                        if page.directory != node.id {
                            return Err(unavailable(Unavailable::InvalidData));
                        }
                        if queue
                            .len()
                            .saturating_add(items.len())
                            .saturating_add(page.entries.len())
                            > MAX_DISCOVERED_NODES
                        {
                            return Err(unavailable(Unavailable::LimitExceeded));
                        }
                        queue.extend(page.entries.into_iter().map(|n| (n, depth + 1)));
                        cursor = page.next;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    0
                }
                NodeKind::Symlink(target) => {
                    if !oink_foundation::content::safe_symlink_target(
                        target,
                        entry.path.split('/').count().saturating_sub(1),
                    ) {
                        return Err(unavailable(Unavailable::InvalidData));
                    }
                    // Create links after every directory/file, so a source can
                    // never make a later write follow one outside the batch.
                    links.push((destination.clone(), target.clone()));
                    0
                }
                NodeKind::File => {
                    if matches!(node.size, ContentSize::Known(n) if n > MAX_BYTES.saturating_sub(bytes)) {
                        return Err(unavailable(Unavailable::LimitExceeded));
                    }
                    let mut writer = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                        .open(&destination)?;
                    let mut reader = super::terminal_reader(self.namespace.open(node.id, node.version, 0)?);
                    let mut count = 0u64;
                    let mut buffer = [0u8; 64 * 1024];
                    loop {
                        self.check(extra)?;
                        let size = reader.read(&mut buffer)?;
                        self.check(extra)?;
                        if size == 0 {
                            break;
                        }
                        count = count
                            .checked_add(size as u64)
                            .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
                        if count > MAX_BYTES.saturating_sub(bytes)
                            || matches!(node.size, ContentSize::Known(expected) if count > expected)
                        {
                            return Err(unavailable(Unavailable::LimitExceeded));
                        }
                        writer.write_all(&buffer[..size])?;
                        progress(
                            bytes + count,
                            items
                                .iter()
                                .filter(|i: &&Item| i.node.kind != NodeKind::Directory)
                                .count() as u64,
                        );
                    }
                    if matches!(node.size, ContentSize::Known(expected) if expected != count) {
                        return Err(unavailable(Unavailable::Superseded));
                    }
                    if let Some(ms) = node.modified_ms {
                        let time = if ms >= 0 {
                            UNIX_EPOCH.checked_add(Duration::from_millis(ms as u64))
                        } else {
                            UNIX_EPOCH.checked_sub(Duration::from_millis(ms.unsigned_abs()))
                        };
                        writer.set_modified(time.ok_or_else(|| unavailable(Unavailable::InvalidData))?)?;
                    }
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mode = writer.metadata()?.mode();
                        writer.set_permissions(fs::Permissions::from_mode(
                            (mode & !0o111) | u32::from(node.exec_bits.unwrap_or(0) & 0o111),
                        ))?;
                    }
                    writer.sync_all()?;
                    bytes += count;
                    count
                }
            };
            entry.local_path = Some(destination);
            entry.size = size;
            if node.parent.is_none() {
                roots.push(entry.clone());
            }
            items.push(Item {
                node,
                path: entry.path,
                size,
            });
        }
        validate_links(&items)?;
        for (path, target) in links {
            self.check(extra)?;
            std::os::unix::fs::symlink(target, path)?;
        }
        self.check(extra)?;
        let catalog = Catalog { items, roots, bytes };
        atomic_json(&self.root, ".completed.json", &catalog)?;
        File::open(&self.root)?.sync_all()?;
        progress(
            bytes,
            catalog
                .items
                .iter()
                .filter(|i| i.node.kind != NodeKind::Directory)
                .count() as u64,
        );
        Ok(catalog)
    }
}
impl NativeFiles for Receiver {
    fn lease(&self) -> io::Result<NativeLease> {
        Ok(Box::new(
            self.owner
                .upgrade()
                .ok_or_else(|| unavailable(Unavailable::Cancelled))?,
        ))
    }
    fn persist_link(&self, descriptor: &super::linked::Descriptor) -> io::Result<()> {
        descriptor.validate()?;
        let mut current = self.descriptor.lock().unwrap();
        // Persist private source authority before permitting any body read.
        atomic_json(&self.root, ".source.json", descriptor)?;
        *current = Some(descriptor.clone());
        Ok(())
    }
    fn open(&self, node: NodeId, offset: u64) -> io::Result<FileReader> {
        let catalog = self.ensure(&(Arc::new(|| Ok(())) as ReaderGuard), &|_, _| {})?;
        let item = catalog
            .items
            .iter()
            .find(|i| i.node.id == node)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        if item.node.kind != NodeKind::File || offset > item.size {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.root.join("files").join(&item.path))?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(file))
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        if let Some(catalog) = self.catalog.lock().unwrap().as_ref() {
            let directory = catalog
                .items
                .iter()
                .find(|i| i.node.id == node && i.node.kind == NodeKind::Directory)
                .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
            let version = directory.node.version;
            if cursor.is_some_and(|c| c.version != version) {
                return Err(unavailable(Unavailable::Superseded));
            }
            let offset = cursor.map_or(0, |c| c.offset as usize);
            let children: Vec<_> = catalog
                .items
                .iter()
                .filter(|i| i.node.parent == Some(node))
                .map(|i| i.node.clone())
                .collect();
            if offset > children.len() {
                return Err(unavailable(Unavailable::InvalidData));
            }
            let end = (offset + DIRECTORY_PAGE_ENTRIES.min(128)).min(children.len());
            return Ok(DirectoryPage {
                directory: node,
                version,
                entries: children[offset..end].to_vec(),
                next: (end < children.len()).then_some(DirectoryCursor {
                    version,
                    offset: end as u32,
                }),
            });
        }
        let page = self.namespace.list(node, cursor)?;
        for child in &page.entries {
            self.observed.lock().unwrap().insert(child.id, child.clone());
        }
        Ok(page)
    }
    fn symlink_target(&self, node: NodeId) -> io::Result<String> {
        if let Some(catalog) = self.catalog.lock().unwrap().as_ref() {
            return catalog
                .items
                .iter()
                .find(|i| i.node.id == node)
                .and_then(|i| i.node.kind.symlink_target())
                .map(String::from)
                .ok_or_else(|| unavailable(Unavailable::NoSuchItem));
        }
        self.observed
            .lock()
            .unwrap()
            .get(&node)
            .and_then(|node| node.kind.symlink_target())
            .map(String::from)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))
    }
    fn metadata(&self, node: NodeId) -> io::Result<FileMetadata> {
        if let Some(catalog) = self.catalog.lock().unwrap().as_ref() {
            let item = catalog
                .items
                .iter()
                .find(|i| i.node.id == node)
                .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
            return Ok(FileMetadata {
                exec_bits: item.node.exec_bits,
                size: item.size,
                modified_ms: item.node.modified_ms,
            });
        }
        let advertised = self
            .observed
            .lock()
            .unwrap()
            .get(&node)
            .cloned()
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        let selected = if advertised.kind == NodeKind::File {
            self.namespace.select(node)?
        } else {
            advertised
        };
        let size = match selected.size {
            ContentSize::Known(size) => size,
            ContentSize::Unknown if selected.kind != NodeKind::File => 0,
            _ => return Err(unavailable(Unavailable::InvalidData)),
        };
        Ok(FileMetadata {
            exec_bits: selected.exec_bits,
            size,
            modified_ms: selected.modified_ms,
        })
    }
    fn download(
        &self,
        check: &ReaderGuard,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> io::Result<Vec<StoredFile>> {
        let catalog = self.ensure(check, progress)?;
        Ok(catalog
            .items
            .into_iter()
            .filter(|i| i.node.kind != NodeKind::Directory)
            .map(|i| StoredFile {
                node: i.node.id,
                version: i.node.version,
                size: i.size,
            })
            .collect())
    }
}
fn private_dir(path: &Path) -> io::Result<()> {
    let mut options = fs::DirBuilder::new();
    options.recursive(true).mode(0o700);
    options.create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no pointers or mutable process state.
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "received-file cache must be a private owned directory",
        ));
    }
    Ok(())
}
fn atomic_json(root: &Path, name: &str, value: &impl serde::Serialize) -> io::Result<()> {
    let temporary = root.join(format!(".tmp-{}", crate::store::BatchId::random()?.name()));
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    serde_json::to_writer(&mut writer, value)?;
    writer.sync_all()?;
    fs::rename(&temporary, root.join(name))?;
    File::open(root)?.sync_all()
}
fn cleanup(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() != 32 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age < RETENTION)
        {
            continue;
        }
        let Ok(lock) = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path.join(".lock"))
        else {
            continue;
        };
        // SAFETY: owned lock fd; unlocked expired batches have no native owner.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tree_is_real_idempotent_private_and_preserves_bytes() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("folder")).unwrap();
        fs::write(source.path().join("folder/你好.txt"), b"real-file-contents").unwrap();
        fs::write(source.path().join("folder/empty"), b"").unwrap();
        let namespace = super::super::namespace::Local::new(vec![(NodeId(1), source.path().join("folder"))]).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let receiver = Receiver::new_at(namespace, Arc::new(|| Ok(())), &cache.path().join("received")).unwrap();
        let a = receiver.entries().unwrap();
        let b = receiver.entries().unwrap();
        assert_eq!(a, b);
        let path = a[0].local_path.as_ref().unwrap();
        assert_eq!(fs::read(path.join("你好.txt")).unwrap(), b"real-file-contents");
        assert_eq!(fs::metadata(path.join("empty")).unwrap().len(), 0);
        assert_eq!(fs::metadata(receiver.root.join(".source.json")).is_ok(), false);
        assert_eq!(fs::metadata(&receiver.root).unwrap().mode() & 0o077, 0);
    }
    #[test]
    fn cancelled_policy_publishes_no_paths_or_partial_body() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("file"), b"secret").unwrap();
        let namespace = super::super::namespace::Local::new(vec![(NodeId(1), source.path().join("file"))]).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let check = allowed.clone();
        let receiver = Receiver::new_at(
            namespace,
            Arc::new(move || {
                if check.load(std::sync::atomic::Ordering::Acquire) {
                    Ok(())
                } else {
                    Err(unavailable(Unavailable::Cancelled))
                }
            }),
            &cache.path().join("received"),
        )
        .unwrap();
        allowed.store(false, std::sync::atomic::Ordering::Release);
        assert!(receiver.entries().is_err());
        assert!(!receiver.root.join("files/file").exists());
    }
}

fn validate_links(items: &[Item]) -> io::Result<()> {
    let links: HashMap<String, String> = items
        .iter()
        .filter_map(|item| {
            item.node
                .kind
                .symlink_target()
                .map(|target| (item.path.clone(), target.to_owned()))
        })
        .collect();
    for (path, target) in &links {
        let mut resolved: Vec<String> = path.split('/').map(String::from).collect();
        resolved.pop();
        let mut pending: VecDeque<String> = target.split('/').map(String::from).collect();
        let mut followed = 0usize;
        while let Some(part) = pending.pop_front() {
            match part.as_str() {
                "" => return Err(unavailable(Unavailable::InvalidData)),
                "." => continue,
                ".." => {
                    if resolved.pop().is_none() {
                        return Err(unavailable(Unavailable::InvalidData));
                    }
                }
                _ => {
                    resolved.push(part);
                    if let Some(next) = links.get(&resolved.join("/")) {
                        followed += 1;
                        if followed > 40 {
                            return Err(unavailable(Unavailable::InvalidData));
                        }
                        resolved.pop();
                        for part in next.split('/').rev() {
                            pending.push_front(part.to_owned());
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod review_regressions {
    use super::*;
    fn item(id: u64, path: &str, kind: NodeKind) -> Item {
        Item {
            node: FileNode {
                id: NodeId(id),
                parent: None,
                name: path.rsplit('/').next().unwrap().into(),
                kind,
                size: ContentSize::Known(0),
                version: 1,
                modified_ms: None,
                exec_bits: None,
            },
            path: path.into(),
            size: 0,
        }
    }
    #[test]
    fn symlink_chain_cannot_escape_even_when_each_target_is_lexically_safe() {
        let items = vec![
            item(1, "a/up", NodeKind::Symlink("..".into())),
            item(2, "escape", NodeKind::Symlink("a/up/..".into())),
        ];
        assert!(validate_links(&items).is_err());
        let safe = vec![
            item(1, "a/up", NodeKind::Symlink("..".into())),
            item(2, "inside", NodeKind::Symlink("a/up/file".into())),
        ];
        assert!(validate_links(&safe).is_ok());
    }
    #[test]
    fn a_waiting_native_reader_can_cancel_without_acquiring_the_copy_lock() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("file"), b"data").unwrap();
        let namespace = super::super::namespace::Local::new(vec![(NodeId(1), source.path().join("file"))]).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let receiver = Receiver::new_at(namespace, Arc::new(|| Ok(())), &cache.path().join("received")).unwrap();
        let lock = receiver.serial.lock().unwrap();
        let waiting = receiver.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let began = std::time::Instant::now();
            let guard: ReaderGuard = Arc::new(move || {
                if began.elapsed() < Duration::from_millis(30) {
                    Ok(())
                } else {
                    Err(unavailable(Unavailable::NoProgress))
                }
            });
            send.send(waiting.entries_with_guard(guard).is_err()).unwrap();
        });
        assert!(receive.recv_timeout(Duration::from_millis(200)).unwrap());
        drop(lock);
        thread.join().unwrap();
    }
}
