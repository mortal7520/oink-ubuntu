use super::*;
use crate::model::{ContentItem, ItemId, Medium, NativeFormat, StandardFormat};
use crate::sync::files::BoxFuture;
use oink_foundation::{DeviceId, content::ClipId};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
};

struct LegacySource {
    inventory: Inventory,
    bodies: HashMap<RepresentationId, Vec<u8>>,
}
struct StandardTranslator(ClipPlatform);
impl Translator for StandardTranslator {
    fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
        Ok(vec![match rep.standard {
            Some(StandardFormat::Text) if self.0 == ClipPlatform::Windows => {
                let words = bytes
                    .chunks_exact(2)
                    .map(|p| u16::from_le_bytes([p[0], p[1]]))
                    .take_while(|n| *n != 0)
                    .collect::<Vec<_>>();
                ClipData::Text(String::from_utf16(&words).unwrap())
            }
            Some(StandardFormat::Text) => ClipData::Text(String::from_utf8(bytes.to_vec()).unwrap()),
            Some(StandardFormat::Html) => ClipData::Html(String::from_utf8(bytes.to_vec()).unwrap()),
            Some(StandardFormat::Rtf) => ClipData::Rtf(bytes.to_vec()),
            Some(StandardFormat::Png) => ClipData::Png(bytes.to_vec()),
            _ => return Err(unavailable(Unavailable::Unsupported)),
        }])
    }
}
impl Provider for LegacySource {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        Ok(Box::new(io::Cursor::new(
            self.bodies
                .get(&id)
                .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?
                .clone(),
        )))
    }
    fn translator(&self) -> Arc<dyn Translator> {
        Arc::new(StandardTranslator(self.inventory.platform))
    }
}
fn profile(formats: &[&str]) -> oink_protocol::v1::dto::FormatSupport {
    use oink_protocol::v1::{RawValue, schema::WireDto};
    let strings = || RawValue::Array(formats.iter().map(|s| RawValue::Text((*s).into())).collect());
    oink_protocol::v1::dto::FormatSupport::from_raw(
        [
            (1, strings()),
            (2, strings()),
            (3, RawValue::Array(vec![RawValue::Text("mime".into())])),
            (4, RawValue::Array(vec![RawValue::Uint(0)])),
        ]
        .into(),
    )
    .unwrap()
}
struct LegacyLink {
    source: Arc<LegacySource>,
    source_profile: oink_protocol::v1::dto::FormatSupport,
    target_profile: oink_protocol::v1::dto::FormatSupport,
    targets: Arc<Mutex<Vec<ClipPlatform>>>,
    opens: Arc<AtomicUsize>,
}
struct LegacyChannel {
    source: Arc<LegacySource>,
    targets: Arc<Mutex<Vec<ClipPlatform>>>,
    replies: VecDeque<ContentMsg>,
}
impl BulkLink for LegacyLink {
    fn bind(self: Arc<Self>, _: DeviceId, _: oink_foundation::content::ClipId) -> Arc<dyn BulkLink> {
        self
    }
    fn select_representation(
        &self,
        inventory: &Inventory,
        id: RepresentationId,
        target: ClipPlatform,
    ) -> io::Result<(ItemId, String)> {
        crate::negotiation::negotiated_selection(&self.source_profile, &self.target_profile, inventory, id, target)
            .map_err(|_| unavailable(Unavailable::Unsupported))
    }
    fn open(&self, _: DeviceId) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            Ok(Box::new(LegacyChannel {
                source: self.source.clone(),
                targets: self.targets.clone(),
                replies: VecDeque::new(),
            }) as Box<dyn BulkChannel>)
        })
    }
}
impl BulkChannel for LegacyChannel {
    fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let ContentMsg::ReadRepresentation {
                offer,
                representation,
                item,
                accepted_format,
                target,
                ..
            } = message
            else {
                panic!("representations only")
            };
            self.targets.lock().unwrap().push(*target);
            // Frozen legacy policy: the released Windows/macOS builds do not
            // expose any representation to an Unknown rendering target.
            let result = if *target == ClipPlatform::Unknown {
                Err(unavailable(Unavailable::Unsupported))
            } else if crate::negotiation::selection(&self.source.inventory, *representation, *target)
                != Some((*item, accepted_format.clone()))
            {
                Err(unavailable(Unavailable::Unsupported))
            } else {
                read_all(&mut open_for(self.source.as_ref(), *representation, *target)?)
            };
            match result {
                Ok(bytes) => {
                    let size = bytes.len() as u64;
                    if !bytes.is_empty() {
                        self.replies.push_back(ContentMsg::RepresentationBytes {
                            offer: *offer,
                            representation: *representation,
                            offset: 0,
                            bytes,
                        });
                    }
                    self.replies.push_back(ContentMsg::RepresentationEnd {
                        offer: *offer,
                        representation: *representation,
                        size,
                    });
                }
                Err(error) => self.replies.push_back(ContentMsg::RepresentationFailed {
                    offer: *offer,
                    representation: *representation,
                    reason: reason(&error),
                }),
            }
            Ok(())
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        Box::pin(async { Ok(self.replies.pop_front()) })
    }
    fn finish(&mut self) {}
}
fn source(platform: ClipPlatform) -> (Arc<LegacySource>, Vec<Vec<u8>>) {
    let expected = vec![
        "native clipboard 中文🙂".as_bytes().to_vec(),
        b"<p>HTML &amp; Unicode</p>".to_vec(),
        br"{\rtf1\ansi Native clipboard}".to_vec(),
        vec![
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4, 0, 0, 0, 181,
            28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 252, 255, 31, 0, 2, 235, 1, 245, 105, 122, 97, 88, 0,
            0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ],
    ];
    let standards = [
        StandardFormat::Text,
        StandardFormat::Html,
        StandardFormat::Rtf,
        StandardFormat::Png,
    ];
    let formats = if platform == ClipPlatform::Windows {
        vec![
            NativeFormat::WindowsPredefined(13),
            NativeFormat::WindowsRegistered("HTML Format".into()),
            NativeFormat::WindowsRegistered("Rich Text Format".into()),
            NativeFormat::WindowsRegistered("PNG".into()),
        ]
    } else {
        vec![
            NativeFormat::Mac("public.utf8-plain-text".into()),
            NativeFormat::Mac("public.html".into()),
            NativeFormat::Mac("public.rtf".into()),
            NativeFormat::Mac("public.png".into()),
        ]
    };
    let mut representations = Vec::new();
    let mut bodies = HashMap::new();
    for (i, ((format, standard), bytes)) in formats.into_iter().zip(standards).zip(expected.iter()).enumerate() {
        let id = RepresentationId(i as u32 + 1);
        let raw = if i == 0 && platform == ClipPlatform::Windows {
            String::from_utf8(bytes.clone())
                .unwrap()
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect()
        } else {
            bytes.clone()
        };
        representations.push(Representation {
            id,
            format,
            medium: if platform == ClipPlatform::Windows {
                Medium::HGlobal
            } else {
                Medium::Bytes
            },
            aspect: 1,
            index: -1,
            standard: Some(standard),
            data: RepresentationData::Reader {
                size: ContentSize::Known(raw.len() as u64),
            },
        });
        bodies.insert(id, raw);
    }
    (
        Arc::new(LegacySource {
            inventory: Inventory {
                id: OfferId {
                    clip: ClipId {
                        origin: DeviceId([1; 32]),
                        seq: 1,
                    },
                    generation: 1,
                },
                platform,
                roots: vec![],
                items: vec![ContentItem {
                    id: ItemId(1),
                    virtual_files: false,
                    file: None,
                    representations,
                }],
            },
            bodies,
        }),
        expected,
    )
}
fn remote(
    source: Arc<LegacySource>,
    receive: &[&str],
    runtime: &tokio::runtime::Runtime,
) -> (Remote, Arc<Mutex<Vec<ClipPlatform>>>, Arc<AtomicUsize>) {
    let targets = Arc::new(Mutex::new(Vec::new()));
    let opens = Arc::new(AtomicUsize::new(0));
    let link = Arc::new(LegacyLink {
        source: source.clone(),
        source_profile: profile(crate::negotiation::PORTABLE_FORMATS),
        target_profile: profile(receive),
        targets: targets.clone(),
        opens: opens.clone(),
    });
    let remote = Remote::on_runtime(
        Path::new(""),
        BatchId([1; 16]),
        source.inventory(),
        ClipPlatform::Unknown,
        link,
        Demand::Clipboard,
        runtime.handle().clone(),
    )
    .unwrap();
    (remote, targets, opens)
}
#[test]
fn linux_reads_explicit_portable_contracts_from_legacy_windows_and_mac_senders() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    for source_platform in [ClipPlatform::Windows, ClipPlatform::Mac] {
        let (source, expected) = source(source_platform);
        let (remote, targets, _) = remote(source, crate::negotiation::PORTABLE_FORMATS, &runtime);
        for (i, expected) in expected.iter().enumerate() {
            assert_eq!(
                &read_all(&mut remote.open(RepresentationId(i as u32 + 1)).unwrap()).unwrap(),
                expected
            );
        }
        assert_eq!(
            *targets.lock().unwrap(),
            vec![
                if source_platform == ClipPlatform::Windows {
                    ClipPlatform::Mac
                } else {
                    ClipPlatform::Windows
                };
                4
            ]
        );
        assert_eq!(
            remote.0.target,
            ClipPlatform::Unknown,
            "local publication/namespace policy remains Linux"
        );
    }
}
#[test]
fn portable_compatibility_does_not_authorize_unnegotiated_or_future_opaque_payloads() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (source, _) = source(ClipPlatform::Windows);
    let (remote, _, opens) = remote(source, &["mime:text/plain;charset=utf-8"], &runtime);
    assert_eq!(
        reason(&remote.open(RepresentationId(2)).err().unwrap()),
        Unavailable::Unsupported
    );
    assert_eq!(
        opens.load(Ordering::Relaxed),
        0,
        "no stream for an unnegotiated HTML format"
    );
    for standard in [None, Some(StandardFormat::Text)] {
        let (source, _) = self::source(ClipPlatform::Windows);
        let mut inventory = source.inventory();
        inventory.items[0].representations[0].format = NativeFormat::Unknown("future/opaque".into());
        inventory.items[0].representations[0].standard = standard;
        assert!(portable_request_target(&inventory, RepresentationId(1), ClipPlatform::Unknown).is_err());
    }
}
