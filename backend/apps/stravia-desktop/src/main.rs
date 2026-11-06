#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod runtime_smoke;

fn main() -> std::process::ExitCode {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--smoke-test-runtime")) {
        return match runtime_smoke::run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Desktop runtime smoke failed: {error:#}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    stravia_desktop_lib::run();
    std::process::ExitCode::SUCCESS
}
