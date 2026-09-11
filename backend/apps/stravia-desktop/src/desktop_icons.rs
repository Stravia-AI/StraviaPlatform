//! Native icons follow the OS shell, independently of the WebUI theme.
use std::sync::Mutex;

use tauri::{AppHandle, Manager, Theme, image::Image};

#[cfg(target_os = "windows")]
mod windows_theme;

#[derive(Default)]
pub(crate) struct DesktopIcons {
    applied: Mutex<Option<Theme>>,
}

#[cfg(feature = "desktop-e2e")]
#[tauri::command]
pub(crate) fn get_desktop_icon_theme(
    state: tauri::State<'_, DesktopIcons>,
) -> Option<&'static str> {
    state
        .applied
        .lock()
        .expect("desktop icon state poisoned")
        .map(|theme| {
            if theme == Theme::Dark {
                "dark"
            } else {
                "light"
            }
        })
}

pub(crate) fn tray_image(theme: Theme) -> Image<'static> {
    match theme {
        Theme::Dark => tauri::include_image!("icons/system-dark-32.png"),
        _ => tauri::include_image!("icons/system-light-32.png"),
    }
}

fn window_image(theme: Theme) -> Image<'static> {
    match theme {
        Theme::Dark => tauri::include_image!("icons/system-dark-128.png"),
        _ => tauri::include_image!("icons/system-light-128.png"),
    }
}

pub(crate) fn current_theme(app: &AppHandle) -> Theme {
    let fallback = app
        .get_webview_window("main")
        .and_then(|w| w.theme().ok())
        .unwrap_or(Theme::Light);
    #[cfg(target_os = "windows")]
    {
        match windows_theme::system_theme() {
            Ok(Some(theme)) => return theme,
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "could not read Windows shell theme; using window theme")
            }
        }
    }
    fallback
}

pub(crate) fn apply(app: &AppHandle, theme: Theme) -> tauri::Result<()> {
    let state = app.state::<DesktopIcons>();
    let mut applied = state.applied.lock().expect("desktop icon state poisoned");
    if *applied == Some(theme) {
        return Ok(());
    }
    for window in app.webview_windows().values() {
        window.set_icon(window_image(theme))?;
    }
    if let Some(tray) = app.try_state::<crate::DesktopTray>() {
        // macOS colors template tray images to match its menu bar automatically.
        let image = if cfg!(target_os = "macos") {
            tray_image(Theme::Light)
        } else {
            tray_image(theme)
        };
        tray.tray
            .set_icon_with_as_template(Some(image), cfg!(target_os = "macos"))?;
    }
    *applied = Some(theme);
    Ok(())
}

pub(crate) fn setup(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    app.manage(DesktopIcons::default());
    apply(app, current_theme(app))?;
    #[cfg(target_os = "windows")]
    {
        let handle = app.clone();
        match windows_theme::ThemeWatcher::start(move |theme| {
            let callback_app = handle.clone();
            if let Err(error) = handle.run_on_main_thread(move || {
                if let Err(error) = apply(
                    &callback_app,
                    theme.unwrap_or_else(|| current_theme(&callback_app)),
                ) {
                    tracing::error!(%error, "could not update desktop icons");
                }
            }) {
                tracing::error!(%error, "could not schedule desktop icon update");
            }
        }) {
            Ok(watcher) => {
                app.manage(watcher);
            }
            Err(error) => {
                tracing::warn!(%error, "Windows shell theme notifications are unavailable")
            }
        }
    }
    Ok(())
}

pub(crate) fn theme_changed(app: &AppHandle) {
    if app.try_state::<DesktopIcons>().is_some() {
        if let Err(error) = apply(app, current_theme(app)) {
            tracing::error!(%error, "could not update desktop icons after theme change");
        }
    }
}

pub(crate) fn shutdown(_app: &AppHandle) {
    #[cfg(target_os = "windows")]
    if let Some(watcher) = _app.try_state::<windows_theme::ThemeWatcher>() {
        watcher.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_keep_the_same_transparent_shape_with_opposite_contrast() {
        for (light, dark) in [
            (tray_image(Theme::Light), tray_image(Theme::Dark)),
            (window_image(Theme::Light), window_image(Theme::Dark)),
        ] {
            assert_eq!(
                (light.width(), light.height()),
                (dark.width(), dark.height())
            );
            let mut transparent = 0;
            let mut solid = 0;
            for (l, d) in light
                .rgba()
                .chunks_exact(4)
                .zip(dark.rgba().chunks_exact(4))
            {
                assert_eq!(l[3], d[3]);
                if l[3] == 0 {
                    transparent += 1;
                } else {
                    assert_eq!(&l[..3], &[0, 0, 0]);
                    assert_eq!(&d[..3], &[255, 255, 255]);
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
