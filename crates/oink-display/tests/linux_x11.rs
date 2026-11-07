#![cfg(target_os = "linux")]
#[test]
#[ignore = "requires an explicitly isolated Xvfb DISPLAY"]
fn x11_displays_and_native_baseline() {
    let display = std::env::var("DISPLAY").unwrap();
    assert_eq!(
        std::env::var("OINK_X11_TEST_DISPLAY").ok().as_deref(),
        Some(display.as_str())
    );
    let backend = oink_display::platform_backend().unwrap();
    let displays = backend.displays().unwrap();
    assert!(!displays.is_empty());
    assert_eq!(displays.iter().filter(|d| d.is_primary).count(), 1);
    for d in &displays {
        assert!(!d.key.is_empty());
        assert!(d.bounds.w > 0. && d.bounds.h > 0.);
        assert!(d.pixel_width > 0 && d.pixel_height > 0);
    }
    assert_eq!(displays, backend.displays().unwrap());
    let baseline = backend.native_configuration().unwrap();
    assert!(!baseline.displays.is_empty());
    backend.watch(std::sync::Arc::new(|| {})).unwrap();
}
