//! Executable icon extraction, called only from the sampling worker.
use std::{mem::size_of, os::windows::ffi::OsStrExt, path::Path};
use windows::{
    Win32::{
        Graphics::Gdi::*,
        UI::{Shell::ExtractIconExW, WindowsAndMessaging::*},
    },
    core::PCWSTR,
};

pub fn executable_icon(path: &Path) -> Option<Vec<u8>> {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: all buffers remain valid through the calls. Every extracted icon,
    // bitmap and DC is released on both success and failure paths.
    unsafe {
        let mut icon = HICON::default();
        if ExtractIconExW(PCWSTR(path.as_ptr()), 0, Some(&mut icon), None, 1) == 0
            || icon.is_invalid()
        {
            return None;
        }
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            let _ = DestroyIcon(icon);
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 32,
                biHeight: -32,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bitmap) => bitmap,
            Err(_) => {
                let _ = DeleteDC(dc);
                let _ = DestroyIcon(icon);
                return None;
            }
        };
        let old = SelectObject(dc, bitmap.into());
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u8>(), 32 * 32 * 4);
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[40, 31, 24, 255]);
        }
        let drawn = DrawIconEx(dc, 0, 0, icon, 32, 32, 0, None, DI_NORMAL).is_ok();
        let mut rgba = pixels.to_vec();
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        let _ = DestroyIcon(icon);
        drawn.then_some(rgba)
    }
}
