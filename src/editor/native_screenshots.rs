use super::{Editor, actions::Action};
use gpui::*;
use std::sync::Arc;

pub(super) struct State {
    pub(super) enabled: bool,
    pub(super) error: Option<String>,
    pub(super) pending: Option<Arc<image::RgbaImage>>,
    pub(super) clean_revision: u64,
    #[cfg(target_os = "macos")]
    pub(super) epoch: u64,
    #[cfg(target_os = "macos")]
    pub(super) monitor: Option<crate::native_screenshots::Monitor>,
    #[cfg(target_os = "macos")]
    pub(super) timer: Option<Task<()>>,
    #[cfg(target_os = "macos")]
    native: bool,
}
impl State {
    pub(super) fn new(native: bool) -> Self {
        #[cfg(not(target_os = "macos"))]
        let _ = native;
        #[cfg(target_os = "macos")]
        let enabled = native && crate::native_screenshots::load_enabled();
        #[cfg(not(target_os = "macos"))]
        let enabled = false;
        #[cfg(target_os = "macos")]
        let result = enabled.then(|| crate::native_screenshots::Monitor::new(false));
        Self {
            enabled,
            #[cfg(target_os = "macos")]
            error: result.as_ref().and_then(|r| r.as_ref().err().cloned()),
            #[cfg(not(target_os = "macos"))]
            error: None,
            pending: None,
            clean_revision: 0,
            #[cfg(target_os = "macos")]
            epoch: 0,
            #[cfg(target_os = "macos")]
            monitor: result.and_then(Result::ok),
            #[cfg(target_os = "macos")]
            timer: None,
            #[cfg(target_os = "macos")]
            native,
        }
    }
    pub(super) fn active(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.monitor.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
    pub(super) fn cancel_capture(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(monitor) = &mut self.monitor {
            monitor.cancel();
        }
    }
}

impl Editor {
    pub(super) fn set_native_screenshot_import(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        #[cfg(not(target_os = "macos"))]
        if enabled {
            return Err("Native screenshot observation is available on macOS only.".into());
        }
        self.native_screenshots.enabled = enabled;
        self.native_screenshots.error = None;
        #[cfg(target_os = "macos")]
        {
            self.native_screenshots.epoch += 1;
            self.native_screenshots.monitor = None;
            if self.native_screenshots.native {
                crate::native_screenshots::save_enabled(enabled);
                if enabled {
                    match crate::native_screenshots::Monitor::new(true) {
                        Ok(monitor) => self.native_screenshots.monitor = Some(monitor),
                        Err(error) => self.native_screenshots.error = Some(error),
                    }
                }
            }
        }
        if !enabled {
            self.native_screenshots.pending = None;
        }
        cx.notify();
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(super) fn poll_native_screenshots(&mut self) {
        let Some(monitor) = &mut self.native_screenshots.monitor else {
            return;
        };
        match monitor.poll() {
            Ok(Some((token, generation))) => {
                let epoch = self.native_screenshots.epoch;
                let sender = self.sender.clone();
                std::thread::spawn(move || {
                    let result = crate::native_screenshots::snapshot(generation)
                        .map(|image| image.map(Arc::new));
                    let _ = sender.send_blocking(super::Message::NativeScreenshot {
                        epoch,
                        token,
                        generation,
                        result,
                    });
                });
            }
            Ok(None) => {}
            Err(error) => {
                self.native_screenshots.error = Some(error);
                self.native_screenshots.monitor = None;
                self.native_screenshots.epoch += 1;
                // Deliver through the normal receiver so state and UI update together.
                let _ = self
                    .sender
                    .try_send(super::Message::NativeScreenshotObserverStopped);
            }
        }
    }

    pub(super) fn prepare_native_screenshot(
        &mut self,
        image: Arc<image::RgbaImage>,
        cx: &mut Context<Self>,
    ) {
        if !self.native_screenshots.enabled {
            return;
        }
        self.native_screenshots.pending = Some(image);
        if self.preview.revision == self.native_screenshots.clean_revision
            && !self.is_busy()
            && !self.interaction.gesture.is_active()
            && self.interaction.text_edit.is_none()
        {
            self.dispatch_ui(Action::OpenNativeScreenshot, cx);
        } else {
            self.feedback.status = "Latest screenshot ready. Open replaces the current image; Dismiss keeps your work.".into();
            crate::platform::show_editor(cx);
        }
    }

    pub(super) fn open_native_screenshot(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        if self.interaction.gesture.is_active() || self.interaction.text_edit.is_some() {
            return Err(
                "Finish the current text edit or gesture before opening the screenshot.".into(),
            );
        }
        let image = self
            .native_screenshots
            .pending
            .take()
            .ok_or("No native screenshot is waiting.")?;
        self.install_image(image);
        crate::platform::show_editor(cx);
        Ok(())
    }

    /// Shared installation for captured, opened, pasted and prepared images.
    pub(super) fn install_image(&mut self, image: Arc<image::RgbaImage>) {
        self.set_selection(Vec::new());
        self.interaction.gesture = super::state::Gesture::Idle;
        self.document = crate::document::Document::from_shared(image);
        self.panels.backdrop_disabled = None;
        self.panels.sampling_color = None;
        self.panels.sampling_tool_color = false;
        self.panels.popup = None;
        self.viewport.zoom = None;
        self.viewport.pan = (0., 0.);
        self.preview.mark_count = usize::MAX;
        self.preview.waiting = true;
        self.changed();
        self.native_screenshots.clean_revision = self.preview.revision;
        self.feedback.status = "Preparing image…".into();
    }
}
