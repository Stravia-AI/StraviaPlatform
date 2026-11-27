use tauri::{
    utils::config::{Color, WindowConfig, WindowEffectsConfig},
    window::Effect,
};

/// Windows 11 21H2 起始 Build；低版本（全部 Windows 10）不支持原生 Mica，
/// 且启用未公开 Acrylic 会在 Windows 10 1903+ 导致 DWM 拖拽严重掉帧与发飘。
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
const WINDOWS_MICA_MIN_BUILD: u32 = 22_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowMaterial {
    Opaque,
    Mica,
    Sidebar,
}

impl WindowMaterial {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Opaque => "opaque",
            Self::Mica => "mica",
            Self::Sidebar => "sidebar",
        }
    }

    /// 在文档解析前注入宿主实际生效的窗口材质，避免前端靠冻结的 User-Agent 误判或首帧闪烁。
    pub(crate) fn initialization_script(self) -> String {
        format!(
            "Object.defineProperty(window, '__STRAVIA_WINDOW_MATERIAL__', {{ value: {:?}, writable: false, configurable: true }});",
            self.as_str()
        )
    }
}

pub(crate) fn configure(config: &mut WindowConfig) -> WindowMaterial {
    #[cfg(target_os = "windows")]
    {
        configure_windows_for_build(config, windows_build_number())
    }
    #[cfg(target_os = "macos")]
    {
        resolve_macos_material(config)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = config;
        WindowMaterial::Opaque
    }
}

#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
fn configure_windows_for_build(config: &mut WindowConfig, build: Option<u32>) -> WindowMaterial {
    if build.is_some_and(|build| build >= WINDOWS_MICA_MIN_BUILD) {
        config.transparent = true;
        config.background_color = Some(Color(0, 0, 0, 0));
        config.window_effects = Some(WindowEffectsConfig {
            effects: vec![Effect::Mica],
            ..Default::default()
        });
        WindowMaterial::Mica
    } else {
        config.transparent = false;
        config.background_color = None;
        config.window_effects = None;
        WindowMaterial::Opaque
    }
}

#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn resolve_macos_material(config: &WindowConfig) -> WindowMaterial {
    let has_sidebar = config.transparent
        && config
            .window_effects
            .as_ref()
            .is_some_and(|effects| effects.effects.contains(&Effect::Sidebar));
    if has_sidebar {
        WindowMaterial::Sidebar
    } else {
        WindowMaterial::Opaque
    }
}

#[cfg(target_os = "windows")]
fn windows_build_number() -> Option<u32> {
    use windows_sys::{
        Wdk::System::SystemServices::RtlGetVersion,
        Win32::System::SystemInformation::OSVERSIONINFOW,
    };

    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    let status = unsafe { RtlGetVersion(&mut info) };
    (status == 0 && info.dwBuildNumber > 0).then_some(info.dwBuildNumber)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_window_config() -> WindowConfig {
        WindowConfig {
            transparent: true,
            background_color: Some(Color(0, 0, 0, 0)),
            window_effects: Some(WindowEffectsConfig {
                effects: vec![Effect::Acrylic],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn windows_10_and_unknown_builds_downgrade_to_opaque() {
        for build in [None, Some(17_763), Some(18_362), Some(19_045), Some(21_999)] {
            let mut config = sample_window_config();
            let material = configure_windows_for_build(&mut config, build);
            assert_eq!(material, WindowMaterial::Opaque);
            assert!(!config.transparent);
            assert_eq!(config.background_color, None);
            assert_eq!(config.window_effects, None);
        }
    }

    #[test]
    fn windows_11_builds_enable_mica_backdrop() {
        for build in [Some(22_000), Some(22_523), Some(22_621), Some(26_100)] {
            let mut config = WindowConfig::default();
            let material = configure_windows_for_build(&mut config, build);
            assert_eq!(material, WindowMaterial::Mica);
            assert!(config.transparent);
            assert_eq!(config.background_color, Some(Color(0, 0, 0, 0)));
            assert_eq!(
                config.window_effects,
                Some(WindowEffectsConfig {
                    effects: vec![Effect::Mica],
                    ..Default::default()
                })
            );
        }
    }

    #[test]
    fn macos_material_matches_sidebar_configuration() {
        let mut config = WindowConfig {
            transparent: true,
            window_effects: Some(WindowEffectsConfig {
                effects: vec![Effect::Sidebar],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_macos_material(&config), WindowMaterial::Sidebar);

        config.transparent = false;
        assert_eq!(resolve_macos_material(&config), WindowMaterial::Opaque);
    }

    #[test]
    fn initialization_script_declares_material_literal() {
        assert!(
            WindowMaterial::Mica
                .initialization_script()
                .contains("\"mica\"")
        );
        assert!(
            WindowMaterial::Opaque
                .initialization_script()
                .contains("\"opaque\"")
        );
        assert!(
            WindowMaterial::Sidebar
                .initialization_script()
                .contains("\"sidebar\"")
        );
    }
}
