use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{
    Manager, PhysicalPosition, PhysicalRect, PhysicalSize, Window, WindowEvent,
    utils::config::WindowConfig,
};
use tauri_plugin_store::{Store, StoreExt};

const STORE_FILE: &str = "window-geometry.json";
const MAIN_WINDOW: &str = "main";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Geometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximized: bool,
}

impl Geometry {
    fn fit(self, work_area: PhysicalRect<i32, u32>) -> Option<Self> {
        if self.width == 0
            || self.height == 0
            || work_area.size.width == 0
            || work_area.size.height == 0
        {
            return None;
        }
        let left = i64::from(work_area.position.x);
        let top = i64::from(work_area.position.y);
        let right = left + i64::from(work_area.size.width);
        let bottom = top + i64::from(work_area.size.height);
        let x = i64::from(self.x);
        let y = i64::from(self.y);
        // 断开外接显示器后不能让恢复窗口留在屏幕外；仍在屏幕上的窗口则保留原位置。
        if (x + i64::from(self.width)).min(right) - x.max(left) < 64
            || (y + i64::from(self.height)).min(bottom) - y.max(top) < 40
        {
            return None;
        }
        let width = self.width.min(work_area.size.width);
        let height = self.height.min(work_area.size.height);
        Some(Self {
            x: x.clamp(left, right - i64::from(width)) as i32,
            y: y.clamp(top, bottom - i64::from(height)) as i32,
            width,
            height,
            ..self
        })
    }
}

pub(crate) struct WindowGeometry {
    store: Arc<Store<tauri::Wry>>,
    normal: Mutex<Option<Geometry>>,
    tracking: AtomicBool,
}

impl WindowGeometry {
    pub(crate) fn open(app: &tauri::AppHandle) -> anyhow::Result<Self> {
        let path = app.path().app_config_dir()?.join(STORE_FILE);
        let store = app.store_builder(path).disable_auto_save().build()?;
        match store.reload() {
            Ok(()) => {}
            Err(tauri_plugin_store::Error::Io(error))
                if error.kind() == io::ErrorKind::NotFound => {}
            Err(tauri_plugin_store::Error::Deserialize(_) | tauri_plugin_store::Error::Json(_)) => {
                tracing::warn!("invalid desktop window geometry store; using default geometry");
                store.clear();
            }
            Err(error) => return Err(error.into()),
        }
        Ok(Self {
            store,
            normal: Mutex::new(None),
            tracking: AtomicBool::new(false),
        })
    }

    pub(crate) fn configure(
        &self,
        app: &tauri::AppHandle,
        config: &mut WindowConfig,
    ) -> tauri::Result<()> {
        let stored = self.store.get(MAIN_WINDOW).and_then(|value| {
            serde_json::from_value::<Geometry>(value)
                .map_err(|error| {
                    tracing::warn!(%error, "invalid desktop window geometry; using defaults");
                })
                .ok()
        });
        if let Some(saved) = stored
            && let Some((geometry, scale)) =
                app.available_monitors()?.into_iter().find_map(|monitor| {
                    saved
                        .fit(*monitor.work_area())
                        .map(|g| (g, monitor.scale_factor()))
                })
        {
            config.x = Some(f64::from(geometry.x) / scale);
            config.y = Some(f64::from(geometry.y) / scale);
            config.width = f64::from(geometry.width) / scale;
            config.height = f64::from(geometry.height) / scale;
            config.maximized = geometry.maximized;
            *self.normal.lock() = Some(geometry);
            return Ok(());
        }

        // 初次启动的 1440×900 不应在较小工作区被任务栏或屏幕边缘裁切。
        if let Some(monitor) = app.primary_monitor()? {
            let area = monitor.work_area();
            let scale = monitor.scale_factor();
            let size = PhysicalSize::new(
                (config.width * scale) as u32,
                (config.height * scale) as u32,
            );
            let width = size.width.min(area.size.width.saturating_sub(64));
            let height = size.height.min(area.size.height.saturating_sub(64));
            let x = area.position.x + ((area.size.width - width) / 2) as i32;
            let y = area.position.y + ((area.size.height - height) / 2) as i32;
            config.x = Some(f64::from(x) / scale);
            config.y = Some(f64::from(y) / scale);
            config.width = f64::from(width) / scale;
            config.height = f64::from(height) / scale;
            *self.normal.lock() = Some(Geometry {
                x,
                y,
                width,
                height,
                maximized: false,
            });
        }
        Ok(())
    }

    pub(crate) fn position_after_show(&self, window: &Window) -> tauri::Result<()> {
        if !self.tracking.load(Ordering::Acquire)
            && !window.is_maximized()?
            && let Some(geometry) = *self.normal.lock()
        {
            // Windows may place a hidden WebView on another monitor when it first becomes visible.
            window.set_position(PhysicalPosition::new(geometry.x, geometry.y))?;
        }
        Ok(())
    }

    pub(crate) fn start_tracking(&self, window: &Window) {
        if window.is_maximized().is_ok_and(|maximized| !maximized) {
            self.capture_normal(window);
        }
        self.tracking.store(true, Ordering::Release);
    }

    pub(crate) fn record_event(&self, window: &Window, event: &WindowEvent) {
        if self.tracking.load(Ordering::Acquire)
            && matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_))
            && window.is_maximized().is_ok_and(|maximized| !maximized)
            && window.is_minimized().is_ok_and(|minimized| !minimized)
            && window.is_fullscreen().is_ok_and(|fullscreen| !fullscreen)
        {
            self.capture_normal(window);
        }
    }

    fn capture_normal(&self, window: &Window) {
        match (window.outer_position(), window.inner_size()) {
            (Ok(position), Ok(size)) if size.width > 0 && size.height > 0 => {
                *self.normal.lock() = Some(Geometry {
                    x: position.x,
                    y: position.y,
                    width: size.width,
                    height: size.height,
                    maximized: false,
                });
            }
            (Err(error), _) | (_, Err(error)) => {
                tracing::debug!(%error, "failed to inspect desktop window geometry");
            }
            _ => {}
        }
    }

    pub(crate) fn save(&self, window: &Window) -> anyhow::Result<()> {
        if !self.tracking.load(Ordering::Acquire) {
            return Ok(());
        }
        let maximized = window.is_maximized()?;
        if !maximized && !window.is_minimized()? && !window.is_fullscreen()? {
            self.capture_normal(window);
        }
        let Some(mut geometry) = *self.normal.lock() else {
            return Ok(());
        };
        geometry.maximized = maximized;
        self.store.set(MAIN_WINDOW, serde_json::to_value(geometry)?);
        self.store.save()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_visible_position_and_clamps_geometry_to_work_area() {
        let area = PhysicalRect {
            position: PhysicalPosition::new(-1920, 0),
            size: PhysicalSize::new(1920, 1080),
        };
        let saved = Geometry {
            x: -1700,
            y: 100,
            width: 1400,
            height: 900,
            maximized: true,
        };
        assert_eq!(saved.fit(area), Some(saved));
        assert_eq!(
            Geometry {
                x: -300,
                y: 900,
                ..saved
            }
            .fit(area),
            Some(Geometry {
                x: -1400,
                y: 180,
                ..saved
            })
        );
        assert_eq!(
            Geometry {
                x: -1920,
                y: 0,
                width: 2200,
                height: 1400,
                ..saved
            }
            .fit(area),
            Some(Geometry {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
                ..saved
            })
        );
    }

    #[test]
    fn removed_monitor_or_invalid_size_uses_default_geometry() {
        let area = PhysicalRect {
            position: PhysicalPosition::new(0, 0),
            size: PhysicalSize::new(1366, 768),
        };
        let saved = Geometry {
            x: -1500,
            y: 100,
            width: 1200,
            height: 800,
            maximized: false,
        };
        assert_eq!(saved.fit(area), None);
        assert_eq!(Geometry { width: 0, ..saved }.fit(area), None);
    }
}
