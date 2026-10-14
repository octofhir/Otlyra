//! The dock icon.
//!
//! A bare `cargo run` binary is not an application bundle, so macOS has no
//! `CFBundleIconFile` to read and shows the generic executable icon in the Dock.
//! Setting the icon on `NSApplication` at startup fixes that for the unbundled
//! case, and is harmless when a bundle exists — the bundle's icon is used for the
//! Finder and this one for the running process, and they are the same image.
//!
//! `winit::Window::set_window_icon` does **not** do this: it is documented as
//! having no effect on macOS, because macOS windows have no per-window icon.

/// Set the application's Dock icon from an encoded image.
///
/// Best effort: an icon that fails to load is a cosmetic problem, never a reason to
/// fail a launch, so failures are logged and swallowed.
pub(crate) fn set_dock_icon(encoded: &[u8], trace: Option<&crate::StartupTrace>) {
    #[cfg(target_os = "macos")]
    {
        use objc2::AnyThread;
        use objc2_app_kit::{NSApplication, NSImage};
        use objc2_foundation::{MainThreadMarker, NSData, NSSize};

        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("dock icon must be set from the main thread; skipping");
            return;
        };

        let data = NSData::with_bytes(encoded);
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
            tracing::warn!("the dock icon could not be decoded; leaving the default");
            return;
        };
        // AppKit's icon installation cost grows with the logical canvas size.
        // Keep the full-resolution representation and its transparent margins,
        // but avoid asking the Dock to prepare a 1024-point canvas. The encoded
        // pixels are untouched; a 1024px source remains a 2x representation.
        let size = image.size();
        let longest = size.width.max(size.height);
        if longest > 512.0 {
            let scale = 512.0 / longest;
            image.setSize(NSSize::new(size.width * scale, size.height * scale));
        }
        if let Some(trace) = trace {
            trace.mark("icon_image_created");
        }

        // SAFETY: `setApplicationIconImage:` is only unsafe because it is an
        // Objective-C method. `mtm` proves we are on the main thread, which is the
        // method's one real requirement, and `image` is a live, initialized
        // `NSImage` we own.
        unsafe {
            NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image));
        }
        tracing::debug!(bytes = encoded.len(), "dock icon set");
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (encoded, trace);
    }
}
