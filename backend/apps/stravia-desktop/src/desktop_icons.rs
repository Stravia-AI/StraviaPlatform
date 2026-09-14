//! Native window and tray icons stay black; only the WebUI brand mark follows its theme.
use tauri::{AppHandle, Manager, image::Image};

pub(crate) fn tray_image() -> Image<'static> {
    tauri::include_image!("icons/32x32.png")
}

fn window_image() -> Image<'static> {
    tauri::include_image!("icons/128x128.png")
}

pub(crate) fn setup(app: &AppHandle) -> tauri::Result<()> {
    for window in app.webview_windows().values() {
        window.set_icon(window_image())?;
    }
    if let Some(tray) = app.try_state::<crate::DesktopTray>() {
        // macOS colors template tray images to match its menu bar automatically.
        tray.tray
            .set_icon_with_as_template(Some(tray_image()), cfg!(target_os = "macos"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_icons_are_black_shapes_on_transparent_canvases() {
        for image in [tray_image(), window_image()] {
            let mut transparent = 0;
            let mut solid = 0;
            for pixel in image.rgba().chunks_exact(4) {
                if pixel[3] == 0 {
                    transparent += 1;
                } else {
                    assert_eq!(&pixel[..3], &[0, 0, 0]);
                    solid += 1;
                }
            }
            assert!(
                transparent > solid,
                "native icon must have a transparent canvas, not a tile"
            );
            assert!(solid > 0);
        }
    }
}
