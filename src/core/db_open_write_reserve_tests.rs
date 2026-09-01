#[test]
fn db_open_applies_busy_timeout_before_schema_queries() {
    let tempdir = short_tempdir();
    let db_path = tempdir.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize database"));

    let blocker = Connection::open(&db_path).expect("open lock holder");
    blocker
        .pragma_update(None, "journal_mode", "DELETE")
        .expect("select delete journal mode");
    blocker
        .execute_batch("BEGIN EXCLUSIVE;")
        .expect("hold exclusive schema lock");

    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let open_path = db_path.clone();
    let opener = std::thread::spawn(move || {
        let _ = opened_tx.send(Database::open_with_busy_timeout(
            &open_path,
            Duration::from_millis(25),
        ));
    });

    let opened = opened_rx.recv_timeout(Duration::from_millis(250));
    blocker
        .execute_batch("COMMIT;")
        .expect("release schema lock");
    opener.join().expect("join database opener");
    assert!(
        opened
            .expect("caller-selected timeout must bound schema query")
            .is_err(),
        "exclusive schema lock must outlast the caller-selected busy timeout"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn db_open_consumes_write_reserve_for_full_old_schema_bootstrap() {
    const IN_NAMESPACE: &str = "MEMPAL_TEST_WRITE_RESERVE_NAMESPACE";
    const TEST_NAME: &str = "db_open_consumes_write_reserve_for_full_old_schema_bootstrap";
    if std::env::var_os(IN_NAMESPACE).is_none() {
        let status = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "--fork", "--"])
            .arg(std::env::current_exe().expect("current test executable"))
            .arg(TEST_NAME)
            .arg("--nocapture")
            .env(IN_NAMESPACE, "1")
            .status()
            .expect("start isolated filesystem test");
        assert!(
            status.success(),
            "bounded filesystem child failed: {status}"
        );
        return;
    }

    let tempdir = short_tempdir();
    let mount_options = format!("size={},mode=0700", WRITE_RESERVE_BYTES + 16 * 1024 * 1024);
    let status = std::process::Command::new("mount")
        .args(["-t", "tmpfs", "-o", &mount_options, "tmpfs"])
        .arg(tempdir.path())
        .status()
        .expect("mount bounded filesystem");
    assert!(status.success(), "mount bounded filesystem: {status}");

    let db_path = tempdir.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize database and reserve"));
    let old_schema = Connection::open(&db_path).expect("open old-schema fixture");
    old_schema
        .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION - 1)
        .expect("downgrade schema fixture");
    drop(old_schema);
    let reserve_path = write_reserve_path(&db_path);
    assert_eq!(
        fs::metadata(&reserve_path)
            .expect("reserve allocation succeeds")
            .len(),
        WRITE_RESERVE_BYTES
    );

    let filler = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(tempdir.path().join("fill"))
        .expect("create bounded-filesystem filler");
    crate::core::db::db_write_reserve::fail_next_write_reserve_retry_with_sqlite_full(&db_path);
    let free_bytes = filesystem_free_bytes(tempdir.path());
    // SAFETY: `filler` owns the valid descriptor and the requested range is
    // bounded by the filesystem's reported free space.
    let result = unsafe {
        use std::os::fd::AsRawFd;

        libc::posix_fallocate(
            filler.as_raw_fd(),
            0,
            free_bytes.saturating_sub(4096) as libc::off_t,
        )
    };
    assert_eq!(result, 0, "fill bounded filesystem: {result}");

    let database =
        Database::open(&db_path).expect("old-schema bootstrap must retry after consuming the reserve");
    assert!(
        !reserve_path.exists(),
        "SQLITE_FULL bootstrap must consume the reserve once"
    );
    assert_eq!(
        database
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .expect("read migrated schema version"),
        CURRENT_SCHEMA_VERSION
    );
    let spool = crate::ingress_spool::IngressSpool::new(tempdir.path());
    assert_eq!(
        spool
            .append(&crate::hook_ipc::HookIpcEnqueueRequest::new(
                "hook", "durable"
            ))
            .expect("hook admission remains durable after bootstrap recovery"),
        AppendOutcome::Appended
    );
    drop((database, filler));
}

#[cfg(target_os = "linux")]
fn filesystem_free_bytes(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;

    let path =
        std::ffi::CString::new(path.as_os_str().as_bytes()).expect("filesystem path has no NUL");
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` is valid writable storage.
    assert_eq!(
        unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) },
        0
    );
    // SAFETY: a zero `statvfs` result initializes `stat`.
    let stat = unsafe { stat.assume_init() };
    stat.f_bavail.saturating_mul(stat.f_frsize)
}

#[test]
fn current_schema_db_open_does_not_reapply_migrations_under_live_writer() {
    let tempdir = short_tempdir();
    let db_path = tempdir.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize current database"));

    let blocker = Connection::open(&db_path).expect("open same-version writer");
    blocker
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold same-version write transaction");
    let opened = Database::open_with_busy_timeout(&db_path, Duration::from_millis(25));
    blocker
        .execute_batch("ROLLBACK;")
        .expect("release same-version writer");

    opened.expect("current schema open must not request a SQLite write lock");
}
