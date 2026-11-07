//! Explicit per-user PulseAudio module lifecycle; no administrator or default-device changes.
use super::*;
use std::sync::Mutex;
struct Last {
    successful: Option<Action>,
    result: Option<bool>,
    detail: Option<String>,
}
static LAST: Mutex<Last> = Mutex::new(Last {
    successful: None,
    result: None,
    detail: None,
});
pub(super) fn status() -> Inspection {
    let mut inspection = Inspection::default();
    let version = env!("CARGO_PKG_VERSION").to_string();
    inspection.status.bundled_version = Some(version.clone());
    inspection.status.bundled_plugin_version = Some("pulse-session-v1".into());
    match oink_audio::microphone::linux::virtual_installation() {
        Ok((managed, oink_audio::microphone::VirtualOutput::Ready { render_id, .. })) => {
            inspection.status.managed = managed;
            inspection.status.input_loaded = true;
            inspection.status.feed_loaded = true;
            inspection.status.state = if managed {
                MicrophonePluginState::Installed
            } else {
                MicrophonePluginState::Unmanaged
            };
            if managed {
                inspection.render_id = Some(render_id);
                inspection.status.installed_version = Some(version);
                inspection.status.installed_plugin_version = Some("pulse-session-v1".into());
            } else {
                inspection.status.detail = Some(
                    "Oink-named devices exist without an ownership receipt; no foreign modules will be removed".into(),
                );
            }
        }
        Ok((_, oink_audio::microphone::VirtualOutput::Missing)) => {
            inspection.status.state = MicrophonePluginState::NotInstalled;
        }
        Ok((_, other)) => {
            inspection.status.state = MicrophonePluginState::Error;
            inspection.status.error_kind = Some(MicrophonePluginErrorKind::QueryFailed);
            inspection.status.detail = Some(format!("virtual microphone unavailable: {other:?}"));
        }
        Err(e) => {
            inspection.status.state = MicrophonePluginState::Error;
            inspection.status.error_kind = Some(MicrophonePluginErrorKind::QueryFailed);
            inspection.status.detail = Some(e);
        }
    }
    let last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    inspection.status.last_operation = last.successful;
    inspection.status.last_operation_succeeded = last.result;
    if last.result == Some(false) {
        inspection.status.error_kind = Some(MicrophonePluginErrorKind::Other);
        inspection.status.detail = last.detail.clone();
    }
    inspection
}
pub(super) fn request(
    action: Action,
    guard: Option<crate::audio::DriverChangeGuard>,
) -> Result<MicrophonePluginStatus, String> {
    std::thread::Builder::new()
        .name("oink-linux-microphone-modules".into())
        .spawn(move || {
            let _guard = guard;
            cache::progress(match action {
                Action::Install => MicrophonePluginStep::InstallDriver,
                Action::Uninstall => MicrophonePluginStep::UninstallDriver,
            });
            let result = match action {
                Action::Install => oink_audio::microphone::linux::install_virtual(),
                Action::Uninstall => oink_audio::microphone::linux::uninstall_virtual(),
            };
            let success = result.is_ok();
            let detail = result
                .err()
                .unwrap_or_else(|| "PulseAudio session modules updated without changing default devices".into());
            {
                let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
                if success {
                    last.successful = Some(action);
                }
                last.result = Some(success);
                last.detail = Some(detail);
            }
            oink_audio::device_notifications::invalidate();
            cache::completed();
        })
        .map_err(|e| e.to_string())?;
    Ok(super::status())
}
