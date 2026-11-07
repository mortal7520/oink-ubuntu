//! These exercise independent X11 clients, not an in-memory clipboard mock.
//! Run inside an isolated server: xvfb-run cargo test -p oink-clipboard
//! --lib linux::tests -- --ignored --test-threads=1
use super::*;
use std::sync::atomic::AtomicUsize;

fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "X11 clipboard did not converge");
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
#[ignore = "requires an isolated X11 server (Xvfb)"]
fn clipboard_cross_connection_text_png_files_incr_and_revocation() {
    super::test_server();
    let a = backend().unwrap();
    let b = backend().unwrap();
    let changes = Arc::new(AtomicUsize::new(0));
    let count = changes.clone();
    b.watch(Arc::new(move || {
        count.fetch_add(1, Ordering::Relaxed);
    }))
    .unwrap();
    let text = "剪贴板 🐷 café\n".repeat(24_000); // forces ICCCM INCR in both directions
    a.write(&[ClipData::Text(text.clone())], Sensitivity::Normal).unwrap();
    wait(|| b.read().is_ok_and(|s| s.items.contains(&ClipData::Text(text.clone()))));
    wait(|| changes.load(Ordering::Relaxed) > 0);

    let file = tempfile::Builder::new().prefix("oink 空格-").tempfile().unwrap();
    let path = file.path().to_owned();
    std::fs::write(&path, b"local file").unwrap();
    let entry = crate::FileEntry {
        path: path.file_name().unwrap().to_str().unwrap().into(),
        is_dir: false,
        size: 10,
        modified_ms: None,
        local_path: Some(path.clone()),
    };
    // Valid 1x1 PNG; exact bytes must round-trip without a bitmap conversion.
    let png = vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21,
        196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 0, 2, 0, 0, 5, 0, 1, 165, 246, 69, 64, 0, 0, 0, 0, 73,
        69, 78, 68, 174, 66, 96, 130,
    ];
    a.write(
        &[ClipData::Png(png.clone()), ClipData::Files(vec![entry])],
        Sensitivity::Normal,
    )
    .unwrap();
    wait(|| {
        b.read().is_ok_and(|s| {
            s.items.iter().any(|i| matches!(i, ClipData::Png(v) if v == &png))
                && s.items
                    .iter()
                    .any(|i| matches!(i, ClipData::Files(f) if f.len() == 1 && f[0].local_path.as_ref() == Some(&path)))
        })
    });

    a.write(&[ClipData::Text("source".into())], Sensitivity::Normal)
        .unwrap();
    let captured = loop {
        match b.capture(std::path::Path::new("")) {
            Ok(value) => break value,
            Err(ClipboardError::Busy) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => panic!("capture: {e}"),
        }
    };
    let provider = captured.content.unwrap();
    let id = provider.inventory().items[0].representations[0].id;
    let mut already_open = provider.open(id).unwrap();
    a.write(&[ClipData::Text("replacement".into())], Sensitivity::Normal)
        .unwrap();
    wait(|| b.change_count() != captured.snapshot.change_count);
    assert_eq!(
        content::reason(&provider.open(id).err().expect("stale provider accepted")),
        Unavailable::Superseded
    );
    assert_eq!(
        content::reason(
            &std::io::Read::read(&mut already_open, &mut [0; 8])
                .err()
                .expect("stale open reader accepted")
        ),
        Unavailable::Superseded
    );

    struct Observed {
        inner: Arc<dyn content::Provider>,
        opens: Arc<AtomicUsize>,
    }
    impl content::Provider for Observed {
        fn inventory(&self) -> Inventory {
            self.inner.inventory()
        }
        fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
            self.opens.fetch_add(1, Ordering::AcqRel);
            self.inner.open(id)
        }
        fn translator(&self) -> Arc<dyn content::Translator> {
            self.inner.translator()
        }
        fn reader_guard(&self, id: RepresentationId) -> io::Result<content::ReaderGuard> {
            self.inner.reader_guard(id)
        }
    }
    let opens = Arc::new(AtomicUsize::new(0));
    let native: Arc<dyn content::Provider> = Arc::new(Observed {
        inner: payload::owned(
            &[ClipData::Text("native publication".into()), ClipData::Png(png.clone())],
            1,
        )
        .unwrap(),
        opens: opens.clone(),
    });
    let before = a.change_count();
    b.write_native(NativeSpec {
        inventory: native.inventory(),
        contents: native,
        files: Vec::new(),
        file_contents: None,
        sensitivity: Sensitivity::Normal,
    })
    .unwrap();
    assert_eq!(
        opens.load(Ordering::Acquire),
        0,
        "native declaration must not read payloads"
    );
    wait(|| a.change_count() != before);
    let requested = a.read().unwrap();
    assert!(requested.items.contains(&ClipData::Text("native publication".into())));
    assert!(requested.items.contains(&ClipData::Png(png)));
    assert_eq!(
        opens.load(Ordering::Acquire),
        2,
        "each actual MIME request reads its negotiated representation once"
    );

    let provider = payload::owned(&[ClipData::Text("must not publish".into())], 1).unwrap();
    let spec = NativeSpec {
        inventory: provider.inventory(),
        contents: provider,
        files: Vec::new(),
        file_contents: None,
        sensitivity: Sensitivity::Normal,
    };
    assert!(matches!(
        b.write_native_guarded(spec, Arc::new(|| Err(io::ErrorKind::PermissionDenied.into()))),
        Err(ClipboardError::Busy)
    ));
    assert!(
        a.read()
            .unwrap()
            .items
            .contains(&ClipData::Text("native publication".into()))
    );
}

#[test]
#[ignore = "requires an isolated X11 server (Xvfb)"]
fn concealed_hint_and_pause_preserve_real_change_identity() {
    super::test_server();
    let a = backend().unwrap();
    let b = backend().unwrap();
    b.set_watch_paused(true).unwrap();
    let count = b.change_count();
    a.write(&[ClipData::Text("private test fixture".into())], Sensitivity::Concealed)
        .unwrap();
    wait(|| b.change_count() != count);
    let captured = b.capture(std::path::Path::new("")).unwrap();
    assert_eq!(captured.snapshot.sensitivity, Sensitivity::Concealed);
    b.set_watch_paused(false).unwrap();
}
