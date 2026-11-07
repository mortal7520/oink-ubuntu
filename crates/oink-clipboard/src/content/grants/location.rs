use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Location {
    path: PathBuf,
    original: PathBuf,
    pub fingerprint: local::Fingerprint,
    #[serde(default)]
    symlink: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub followed: Vec<local::batch::Followed>,
    #[cfg(target_os = "macos")]
    native: mac::Identity,
    #[cfg(windows)]
    volume: PathBuf,
    #[cfg(target_os = "linux")]
    native_linux: Option<linux::Identity>,
}
impl Location {
    pub fn immutable(&self) -> io::Result<PathBuf> {
        if self.inspect(&self.path)? != self.fingerprint {
            return Err(unavailable(Unavailable::Superseded));
        }
        Ok(self.path.clone())
    }
    pub fn capture(path: &Path) -> io::Result<Self> {
        let fingerprint = local::inspect_leaf(path)?;
        let symlink = fs::symlink_metadata(path)?.file_type().is_symlink();
        let path = if symlink {
            path.parent()
                .ok_or_else(|| unavailable(Unavailable::InvalidData))?
                .canonicalize()?
                .join(path.file_name().unwrap())
        } else {
            path.canonicalize()?
        };
        #[cfg(target_os = "macos")]
        let native = mac::Identity::capture(if symlink { path.parent().unwrap() } else { &path })?;
        #[cfg(windows)]
        let volume = windows::volume(&path)?;
        #[cfg(windows)]
        {
            // Establish that this volume accepts this exact file-ID mechanism
            // before issuing a durable token. Unsupported volumes fail here.
            let resolved = windows::resolve(&volume, fingerprint.identity[1])?;
            if local::inspect(&resolved)?.identity != fingerprint.identity {
                return Err(unavailable(Unavailable::Superseded));
            }
        }
        #[cfg(target_os = "linux")]
        let native_linux = if symlink {
            None
        } else {
            Some(linux::Identity::capture(&path, &fingerprint)?)
        };
        let result = Self {
            original: path.clone(),
            path,
            fingerprint,
            symlink,
            followed: Vec::new(),
            #[cfg(target_os = "macos")]
            native,
            #[cfg(windows)]
            volume,
            #[cfg(target_os = "linux")]
            native_linux,
        };
        if result.inspect(&result.path)? != result.fingerprint {
            return Err(unavailable(Unavailable::Superseded));
        }
        Ok(result)
    }
    pub fn capture_followed(path: &Path, followed: Vec<local::batch::Followed>) -> io::Result<Self> {
        for guard in &followed {
            guard.verify()?;
        }
        let mut location = Self::capture(path)?;
        location.followed = followed;
        Ok(location)
    }
    fn matches(&self, _path: &Path, current: &local::Fingerprint) -> io::Result<bool> {
        if current.identity[1] != self.fingerprint.identity[1]
            || current.created != self.fingerprint.created
            || current.directory != self.fingerprint.directory
        {
            return Ok(false);
        }
        #[cfg(target_os = "macos")]
        return Ok(mac::volume_uuid(_path)? == self.native.volume_uuid);
        #[cfg(windows)]
        return Ok(current.identity[0] == self.fingerprint.identity[0] && windows::volume(_path)? == self.volume);
        #[cfg(not(any(target_os = "macos", windows)))]
        Ok(current.identity == self.fingerprint.identity)
    }
    fn inspect(&self, path: &Path) -> io::Result<local::Fingerprint> {
        if self.symlink {
            local::inspect_leaf(path)
        } else {
            local::inspect(path)
        }
    }
    pub fn resolve(&self) -> io::Result<PathBuf> {
        if !self.followed.is_empty() {
            for guard in &self.followed {
                guard.verify()?;
            }
            let current = self
                .inspect(&self.path)
                .map_err(|_| unavailable(Unavailable::Superseded))?;
            if current.identity != self.fingerprint.identity || current.created != self.fingerprint.created {
                return Err(unavailable(Unavailable::Superseded));
            }
            return Ok(self.path.clone());
        }
        // Links have no bookmark/target authority. Their parent is resolved by
        // the directory grant; never recover a link by resolving its target.
        if self.symlink {
            return (self.inspect(&self.path)? == self.fingerprint)
                .then(|| self.path.clone())
                .ok_or_else(|| unavailable(Unavailable::Superseded));
        }
        // The path is only a fast hint. Prefer the original object whenever it
        // still exists; atomic-save replacements are allowed only after Gone.
        #[cfg(not(target_os = "linux"))]
        if let Ok(current) = local::inspect(&self.path)
            && self.matches(&self.path, &current)?
        {
            return Ok(self.path.clone());
        }
        match self.resolve_identity() {
            Ok(path) => Ok(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let current = local::inspect(&self.original)?;
                if current.directory != self.fingerprint.directory {
                    return Err(unavailable(Unavailable::Unsupported));
                }
                #[cfg(target_os = "macos")]
                if mac::volume_uuid(&self.original)? != self.native.volume_uuid {
                    return Err(unavailable(Unavailable::Permission));
                }
                #[cfg(windows)]
                if windows::volume(&self.original)? != self.volume {
                    return Err(unavailable(Unavailable::Permission));
                }
                #[cfg(target_os = "linux")]
                self.validate_resolved(&self.original, &current)?;
                Ok(self.original.clone())
            }
            Err(error) => Err(error),
        }
    }
    /// Bind the final metadata to the authority that selected this object.
    /// A path returned by a native resolver can be renamed/replaced before a
    /// caller stats it. That race must not silently refresh a grant to a new ID.
    pub(super) fn validate_resolved(&self, path: &Path, current: &local::Fingerprint) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            if !self.followed.is_empty() {
                for guard in &self.followed {
                    guard.verify()?;
                }
                if !self.matches(path, current)? {
                    return Err(unavailable(Unavailable::Superseded));
                }
                return Ok(());
            }
            if self.symlink {
                return self
                    .matches(path, current)?
                    .then_some(())
                    .ok_or_else(|| unavailable(Unavailable::Superseded));
            }
            let native = self
                .native_linux
                .as_ref()
                .ok_or_else(|| unavailable(Unavailable::Unsupported))?;
            if self.matches(path, current)? {
                return linux::remember(path, current);
            }
            if path != self.original
                || current.directory != self.fingerprint.directory
                || current.identity[0] != self.fingerprint.identity[0]
                || fs::symlink_metadata(path)?.file_type().is_symlink()
            {
                return Err(unavailable(Unavailable::Permission));
            }
            native.authorize_replacement(self)?;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (path, current);
        Ok(())
    }
    fn resolve_identity(&self) -> io::Result<PathBuf> {
        #[cfg(target_os = "macos")]
        let path = self.native.resolve(self)?;
        #[cfg(windows)]
        let path = windows::resolve(&self.volume, self.fingerprint.identity[1])?;
        #[cfg(target_os = "linux")]
        let path = self
            .native_linux
            .as_ref()
            .ok_or_else(|| unavailable(Unavailable::Unsupported))?
            .resolve(self)?;
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        let path = self.path.clone();
        let current = local::inspect(&path)?;
        if !self.matches(&path, &current)? {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        path.canonicalize()
    }
    pub fn refresh(&self, path: &Path) -> io::Result<Self> {
        let mut location = Self::capture(path)?;
        location.original = self.original.clone();
        location.followed = self.followed.clone();
        #[cfg(target_os = "linux")]
        if let (Some(previous), Some(current)) = (&self.native_linux, &mut location.native_linux) {
            current.original_parent = previous.original_parent.clone();
        }
        Ok(location)
    }
    pub fn current(&self, path: &Path, fingerprint: &local::Fingerprint) -> bool {
        self.path == path && self.fingerprint == *fingerprint
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(super) mod test_pause {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Arc, Mutex, OnceLock, mpsc},
        time::Duration,
    };
    struct Pause {
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    static HOOKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Pause>>>> = OnceLock::new();
    fn hooks() -> &'static Mutex<HashMap<PathBuf, Arc<Pause>>> {
        HOOKS.get_or_init(Default::default)
    }
    pub(crate) struct Guard {
        path: PathBuf,
        pause: Arc<Pause>,
        resume: mpsc::Sender<()>,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.resume.send(());
            let mut hooks = hooks().lock().unwrap();
            if hooks
                .get(&self.path)
                .is_some_and(|current| Arc::ptr_eq(current, &self.pause))
            {
                hooks.remove(&self.path);
            }
        }
    }
    pub(crate) fn install(path: PathBuf) -> (mpsc::Receiver<()>, mpsc::Sender<()>, Guard) {
        let (entered_tx, entered) = mpsc::channel();
        let (resume, resume_rx) = mpsc::channel();
        let pause = Arc::new(Pause {
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
        });
        assert!(hooks().lock().unwrap().insert(path.clone(), pause.clone()).is_none());
        (entered, resume.clone(), Guard { path, pause, resume })
    }
    pub(crate) fn run(path: &Path) {
        let pause = hooks().lock().unwrap().remove(path);
        if let Some(pause) = pause {
            let _ = pause.entered.send(());
            assert!(
                pause
                    .resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .is_ok(),
                "identity test pause timed out"
            );
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use objc2_foundation::{
        NSData, NSString, NSURL, NSURLBookmarkCreationOptions as Creation,
        NSURLBookmarkResolutionOptions as Resolution, NSURLVolumeUUIDStringKey,
    };
    use std::{
        ffi::{CStr, CString, OsStr},
        os::unix::ffi::OsStrExt,
    };
    #[derive(Clone, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Identity {
        bookmark: Vec<u8>,
        volume_bookmark: Vec<u8>,
        pub volume_uuid: String,
    }
    fn url(path: &Path) -> io::Result<objc2::rc::Retained<NSURL>> {
        let text = path.to_str().ok_or_else(|| unavailable(Unavailable::InvalidData))?;
        Ok(NSURL::fileURLWithPath(&NSString::from_str(text)))
    }
    fn bookmark(path: &Path) -> io::Result<Vec<u8>> {
        // No implicit security scope: persisted links cannot grant additional
        // filesystem/TCC access or cause later resolution prompts.
        url(path)?
            .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                Creation::WithoutImplicitSecurityScope,
                None,
                None,
            )
            .map(|data| data.to_vec())
            .map_err(|_| unavailable(Unavailable::Permission))
    }
    pub(super) fn volume_uuid(path: &Path) -> io::Result<String> {
        let mut value = None;
        // SAFETY: this resource key returns an NSString, checked by downcast.
        unsafe { url(path)?.getResourceValue_forKey_error(&mut value, NSURLVolumeUUIDStringKey) }
            .map_err(|_| unavailable(Unavailable::ReadFailed))?;
        value
            .and_then(|value| value.downcast::<NSString>().ok())
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| unavailable(Unavailable::Unsupported))
    }
    fn filesystem(path: &Path) -> io::Result<libc::statfs> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| unavailable(Unavailable::InvalidData))?;
        let mut result = std::mem::MaybeUninit::uninit();
        // SAFETY: NUL-terminated path and writable statfs output live for call.
        if unsafe { libc::statfs(path.as_ptr(), result.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful statfs initialized the entire output record.
        Ok(unsafe { result.assume_init() })
    }
    fn resolve_bookmark(bytes: &[u8]) -> io::Result<PathBuf> {
        // SAFETY: optional stale pointer is null; no UI, mounts or implicit access.
        let url = unsafe {
            NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                &NSData::with_bytes(bytes),
                Resolution::WithoutUI | Resolution::WithoutMounting | Resolution::WithoutImplicitStartAccessing,
                None,
                std::ptr::null_mut(),
            )
        }
        .map_err(|_| unavailable(Unavailable::ReadFailed))?;
        // SAFETY: NSURL owns this NUL-terminated representation until the copy.
        let bytes = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) }.to_bytes();
        Ok(PathBuf::from(OsStr::from_bytes(bytes)))
    }

    impl Identity {
        pub(super) fn capture(path: &Path) -> io::Result<Self> {
            let filesystem = filesystem(path)?;
            // SAFETY: successful statfs returns a NUL-terminated mount name.
            let mount = unsafe { CStr::from_ptr(filesystem.f_mntonname.as_ptr()) }.to_bytes();
            let mount = Path::new(OsStr::from_bytes(mount));
            Ok(Self {
                bookmark: bookmark(path)?,
                volume_bookmark: bookmark(mount)?,
                volume_uuid: volume_uuid(path)?,
            })
        }
        pub(super) fn resolve(&self, location: &Location) -> io::Result<PathBuf> {
            if let Ok(path) = resolve_bookmark(&self.bookmark)
                && let Ok(current) = local::inspect(&path)
                && location.matches(&path, &current)?
            {
                return Ok(path);
            }
            // macOS 26.6.2: bookmarks can prefer a replacement at the old path;
            // fileReferenceURL-created bookmarks behave the same. Resolve the
            // saved volume bookmark, verify its stable UUID, then ask the kernel
            // for the original inode. No directory/disk scan or private option.
            // Neither a path fallback nor this lookup bypasses identity/scope.
            let mount = resolve_bookmark(&self.volume_bookmark)?;
            if volume_uuid(&mount)? != self.volume_uuid {
                return Err(unavailable(Unavailable::ReadFailed));
            }
            let mut filesystem = filesystem(&mount)?;
            unsafe extern "C" {
                fn fsgetpath(
                    buffer: *mut libc::c_char,
                    length: libc::size_t,
                    fsid: *mut libc::fsid_t,
                    object: u64,
                ) -> libc::ssize_t;
            }
            let mut buffer = vec![0; libc::PATH_MAX as usize];
            // SAFETY: valid bounded output and the mounted volume's current fsid.
            let length = unsafe {
                fsgetpath(
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut filesystem.f_fsid,
                    location.fingerprint.identity[1],
                )
            };
            if length < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fsgetpath success returns a NUL-terminated filesystem path.
            let bytes = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_bytes();
            Ok(PathBuf::from(OsStr::from_bytes(bytes)))
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use ::windows::{
        Win32::{Foundation::HANDLE, Storage::FileSystem::*},
        core::PCWSTR,
    };
    use std::os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    };
    pub(super) fn volume(path: &Path) -> io::Result<PathBuf> {
        let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut mount = vec![0; 32768];
        let mut volume = [0; 128];
        // SAFETY: NUL-terminated input and writable output buffers survive calls.
        unsafe {
            GetVolumePathNameW(PCWSTR(path.as_ptr()), &mut mount).map_err(io::Error::other)?;
            GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut volume).map_err(io::Error::other)?;
        }
        let end = volume
            .iter()
            .position(|c| *c == 0)
            .ok_or_else(|| unavailable(Unavailable::InvalidData))?;
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&volume[..end])))
    }
    pub(super) fn resolve(volume: &Path, id: u64) -> io::Result<PathBuf> {
        // A directory handle on the stored volume is sufficient as the hint;
        // no raw-volume handle, privilege changes, or disk scan is necessary.
        let hint = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(volume)
            .map_err(|_| unavailable(Unavailable::ReadFailed))?;
        let file = match open_id(&hint, id) {
            Ok(file) => file,
            Err(error) => {
                // Windows 11/NTFS returns ERROR_INVALID_PARAMETER for a deleted
                // file ID. Capture already proved this ID/volume mechanism; a
                // second lookup of the live volume-root ID distinguishes Gone
                // from an unsupported API/volume or invalid handle. Do not turn
                // generic errors into permission to read a path replacement.
                let reason = match error.code().0 as u32 {
                    0x80070002 | 0x80070003 => Unavailable::NoSuchItem,
                    0x80070057 if open_id(&hint, local::file_fingerprint(&hint)?.identity[1]).is_ok() => {
                        Unavailable::NoSuchItem
                    }
                    0x80070005 => Unavailable::Permission,
                    _ => Unavailable::ReadFailed,
                };
                return Err(unavailable(reason));
            }
        };
        let mut path = vec![0; 32768];
        // SAFETY: live owned handle and writable bounded UTF-16 buffer.
        let count =
            unsafe { GetFinalPathNameByHandleW(HANDLE(file.as_raw_handle()), &mut path, VOLUME_NAME_DOS) } as usize;
        if count == 0 {
            return Err(io::Error::last_os_error());
        }
        if count >= path.len() {
            return Err(unavailable(Unavailable::LimitExceeded));
        }
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&path[..count])))
    }
    fn open_id(hint: &File, id: u64) -> ::windows::core::Result<File> {
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: id as i64 },
        };
        // SAFETY: borrowed live hint and initialized descriptor remain valid.
        let handle = unsafe {
            OpenFileById(
                HANDLE(hint.as_raw_handle()),
                &descriptor,
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        }?;
        // SAFETY: successful OpenFileById transfers a unique owned handle.
        Ok(unsafe { File::from_raw_handle(handle.0) })
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
        sync::OnceLock,
        time::{Duration, Instant, SystemTime},
    };
    type Key = (u64, u64, Option<SystemTime>);
    #[derive(Clone, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Identity {
        parent: PathBuf,
        ancestor: Option<PathBuf>,
        #[serde(default)]
        pub(super) original_parent: Option<local::Fingerprint>,
    }
    #[derive(Default)]
    struct Pins {
        files: HashMap<Key, Arc<File>>,
        order: VecDeque<Key>,
        gone: HashSet<Key>,
        gone_order: VecDeque<Key>,
    }
    static PINS: OnceLock<Mutex<Pins>> = OnceLock::new();
    fn key(fp: &local::Fingerprint) -> Key {
        (fp.identity[0], fp.identity[1], fp.created)
    }
    fn same(fp: &local::Fingerprint, other: &local::Fingerprint) -> bool {
        fp.identity == other.identity && fp.created == other.created && fp.directory == other.directory
    }
    fn pins() -> &'static Mutex<Pins> {
        PINS.get_or_init(|| Mutex::new(Pins::default()))
    }
    static SWEEPER: OnceLock<bool> = OnceLock::new();
    fn start_sweeper() -> io::Result<()> {
        if *SWEEPER.get_or_init(|| {
            std::thread::Builder::new()
                .name("oink-source-identities".into())
                .spawn(|| {
                    loop {
                        std::thread::sleep(Duration::from_secs(1));
                        reap_deleted();
                    }
                })
                .is_ok()
        }) {
            Ok(())
        } else {
            Err(unavailable(Unavailable::ReadFailed))
        }
    }
    fn reap_deleted() {
        let snapshot: Vec<_> = pins()
            .lock()
            .unwrap()
            .files
            .iter()
            .map(|(key, file)| (*key, file.clone()))
            .collect();
        let deleted: Vec<_> = snapshot
            .into_iter()
            .filter(|(_key, file)| {
                #[cfg(test)]
                super::test_pause::run(&reap_test_key(_key));
                file.metadata().is_ok_and(|metadata| metadata.nlink() == 0)
            })
            .collect();
        let mut retired = Vec::new();
        {
            let mut pins = pins().lock().unwrap();
            for (key, observed) in &deleted {
                if !pins
                    .files
                    .get(key)
                    .is_some_and(|current| Arc::ptr_eq(current, observed))
                {
                    continue;
                }
                if let Some(file) = pins.files.remove(key) {
                    retired.push(file);
                }
                pins.order.retain(|old| old != key);
                if pins.gone.insert(*key) {
                    pins.gone_order.push_back(*key);
                }
            }
            while pins.gone.len() > 1024 {
                if let Some(key) = pins.gone_order.pop_front() {
                    pins.gone.remove(&key);
                } else {
                    break;
                }
            }
        }
        // Closing the last O_PATH reference may release a large unlinked file.
        // Neither that close nor fstat above may hold the global identity gate.
        drop(retired);
        drop(deleted);
    }
    #[cfg(test)]
    fn reap_test_key(key: &Key) -> PathBuf {
        PathBuf::from(format!("/__oink_identity_reaper_test/{key:?}"))
    }

    pub(super) fn remember(path: &Path, fp: &local::Fingerprint) -> io::Result<()> {
        start_sweeper()?;
        let id = key(fp);
        {
            let pins = pins().lock().unwrap();
            if pins.gone.contains(&id) {
                return Err(unavailable(Unavailable::Superseded));
            }
            if pins.files.contains_key(&id) {
                return Ok(());
            }
        }
        // O_PATH pins identity without granting a content read or changing
        // permission. NOFOLLOW prevents a raced symlink from becoming authority.
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        if !same(fp, &local::file_fingerprint(&file)?) {
            return Err(unavailable(Unavailable::Superseded));
        }
        let mut retired = Vec::new();
        let mut pins = pins().lock().unwrap();
        if pins.gone.contains(&id) {
            return Err(unavailable(Unavailable::Superseded));
        }
        if !pins.files.contains_key(&id) {
            while pins.files.len() >= 256 {
                if let Some(old) = pins.order.pop_front() {
                    if let Some(file) = pins.files.remove(&old) {
                        retired.push(file);
                    }
                } else {
                    break;
                }
            }
            pins.order.push_back(id);
            pins.files.insert(id, Arc::new(file));
        }
        drop(pins);
        drop(retired);
        Ok(())
    }
    impl Identity {
        pub(super) fn capture(path: &Path, fp: &local::Fingerprint) -> io::Result<Self> {
            // A persisted inode without a birth identity cannot distinguish
            // inode reuse after a process restart. Refuse that filesystem here.
            if fp.created.is_none() {
                return Err(unavailable(Unavailable::Unsupported));
            }
            remember(path, fp)?;
            let parent = path
                .parent()
                .ok_or_else(|| unavailable(Unavailable::InvalidData))?
                .to_path_buf();
            let ancestor = parent.parent().filter(|p| p.parent().is_some()).map(Path::to_path_buf);
            let original_parent = Some(local::inspect(&parent)?);
            Ok(Self {
                parent,
                ancestor,
                original_parent,
            })
        }
        fn proves_gone(&self, fp: &local::Fingerprint) -> io::Result<bool> {
            let file = {
                let pins = pins().lock().unwrap();
                if pins.gone.contains(&key(fp)) {
                    return Ok(true);
                }
                pins.files.get(&key(fp)).cloned()
            };
            let Some(file) = file else {
                return Ok(false);
            };
            if !same(fp, &local::file_fingerprint(&file)?) {
                return Err(unavailable(Unavailable::Superseded));
            }
            Ok(file.metadata()?.nlink() == 0)
        }
        pub(super) fn authorize_replacement(&self, location: &Location) -> io::Result<()> {
            if !self.proves_gone(&location.fingerprint)? {
                return Err(unavailable(Unavailable::Superseded));
            }
            let parent = location
                .original
                .parent()
                .ok_or_else(|| unavailable(Unavailable::Permission))?;
            let expected = self
                .original_parent
                .as_ref()
                .ok_or_else(|| unavailable(Unavailable::Unsupported))?;
            if expected.created.is_none() {
                return Err(unavailable(Unavailable::Unsupported));
            }
            let current = local::inspect(parent)?;
            if !same(expected, &current) {
                return Err(unavailable(Unavailable::Permission));
            }
            Ok(())
        }
        pub(super) fn resolve(&self, location: &Location) -> io::Result<PathBuf> {
            let fp = &location.fingerprint;
            let file = {
                let pins = pins().lock().unwrap();
                if pins.gone.contains(&key(fp)) {
                    return Err(io::ErrorKind::NotFound.into());
                }
                pins.files.get(&key(fp)).cloned()
            };
            if let Some(file) = file {
                let current = local::file_fingerprint(&file)?;
                if !same(fp, &current) {
                    return Err(unavailable(Unavailable::Superseded));
                }
                if file.metadata()?.nlink() == 0 {
                    // This is positive kernel evidence that the original is gone,
                    // unlike a failed path search. Only this permits atomic-save
                    // replacement at the originally authorized path.
                    return Err(io::ErrorKind::NotFound.into());
                }
                let path = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
                if path.is_absolute() && local::inspect(&path).is_ok_and(|other| same(fp, &other)) {
                    return Ok(path);
                }
                return Err(unavailable(Unavailable::ReadFailed));
            }
            // Linux unprivileged processes cannot open arbitrary inode handles.
            // Recover a renamed object only in its saved parent neighbourhood,
            // with bounded metadata-only traversal, no symlinks/mount crossing.
            // An incomplete/negative search is NEVER evidence that it was deleted.
            let started = Instant::now();
            let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::from([(self.parent.clone(), 0)]);
            if let Some(ancestor) = &self.ancestor {
                queue.push_back((ancestor.clone(), 0));
            }
            let mut directories = HashSet::new();
            let mut count = 0usize;
            while let Some((directory, depth)) = queue.pop_front() {
                if started.elapsed() > Duration::from_secs(2) {
                    break;
                }
                let Ok(metadata) = fs::symlink_metadata(&directory) else {
                    continue;
                };
                if !metadata.is_dir()
                    || metadata.file_type().is_symlink()
                    || metadata.dev() != fp.identity[0]
                    || !directories.insert((metadata.dev(), metadata.ino()))
                {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&directory) else {
                    continue;
                };
                for entry in entries {
                    if started.elapsed() > Duration::from_secs(2) || count >= 65_536 {
                        return Err(unavailable(Unavailable::ReadFailed));
                    }
                    count += 1;
                    let Ok(entry) = entry else {
                        continue;
                    };
                    let path = entry.path();
                    let Ok(metadata) = fs::symlink_metadata(&path) else {
                        continue;
                    };
                    if metadata.file_type().is_symlink() || metadata.dev() != fp.identity[0] {
                        continue;
                    }
                    if metadata.ino() == fp.identity[1] {
                        if let Ok(current) = local::inspect(&path) {
                            if same(fp, &current) {
                                remember(&path, fp)?;
                                return Ok(path);
                            }
                        }
                    }
                    if depth < 2 && metadata.is_dir() {
                        queue.push_back((path, depth + 1));
                    }
                }
            }
            Err(unavailable(Unavailable::ReadFailed))
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn atomic_replacement_cannot_turn_an_original_file_grant_into_a_symlink_grant() {
            let directory = tempfile::tempdir().unwrap();
            let original = directory.path().join("original");
            let other = directory.path().join("not-selected");
            fs::write(&original, b"selected").unwrap();
            fs::write(&other, b"not-authorized").unwrap();
            let location = Location::capture(&original).unwrap();
            fs::remove_file(&original).unwrap();
            std::os::unix::fs::symlink(&other, &original).unwrap();
            assert!(location.resolve().is_err());
        }
        #[test]
        fn gone_object_cannot_rebind_inside_a_replaced_parent_directory() {
            let directory = tempfile::tempdir().unwrap();
            let parent = directory.path().join("parent");
            fs::create_dir(&parent).unwrap();
            let original = parent.join("original");
            fs::write(&original, b"selected").unwrap();
            let location = Location::capture(&original).unwrap();
            fs::remove_file(&original).unwrap();
            fs::rename(&parent, directory.path().join("old-parent")).unwrap();
            fs::create_dir(&parent).unwrap();
            fs::write(&original, b"different-parent-authority").unwrap();
            assert!(location.resolve().is_err());
        }
        #[test]
        fn releasing_unlinked_pins_preserves_proof_without_retaining_deleted_file_bytes() {
            let directory = tempfile::tempdir().unwrap();
            let original = directory.path().join("original");
            fs::write(&original, b"selected").unwrap();
            let location = Location::capture(&original).unwrap();
            let id = key(&location.fingerprint);
            fs::remove_file(&original).unwrap();
            fs::write(&original, b"atomic-save").unwrap();
            reap_deleted();
            assert!(!pins().lock().unwrap().files.contains_key(&id));
            assert!(pins().lock().unwrap().gone.contains(&id));
            assert_eq!(fs::read(location.resolve().unwrap()).unwrap(), b"atomic-save");
        }

        #[test]
        fn reaper_checks_metadata_without_holding_the_global_pin_gate() {
            let directory = tempfile::tempdir().unwrap();
            let original = directory.path().join("original");
            fs::write(&original, b"selected").unwrap();
            let location = Location::capture(&original).unwrap();
            let id = key(&location.fingerprint);
            let (entered, resume, _guard) = super::super::test_pause::install(reap_test_key(&id));
            let worker = std::thread::spawn(reap_deleted);
            entered.recv_timeout(Duration::from_secs(2)).unwrap();
            let deadline = Instant::now() + Duration::from_millis(100);
            loop {
                if let Ok(gate) = pins().try_lock() {
                    drop(gate);
                    break;
                }
                assert!(Instant::now() < deadline, "fstat worker retained the global pin gate");
                std::thread::yield_now();
            }
            resume.send(()).unwrap();
            worker.join().unwrap();
        }

        #[test]
        fn persisted_identity_recovers_in_a_fresh_process_without_reading_replacement() {
            const CHILD: &str = "OINK_LINUX_IDENTITY_CHILD";
            if let Some(directory) = std::env::var_os(CHILD) {
                let directory = PathBuf::from(directory);
                let location: Location =
                    serde_json::from_slice(&fs::read(directory.join("identity.json")).unwrap()).unwrap();
                let path = location.resolve().unwrap();
                assert_eq!(fs::read(path).unwrap(), b"original-object");
                return;
            }
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("original");
            fs::write(&path, b"original-object").unwrap();
            let location = Location::capture(&path).unwrap();
            fs::write(
                directory.path().join("identity.json"),
                serde_json::to_vec(&location).unwrap(),
            )
            .unwrap();
            fs::rename(&path, directory.path().join("renamed")).unwrap();
            fs::write(&path, b"never-the-authorized-object").unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["content::grants::location::linux::tests::persisted_identity_recovers_in_a_fresh_process_without_reading_replacement", "--exact"])
                .env(CHILD, directory.path()).status().unwrap();
            assert!(status.success());
        }
    }
}
