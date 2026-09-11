use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{null, null_mut},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

use tauri::Theme;
use windows_sys::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WAIT_OBJECT_0},
        System::{
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, KEY_QUERY_VALUE, REG_NOTIFY_CHANGE_LAST_SET,
                REG_NOTIFY_THREAD_AGNOSTIC, RRF_RT_REG_DWORD, RegCloseKey, RegGetValueW,
                RegNotifyChangeKeyValue, RegOpenKeyExW,
            },
            Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects},
        },
    },
    core::w,
};

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

fn personalize_path() -> String {
    // Native E2E exercises the real registry notifications in an isolated key.
    // Release builds always read the actual Windows shell preference.
    #[cfg(feature = "desktop-e2e")]
    if let Ok(path) = std::env::var("STRAVIA_DESKTOP_E2E_THEME_KEY") {
        if let Some(name) = path.strip_prefix(r"Software\Stravia\Tests\stravia-desktop-e2e-") {
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return path;
            }
        }
    }
    PERSONALIZE.into()
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

struct RegistryKey(HKEY);
// Registry handles are process-wide. Ownership moves to the watcher thread,
// and RegCloseKey runs only after that thread has finished using the handle.
unsafe impl Send for RegistryKey {}

impl RegistryKey {
    fn open(path: &str) -> io::Result<Self> {
        let mut key = null_mut();
        let path = wide(path);
        // SAFETY: path is NUL-terminated; key points to writable storage.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                KEY_QUERY_VALUE | KEY_NOTIFY,
                &mut key,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(Self(key))
    }

    fn read(&self) -> io::Result<Option<Theme>> {
        let mut value = 0u32;
        let mut len = size_of::<u32>() as u32;
        // Use the shell preference, not AppsUseLightTheme: Windows supports mixed modes.
        // SAFETY: self owns a live registry handle; the typed DWORD output has len bytes.
        let status = unsafe {
            RegGetValueW(
                self.0,
                null(),
                w!("SystemUsesLightTheme"),
                RRF_RT_REG_DWORD,
                null_mut(),
                (&mut value as *mut u32).cast(),
                &mut len,
            )
        };
        match status {
            ERROR_SUCCESS => Ok(match value {
                0 => Some(Theme::Dark),
                1 => Some(Theme::Light),
                _ => None,
            }),
            ERROR_FILE_NOT_FOUND => Ok(None),
            _ => Err(io::Error::from_raw_os_error(status as i32)),
        }
    }

    fn arm(&self, changed: &OwnedHandle) -> io::Result<()> {
        // SAFETY: both handles remain alive through the subsequent wait. Thread-agnostic
        // registration also covers the initial subscription made before spawning.
        let status = unsafe {
            RegNotifyChangeKeyValue(
                self.0,
                0,
                REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_THREAD_AGNOSTIC,
                changed.as_raw_handle(),
                1,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }
}

impl Drop for RegistryKey {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner of the opened registry key.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn event(manual_reset: bool) -> io::Result<OwnedHandle> {
    // SAFETY: unnamed event with default security, initially unsignaled.
    let handle = unsafe { CreateEventW(null(), i32::from(manual_reset), 0, null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateEventW returned a new owned handle, closed by OwnedHandle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub(super) fn system_theme() -> io::Result<Option<Theme>> {
    RegistryKey::open(&personalize_path())?.read()
}

pub(super) struct ThemeWatcher {
    stop: Arc<OwnedHandle>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ThemeWatcher {
    pub(super) fn start(callback: impl Fn(Option<Theme>) + Send + 'static) -> io::Result<Self> {
        Self::start_at(&personalize_path(), callback)
    }

    fn start_at(path: &str, callback: impl Fn(Option<Theme>) + Send + 'static) -> io::Result<Self> {
        let key = RegistryKey::open(path)?;
        let changed = event(false)?;
        let stop = Arc::new(event(true)?);
        key.arm(&changed)?;
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("stravia-shell-theme".into())
            .spawn(move || {
                let run = || -> io::Result<()> {
                    let mut last = None;
                    loop {
                        let theme = key.read()?;
                        if last != Some(theme) {
                            callback(theme);
                            last = Some(theme);
                        }
                        let handles = [thread_stop.as_raw_handle(), changed.as_raw_handle()];
                        // SAFETY: handles stay alive for the entire blocking wait. Stop is
                        // first so shutdown wins if a theme change is also pending.
                        let result =
                            unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
                        if result == WAIT_OBJECT_0 {
                            return Ok(());
                        }
                        if result != WAIT_OBJECT_0 + 1 {
                            return Err(io::Error::last_os_error());
                        }
                        // Re-arm before reading so rapid changes cannot fall into a gap.
                        key.arm(&changed)?;
                    }
                };
                if let Err(error) = run() {
                    tracing::warn!(%error, "Windows shell theme watcher stopped");
                }
            })?;
        Ok(Self {
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub(super) fn stop(&self) {
        let mut thread = self
            .thread
            .lock()
            .expect("shell theme watcher state poisoned");
        if let Some(handle) = thread.take() {
            // SAFETY: self owns the stop event until the worker has exited.
            if unsafe { SetEvent(self.stop.as_raw_handle()) } == 0 {
                tracing::error!(error = %io::Error::last_os_error(), "could not stop shell theme watcher");
                return;
            }
            if handle.join().is_err() {
                tracing::error!("shell theme watcher thread panicked");
            }
        }
    }
}

impl Drop for ThemeWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};
    use windows_sys::Win32::System::Registry::{
        REG_DWORD, RegCreateKeyW, RegDeleteTreeW, RegSetKeyValueW,
    };

    struct TestKey {
        key: RegistryKey,
        path: String,
    }
    impl TestKey {
        fn new() -> Self {
            let path = format!(
                r"Software\Stravia\Tests\shell-theme-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let mut raw = null_mut();
            let name = wide(&path);
            assert_eq!(
                unsafe { RegCreateKeyW(HKEY_CURRENT_USER, name.as_ptr(), &mut raw) },
                ERROR_SUCCESS
            );
            Self {
                key: RegistryKey(raw),
                path,
            }
        }
        fn set(&self, name: &str, value: u32) {
            let name = wide(name);
            assert_eq!(
                unsafe {
                    RegSetKeyValueW(
                        self.key.0,
                        null(),
                        name.as_ptr(),
                        REG_DWORD,
                        (&value as *const u32).cast(),
                        4,
                    )
                },
                ERROR_SUCCESS
            );
        }
    }
    impl Drop for TestKey {
        fn drop(&mut self) {
            let path = wide(&self.path);
            unsafe {
                RegDeleteTreeW(HKEY_CURRENT_USER, path.as_ptr());
            }
        }
    }

    #[test]
    fn shell_preference_overrides_opposite_app_preference() {
        let fixture = TestKey::new();
        assert_eq!(fixture.key.read().unwrap(), None);
        fixture.set("AppsUseLightTheme", 0);
        fixture.set("SystemUsesLightTheme", 1);
        assert_eq!(fixture.key.read().unwrap(), Some(Theme::Light));
        fixture.set("AppsUseLightTheme", 1);
        fixture.set("SystemUsesLightTheme", 0);
        assert_eq!(fixture.key.read().unwrap(), Some(Theme::Dark));
        fixture.set("SystemUsesLightTheme", 9);
        assert_eq!(fixture.key.read().unwrap(), None);
    }

    #[test]
    fn observes_repeated_theme_changes_and_shuts_down_without_changing_user_settings() {
        let fixture = TestKey::new();
        fixture.set("SystemUsesLightTheme", 1);
        let (send, receive) = mpsc::channel();
        let watcher = ThemeWatcher::start_at(&fixture.path, move |theme| {
            send.send(theme).unwrap();
        })
        .unwrap();
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(5)).unwrap(),
            Some(Theme::Light)
        );
        fixture.set("SystemUsesLightTheme", 0);
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(5)).unwrap(),
            Some(Theme::Dark)
        );
        fixture.set("SystemUsesLightTheme", 1);
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(5)).unwrap(),
            Some(Theme::Light)
        );
        watcher.stop();
        assert!(matches!(
            receive.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
