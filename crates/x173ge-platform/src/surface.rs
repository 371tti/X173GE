//! Converts winit's borrowed native handles into compact protocol descriptors.

use std::num::NonZeroUsize;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::window::Window;
use x173ge_core::{Extent2d, SurfaceGeneration, SurfaceId};
use x173ge_protocol::{NativeSurfaceHandle, SurfaceDescriptor, SurfaceToken};

/// Failure to obtain a stable native handle for the renderer thread.
#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error(transparent)]
    Handle(#[from] raw_window_handle::HandleError),
    #[error("window and display handle kinds do not form a supported Vulkan surface")]
    Unsupported,
    #[error("native surface handle contains a null or zero value")]
    NullHandle,
}

/// Copies the native identity while the caller keeps the window alive.
pub fn describe_surface(
    window: &Window,
    id: SurfaceId,
    generation: SurfaceGeneration,
    extent: Extent2d,
    vsync: bool,
) -> Result<SurfaceDescriptor, SurfaceError> {
    let raw_window = window.window_handle()?.as_raw();
    let raw_display = window.display_handle()?.as_raw();
    let native = match (raw_display, raw_window) {
        (RawDisplayHandle::Windows(_), RawWindowHandle::Win32(handle)) => {
            NativeSurfaceHandle::Win32 {
                hwnd: handle.hwnd,
                hinstance: handle.hinstance.ok_or(SurfaceError::NullHandle)?,
            }
        }
        (RawDisplayHandle::Xlib(display), RawWindowHandle::Xlib(window)) => {
            NativeSurfaceHandle::X11 {
                display: NonZeroUsize::new(
                    display.display.ok_or(SurfaceError::NullHandle)?.as_ptr() as usize,
                )
                .ok_or(SurfaceError::NullHandle)?,
                window: std::num::NonZeroU32::new(window.window).ok_or(SurfaceError::NullHandle)?,
            }
        }
        (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(window)) => {
            NativeSurfaceHandle::Wayland {
                display: NonZeroUsize::new(display.display.as_ptr() as usize)
                    .ok_or(SurfaceError::NullHandle)?,
                surface: NonZeroUsize::new(window.surface.as_ptr() as usize)
                    .ok_or(SurfaceError::NullHandle)?,
            }
        }
        _ => return Err(SurfaceError::Unsupported),
    };

    Ok(SurfaceDescriptor {
        token: SurfaceToken { id, generation },
        extent,
        native,
        vsync,
    })
}

/// Converts a physical window size into a drawable nonzero extent.
pub fn drawable_extent(width: u32, height: u32) -> Option<Extent2d> {
    Extent2d::new(width, height)
}
