//! MIME/URI codecs shared by CLIPBOARD and XDND. Only local file:// URIs are
//! filesystem capabilities; HTTP URLs and remote authorities are never opened.
use super::*;
use oink_foundation::clipboard::ClipPlatform;
use std::collections::HashSet;
#[derive(Clone)]
pub(super) struct Format {
    pub atom: Atom,
    pub standard: StandardFormat,
    pub latin1: bool,
}
pub(super) fn available(x: &X11, offered: &[Atom]) -> Result<Vec<Format>, ClipboardError> {
    let mut out = Vec::new();
    for (standard, names) in [
        (
            StandardFormat::Text,
            &["UTF8_STRING", "text/plain;charset=utf-8", "text/plain", "STRING"][..],
        ),
        (StandardFormat::Html, &["text/html"][..]),
        (StandardFormat::Rtf, &["text/rtf", "application/rtf"][..]),
        (StandardFormat::Png, &["image/png"][..]),
    ] {
        for name in names {
            let atom = x.atom(name)?;
            if offered.contains(&atom) {
                out.push(Format {
                    atom,
                    standard,
                    latin1: *name == "STRING",
                });
                break;
            }
        }
    }
    Ok(out)
}
pub(super) struct Translator;
impl content::Translator for Translator {
    fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
        content::decode_standard(rep, bytes)
    }
}
pub(super) fn decode_uris(bytes: &[u8], gnome: bool) -> Result<Vec<PathBuf>, ClipboardError> {
    if bytes.len() > crate::model::INVENTORY_BYTES {
        return Err(ClipboardError::ContentUnavailable(Unavailable::LimitExceeded));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ClipboardError::ContentUnavailable(Unavailable::InvalidData))?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') || (gnome && index == 0 && matches!(line, "copy" | "cut")) {
            continue;
        }
        let url = url::Url::parse(line).map_err(|_| ClipboardError::ContentUnavailable(Unavailable::InvalidData))?;
        if url.scheme() != "file" {
            continue;
        }
        let path = url
            .to_file_path()
            .map_err(|_| ClipboardError::ContentUnavailable(Unavailable::InvalidData))?;
        if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(ClipboardError::ContentUnavailable(Unavailable::InvalidData));
        }
        if seen.insert(path.clone()) {
            out.push(path);
        }
        if out.len() > crate::model::MAX_DISCOVERED_NODES {
            return Err(ClipboardError::ContentUnavailable(Unavailable::LimitExceeded));
        }
    }
    Ok(out)
}
pub(super) fn encode_uris(files: &[crate::FileEntry]) -> Result<Vec<u8>, ClipboardError> {
    let mut out = String::new();
    for file in files {
        let path = file
            .local_path
            .as_ref()
            .ok_or(ClipboardError::FileProviderUnavailable)?;
        if !path.is_absolute() {
            return Err(ClipboardError::ContentUnavailable(Unavailable::InvalidData));
        }
        std::fs::symlink_metadata(path).map_err(|e| ClipboardError::ContentUnavailable(content::reason(&e)))?;
        let uri =
            url::Url::from_file_path(path).map_err(|_| ClipboardError::ContentUnavailable(Unavailable::InvalidData))?;
        out.push_str(uri.as_str());
        out.push_str("\r\n");
    }
    if out.len() > crate::model::INVENTORY_BYTES {
        return Err(ClipboardError::ContentUnavailable(Unavailable::LimitExceeded));
    }
    Ok(out.into_bytes())
}
pub(super) fn encode(x: &X11, items: &[ClipData], sensitivity: Sensitivity) -> Result<Formats, ClipboardError> {
    let mut formats = Formats::new();
    for item in items {
        let (names, bytes): (&[&str], Vec<u8>) = match item {
            ClipData::Text(s) => (
                &["UTF8_STRING", "text/plain;charset=utf-8", "text/plain"],
                s.as_bytes().to_vec(),
            ),
            ClipData::Html(s) => (&["text/html"], s.as_bytes().to_vec()),
            ClipData::Rtf(v) => (&["text/rtf", "application/rtf"], v.clone()),
            ClipData::Png(v) => (&["image/png"], v.clone()),
            ClipData::Files(files) => {
                let uris = encode_uris(files)?;
                let mut gnome = b"copy\n".to_vec();
                gnome.extend_from_slice(&uris);
                formats.insert(x.atom("x-special/gnome-copied-files")?, Arc::new(gnome));
                (&["text/uri-list"], uris)
            }
            _ => continue,
        };
        if bytes.len() > x11::LIMIT {
            return Err(ClipboardError::ContentUnavailable(Unavailable::LimitExceeded));
        }
        let bytes = Arc::new(bytes);
        for name in names {
            formats.entry(x.atom(name)?).or_insert_with(|| bytes.clone());
        }
    }
    if formats.is_empty() {
        return Err(ClipboardError::Unsupported);
    }
    if sensitivity == Sensitivity::Concealed {
        formats.insert(x.atom("x-kde-passwordManagerHint")?, Arc::new(b"secret".to_vec()));
    }
    Ok(formats)
}
fn native_bytes(
    contents: &dyn content::Provider,
    rep: &Representation,
    current: &content::ReaderGuard,
) -> Result<Vec<u8>, ClipboardError> {
    let callback = content::native_read::Callback::begin();
    let result = (|| {
        current()?;
        let source_guard = contents.reader_guard(rep.id)?;
        source_guard()?;
        let reader: crate::FileReader = match &rep.data {
            RepresentationData::Inline(bytes) => Box::new(io::Cursor::new(bytes.clone())),
            RepresentationData::Reader { .. } => contents.open(rep.id)?,
            _ => return Err(content::unavailable(Unavailable::Unsupported)),
        };
        let reader = content::guarded(reader, source_guard)?;
        let mut reader = content::guarded(reader, current.clone())?;
        let bytes = content::read_all(&mut *reader)?;
        current()?;
        match rep.standard {
            Some(StandardFormat::Url) => Ok(content::url::Link::decode(&bytes)?.url.into_bytes()),
            Some(StandardFormat::Text | StandardFormat::Html) => {
                std::str::from_utf8(&bytes).map_err(|_| content::unavailable(Unavailable::InvalidData))?;
                Ok(bytes)
            }
            Some(StandardFormat::Rtf | StandardFormat::Png) => Ok(bytes),
            _ => Err(content::unavailable(Unavailable::Unsupported)),
        }
    })();
    if result.is_ok() {
        callback.served();
    }
    result.map_err(|e| ClipboardError::ContentUnavailable(content::reason(&e)))
}
#[cfg(test)]
pub(super) fn native_items(spec: &NativeSpec) -> Result<Vec<ClipData>, ClipboardError> {
    let mut items = Vec::new();
    for rep in spec.inventory.wanted(ClipPlatform::Unknown) {
        if rep.standard.is_none() {
            continue;
        }
        // The snapshot sensitivity is the authoritative summary on Linux; do
        // not turn foreign password-manager control bytes into an opaque MIME.
        if rep.standard == Some(StandardFormat::Privacy) {
            continue;
        }
        // Remote providers already supply the negotiated portable encoding.
        // open_for is the SOURCE-side network translator and must not be run
        // a second time on received bytes (Remote has no native translator).
        let bytes = native_bytes(&*spec.contents, rep, &(Arc::new(|| Ok(())) as content::ReaderGuard))?;
        if rep.standard == Some(StandardFormat::Url) {
            items.push(ClipData::Text(String::from_utf8(bytes).map_err(error)?));
        } else {
            items.extend(content::decode_standard(rep, &bytes).map_err(error)?);
        }
    }
    if !spec.files.is_empty() && spec.files.iter().all(|f| f.local_path.is_some()) {
        items.push(ClipData::Files(spec.files.clone()));
    }
    Ok(items)
}
pub(super) struct Publication {
    pub items: Vec<ClipData>,
    receiver: Option<Arc<content::materialize::Receiver>>,
    native: Option<(NativeSpec, content::ReaderGuard)>,
}
impl Publication {
    pub fn normalized(items: Vec<ClipData>) -> Self {
        Self {
            items,
            receiver: None,
            native: None,
        }
    }
    pub fn native(spec: &NativeSpec, guard: content::ReaderGuard) -> Result<Self, ClipboardError> {
        let receiver = if !spec.inventory.namespace_roots_for(ClipPlatform::Unknown).is_empty()
            && !spec.files.iter().all(|f| f.local_path.is_some())
        {
            Some(content::materialize::Receiver::for_spec(spec, guard.clone()).map_err(error)?)
        } else if !spec.inventory.namespace_roots_for(ClipPlatform::Unknown).is_empty() && spec.files.is_empty() {
            Some(content::materialize::Receiver::for_spec(spec, guard.clone()).map_err(error)?)
        } else {
            None
        };
        let items = if !spec.files.is_empty() && spec.files.iter().all(|f| f.local_path.is_some()) {
            vec![ClipData::Files(spec.files.clone())]
        } else {
            Vec::new()
        };
        Ok(Self {
            items,
            receiver,
            native: Some((spec.clone(), guard)),
        })
    }
    /// Ordinary clipboard publication has an already-authorized Prefetch grant.
    /// Finish that existing bounded policy on the action worker, before exposing
    /// any file URI. XDND deliberately does not call this while still hovering.
    pub fn prefetch_files(&mut self) -> Result<(), ClipboardError> {
        if let Some(receiver) = &self.receiver {
            self.items.push(ClipData::Files(receiver.entries().map_err(error)?));
        }
        Ok(())
    }
    pub fn formats(&self, x: &X11, sensitivity: Sensitivity) -> Result<(Formats, x11::LazyFormats), ClipboardError> {
        let formats = match encode(x, &self.items, sensitivity) {
            Ok(formats) => formats,
            Err(ClipboardError::Unsupported) if self.receiver.is_some() || self.native.is_some() => Formats::new(),
            Err(e) => return Err(e),
        };
        let mut lazy = x11::LazyFormats::new();
        if let Some((spec, current)) = &self.native {
            for rep in spec.inventory.wanted(ClipPlatform::Unknown) {
                let names: &[&str] = match rep.standard {
                    Some(StandardFormat::Text | StandardFormat::Url) => {
                        &["UTF8_STRING", "text/plain;charset=utf-8", "text/plain"]
                    }
                    Some(StandardFormat::Html) => &["text/html"],
                    Some(StandardFormat::Rtf) => &["text/rtf", "application/rtf"],
                    Some(StandardFormat::Png) => &["image/png"],
                    _ => continue,
                };
                for name in names {
                    let contents = spec.contents.clone();
                    let rep = rep.clone();
                    let current = current.clone();
                    lazy.entry(x.atom(name)?)
                        .or_insert_with(|| Arc::new(move || native_bytes(&*contents, &rep, &current)) as x11::Loader);
                }
            }
        }
        if let Some(receiver) = &self.receiver {
            for name in ["text/uri-list", "x-special/gnome-copied-files"] {
                let receiver = receiver.clone();
                lazy.insert(
                    x.atom(name)?,
                    Arc::new(move || {
                        let deadline = std::time::Instant::now() + content::native_read::NATIVE_READ_TIMEOUT;
                        let check: content::ReaderGuard = Arc::new(move || {
                            if std::time::Instant::now() < deadline {
                                Ok(())
                            } else {
                                Err(content::unavailable(Unavailable::NoProgress))
                            }
                        });
                        let entries = receiver.entries_with_guard(check).map_err(error)?;
                        let uris = encode_uris(&entries)?;
                        if name == "text/uri-list" {
                            Ok(uris)
                        } else {
                            let mut bytes = b"copy\n".to_vec();
                            bytes.extend_from_slice(&uris);
                            Ok(bytes)
                        }
                    }) as x11::Loader,
                );
            }
        }
        if formats.is_empty() && lazy.is_empty() {
            return Err(ClipboardError::Unsupported);
        }
        let mut formats = formats;
        if sensitivity == Sensitivity::Concealed {
            formats.insert(x.atom("x-kde-passwordManagerHint")?, Arc::new(b"secret".to_vec()));
        }
        Ok((formats, lazy))
    }
}
pub(super) fn path_entries(paths: Vec<PathBuf>) -> Result<Vec<crate::FileEntry>, ClipboardError> {
    let mut out = Vec::new();
    for path in paths {
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|e| ClipboardError::ContentUnavailable(content::reason(&e)))?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or(ClipboardError::Unsupported)?;
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_millis()).ok());
        out.push(crate::FileEntry {
            path: name.into(),
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified_ms,
            local_path: Some(path),
        });
    }
    Ok(out)
}
pub(super) fn read(x: &mut X11, selection: Atom, time: u32) -> Result<Vec<ClipData>, ClipboardError> {
    let offered = x.offered(selection, time)?;
    read_offered(x, selection, time, &offered)
}
pub(super) fn read_offered(
    x: &mut X11,
    selection: Atom,
    time: u32,
    offered: &[Atom],
) -> Result<Vec<ClipData>, ClipboardError> {
    let mut items = Vec::new();
    for format in available(x, offered)? {
        let bytes = x.load(selection, format.atom, time)?;
        let item = match format.standard {
            StandardFormat::Text if format.latin1 => ClipData::Text(bytes.iter().map(|&b| char::from(b)).collect()),
            StandardFormat::Text => ClipData::Text(String::from_utf8(bytes).map_err(error)?),
            StandardFormat::Html => ClipData::Html(String::from_utf8(bytes).map_err(error)?),
            StandardFormat::Rtf => ClipData::Rtf(bytes),
            StandardFormat::Png => ClipData::Png(bytes),
            _ => continue,
        };
        items.push(item);
    }
    let uri = x.atom("text/uri-list")?;
    let gnome = x.atom("x-special/gnome-copied-files")?;
    let paths = if offered.contains(&uri) {
        decode_uris(&x.load(selection, uri, time)?, false)?
    } else if offered.contains(&gnome) {
        decode_uris(&x.load(selection, gnome, time)?, true)?
    } else {
        Vec::new()
    };
    if !paths.is_empty() {
        items.push(ClipData::Files(path_entries(paths)?));
    }
    Ok(items)
}

struct Owned {
    inventory: Inventory,
    bytes: std::collections::HashMap<RepresentationId, Arc<Vec<u8>>>,
    namespace: Option<Arc<dyn Namespace>>,
}
impl content::Provider for Owned {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
        let bytes = self
            .bytes
            .get(&id)
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
        Ok(Box::new(io::Cursor::new(bytes.as_ref().clone())))
    }
    fn translator(&self) -> Arc<dyn content::Translator> {
        Arc::new(Translator)
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        self.namespace.clone()
    }
}
pub(super) fn owned(items: &[ClipData], generation: u64) -> Result<Arc<dyn content::Provider>, ClipboardError> {
    let mut representations = Vec::new();
    let mut bytes = std::collections::HashMap::new();
    let mut paths = Vec::new();
    let mut total = 0;
    for item in items {
        let (standard, data) = match item {
            ClipData::Text(t) => (StandardFormat::Text, t.as_bytes().to_vec()),
            ClipData::Html(t) => (StandardFormat::Html, t.as_bytes().to_vec()),
            ClipData::Rtf(b) => (StandardFormat::Rtf, b.clone()),
            ClipData::Png(b) => (StandardFormat::Png, b.clone()),
            ClipData::Files(files) => {
                paths.extend(files.iter().filter_map(|f| f.local_path.clone()));
                continue;
            }
            _ => continue,
        };
        total += data.len();
        if data.len() > x11::LIMIT || total as u64 > crate::model::SNAPSHOT_BYTES {
            return Err(ClipboardError::ContentUnavailable(Unavailable::LimitExceeded));
        }
        let id = RepresentationId(representations.len() as u32 + 1);
        representations.push(Representation {
            id,
            format: NativeFormat::Unknown(crate::negotiation::portable_format(standard).into()),
            medium: Medium::Bytes,
            aspect: 0,
            index: -1,
            standard: Some(standard),
            data: RepresentationData::Reader {
                size: ContentSize::Known(data.len() as u64),
            },
        });
        bytes.insert(id, Arc::new(data));
    }
    let namespace: Option<Arc<dyn Namespace>> = if paths.is_empty() {
        None
    } else {
        Some(
            content::namespace::Local::new(
                paths
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| (NodeId(i as u64 + 1), p))
                    .collect(),
            )
            .map_err(|e| ClipboardError::ContentUnavailable(content::reason(&e)))?,
        )
    };
    let roots = namespace.as_ref().map_or_else(Vec::new, |n| n.roots());
    if !roots.is_empty() {
        representations.clear();
        bytes.clear();
    }
    let mut native_items = Vec::new();
    if !representations.is_empty() {
        native_items.push(ContentItem {
            id: ItemId(1),
            virtual_files: false,
            file: roots.first().map(|r| r.id),
            representations,
        });
    }
    for (i, root) in roots.iter().enumerate() {
        if i == 0 && !native_items.is_empty() {
            continue;
        }
        native_items.push(ContentItem {
            id: ItemId(i as u32 + 2),
            virtual_files: false,
            file: Some(root.id),
            representations: Vec::new(),
        });
    }
    let inventory = Inventory {
        id: OfferId {
            clip: ClipId {
                origin: oink_foundation::DeviceId([0; 32]),
                seq: 0,
            },
            generation,
        },
        platform: ClipPlatform::Unknown,
        items: native_items,
        roots,
    };
    inventory
        .validate()
        .map_err(|r| ClipboardError::ContentUnavailable(r))?;
    Ok(Arc::new(Owned {
        inventory,
        bytes,
        namespace,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uri_list_decodes_names_without_accepting_network_authorities() {
        assert_eq!(
            decode_uris(
                b"# comment\r\nfile:///tmp/a%20b\r\nfile:///tmp/a%20b\r\nhttps://example.test/a\n",
                false
            )
            .unwrap(),
            vec![PathBuf::from("/tmp/a b")]
        );
        assert!(decode_uris(b"file://remote-host/tmp/a\n", false).is_err());
        assert!(decode_uris(b"file:///tmp/a%00b\n", false).is_err());
        assert_eq!(
            decode_uris(b"copy\nfile:///tmp/a\n", true).unwrap(),
            vec![PathBuf::from("/tmp/a")]
        );
    }
    #[test]
    fn received_foreign_text_is_already_portable_and_is_not_translated_twice() {
        struct Received;
        struct NoTranslation;
        impl content::Translator for NoTranslation {
            fn standard(&self, _: &Representation, _: &[u8]) -> io::Result<Vec<ClipData>> {
                panic!("received bytes must not be translated twice")
            }
        }
        impl content::Provider for Received {
            fn inventory(&self) -> Inventory {
                Inventory {
                    id: OfferId {
                        clip: ClipId {
                            origin: oink_foundation::DeviceId([0; 32]),
                            seq: 0,
                        },
                        generation: 1,
                    },
                    platform: ClipPlatform::Windows,
                    roots: Vec::new(),
                    items: vec![ContentItem {
                        id: ItemId(1),
                        virtual_files: false,
                        file: None,
                        representations: vec![Representation {
                            id: RepresentationId(1),
                            format: NativeFormat::WindowsPredefined(13),
                            medium: Medium::Bytes,
                            aspect: 0,
                            index: -1,
                            standard: Some(StandardFormat::Text),
                            data: RepresentationData::Reader {
                                size: ContentSize::Known(9),
                            },
                        }],
                    }],
                }
            }
            fn open(&self, _: RepresentationId) -> io::Result<crate::FileReader> {
                Ok(Box::new(io::Cursor::new(b"UTF8 text".to_vec())))
            }
            fn translator(&self) -> Arc<dyn content::Translator> {
                Arc::new(NoTranslation)
            }
        }
        let received: Arc<dyn content::Provider> = Arc::new(Received);
        let spec = NativeSpec {
            inventory: received.inventory(),
            contents: received,
            files: Vec::new(),
            file_contents: None,
            sensitivity: Sensitivity::Normal,
        };
        assert_eq!(native_items(&spec).unwrap(), vec![ClipData::Text("UTF8 text".into())]);
    }
    #[test]
    fn real_file_companion_text_is_not_exported_as_an_independent_path_leak() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let entry = crate::FileEntry {
            path: "file".into(),
            is_dir: false,
            size: 0,
            modified_ms: None,
            local_path: Some(file.path().into()),
        };
        let provider = owned(
            &[
                ClipData::Text(file.path().to_string_lossy().into()),
                ClipData::Files(vec![entry]),
            ],
            1,
        )
        .unwrap();
        let inventory = provider.inventory();
        assert_eq!(inventory.roots.len(), 1);
        assert!(inventory.wanted(ClipPlatform::Windows).next().is_none());
        assert!(crate::policy::without_files(inventory).items.is_empty());
    }
}
