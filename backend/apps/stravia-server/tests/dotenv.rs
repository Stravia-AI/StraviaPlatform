use std::{fs, process::Command};

#[test]
fn loads_dotenv_from_working_directory_before_parsing_arguments() {
    let working_directory = tempfile::tempdir().expect("create temporary working directory");
    fs::write(
        working_directory.path().join(".env"),
        "STRAVIA_STORAGE_BACKEND=invalid-from-dotenv\n",
    )
    .expect("write .env");

    let output = Command::new(env!("CARGO_BIN_EXE_stravia-server"))
        .current_dir(working_directory.path())
        .env_remove("STRAVIA_STORAGE_BACKEND")
        .output()
        .expect("run stravia-server");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid value 'invalid-from-dotenv'"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
