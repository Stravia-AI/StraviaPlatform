//! Read-only lock ownership evidence for strict Windows fixture cleanup failures.

use std::path::PathBuf;

#[derive(Debug)]
pub(super) struct LockOwner {
    pub pid: u32,
    pub kind: &'static str,
    pub application_type: u32,
}

#[derive(Debug)]
pub(super) struct LockOwnerReport {
    pub owners: Vec<LockOwner>,
    pub query_error: Option<(&'static str, u32)>,
    pub end_session_error: Option<u32>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FileTime {
    low: u32,
    high: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UniqueProcess {
    pid: u32,
    started: FileTime,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessInfo {
    process: UniqueProcess,
    application_name: [u16; 256],
    service_name: [u16; 64],
    application_type: u32,
    application_status: u32,
    terminal_session: u32,
    restartable: i32,
}

#[link(name = "Rstrtmgr")]
unsafe extern "system" {
    fn RmStartSession(session: *mut u32, flags: u32, key: *mut u16) -> u32;
    fn RmRegisterResources(
        session: u32,
        file_count: u32,
        files: *const *const u16,
        process_count: u32,
        processes: *const UniqueProcess,
        service_count: u32,
        services: *const *const u16,
    ) -> u32;
    fn RmGetList(
        session: u32,
        needed: *mut u32,
        count: *mut u32,
        processes: *mut ProcessInfo,
        reboot_reasons: *mut u32,
    ) -> u32;
    fn RmEndSession(session: u32) -> u32;
}

pub(super) fn lock_owners(files: &[PathBuf]) -> LockOwnerReport {
    use std::os::windows::ffi::OsStrExt;

    let mut report = LockOwnerReport {
        owners: Vec::new(),
        query_error: None,
        end_session_error: None,
    };
    if files.is_empty() {
        return report;
    }
    let Ok(file_count) = u32::try_from(files.len()) else {
        report.query_error = Some(("file_count", 87));
        return report;
    };
    let filenames: Vec<Vec<u16>> = files
        .iter()
        .map(|path| path.as_os_str().encode_wide().chain(Some(0)).collect())
        .collect();
    let pointers: Vec<*const u16> = filenames.iter().map(|path| path.as_ptr()).collect();
    let mut session = 0;
    let mut key = [0u16; 33];
    // All pointers below refer to live, correctly sized buffers. Only the read-only
    // session/register/list/end APIs are linked; no process control API is used.
    let status = unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) };
    if status != 0 {
        report.query_error = Some(("start_session", status));
        return report;
    }
    let query = || -> Result<Vec<LockOwner>, (&'static str, u32)> {
        let status = unsafe {
            RmRegisterResources(
                session,
                file_count,
                pointers.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            )
        };
        if status != 0 {
            return Err(("register_files", status));
        }
        let mut needed = 0;
        let mut count = 0;
        let mut reboot_reasons = 0;
        let status = unsafe {
            RmGetList(
                session,
                &mut needed,
                &mut count,
                std::ptr::null_mut(),
                &mut reboot_reasons,
            )
        };
        if status == 0 {
            return Ok(Vec::new());
        }
        const ERROR_MORE_DATA: u32 = 234;
        if status != ERROR_MORE_DATA {
            return Err(("list_size", status));
        }
        // The size query and data query are the API's two-call enumeration
        // protocol, not a cleanup retry. Changing owners are reported as an error.
        let empty = ProcessInfo {
            process: UniqueProcess {
                pid: 0,
                started: FileTime { low: 0, high: 0 },
            },
            application_name: [0; 256],
            service_name: [0; 64],
            application_type: 0,
            application_status: 0,
            terminal_session: 0,
            restartable: 0,
        };
        let mut processes = vec![empty; needed as usize];
        count = needed;
        let status = unsafe {
            RmGetList(
                session,
                &mut needed,
                &mut count,
                processes.as_mut_ptr(),
                &mut reboot_reasons,
            )
        };
        if status != 0 {
            return Err(("list_owners", status));
        }
        processes.truncate(count as usize);
        Ok(processes
            .into_iter()
            .map(|process| LockOwner {
                pid: process.process.pid,
                kind: if process.process.pid == std::process::id() {
                    "current_test"
                } else {
                    "external"
                },
                application_type: process.application_type,
            })
            .collect())
    };
    match query() {
        Ok(owners) => report.owners = owners,
        Err(error) => report.query_error = Some(error),
    }
    let status = unsafe { RmEndSession(session) };
    if status != 0 {
        report.end_session_error = Some(status);
    }
    report
}

#[tokio::test]
async fn restart_manager_identifies_current_sqlite_file_owner() {
    let directory = tempfile::tempdir().expect("temporary SQLite directory");
    let pool = crate::db::init_pool(directory.path())
        .await
        .expect("SQLite pool");
    let mut connection = pool.begin_write().await.expect("SQLite write transaction");
    sqlx::query("CREATE TABLE lock_owner_probe (id INTEGER)")
        .execute(&mut *connection)
        .await
        .expect("open SQLite database and WAL");
    connection
        .commit()
        .await
        .expect("commit SQLite write transaction");
    let root = directory.path().to_path_buf();
    let path = root.join("db").join("gateway.db");
    // Exercise the same contextual io::Error returned by strict gateway cleanup,
    // not a synthetic OS error. The live SQLite pool intentionally holds the file
    // until its explicit fixture teardown below.
    directory
        .close()
        .expect_err("live SQLite database prevents temporary directory removal");
    let report = lock_owners(&[path]);
    let owns_database = report
        .owners
        .iter()
        .any(|owner| owner.pid == std::process::id() && owner.kind == "current_test");
    pool.close().await;
    drop(pool);
    std::fs::remove_dir_all(&root).expect("remove closed SQLite database directory");
    assert!(
        report.query_error.is_none(),
        "owner query failed: {report:?}"
    );
    assert!(
        report.end_session_error.is_none(),
        "owner session cleanup failed: {report:?}"
    );
    assert!(owns_database, "current SQLite owner missing: {report:?}");
}
