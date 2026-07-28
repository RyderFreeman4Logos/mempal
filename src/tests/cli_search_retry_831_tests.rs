use super::*;

#[tokio::test]
async fn search_command_deadline_covers_embedder_initialization() {
    let (_tmp, db) = new_temp_db();
    let config = Config::default();
    let initialization_started = Arc::new(AtomicUsize::new(0));
    let initialization_started_for_factory = Arc::clone(&initialization_started);
    let started_at = std::time::Instant::now();

    let error = search_command_with_embedder_initializer(
        &db,
        &config,
        SearchCommandOptions {
            query: "deadline-bound embedder initialization",
            wing: None,
            room: None,
            session: None,
            filters: SearchFilters::default(),
            top_k: 0,
            project: None,
            include_global: false,
            all_projects: true,
            json: true,
            with_neighbors: false,
            include_raw_turns: false,
            include_expired: false,
        },
        std::time::Duration::from_millis(20),
        move || async move {
            initialization_started_for_factory.store(1, Ordering::SeqCst);
            tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(250));
            })
            .await
            .expect("blocking embedder initialization task should not panic");
            Ok::<Box<dyn Embedder>, anyhow::Error>(Box::new(RecordingEmbedder::default()))
        },
    )
    .await
    .expect_err("the command-level deadline must cancel embedder initialization");

    assert_eq!(initialization_started.load(Ordering::SeqCst), 1);
    assert!(format!("{error:#}").contains("CLI search total deadline exceeded"));
    assert!(
        started_at.elapsed() < std::time::Duration::from_secs(1),
        "the search command should not wait for the slow embedder initializer"
    );
}

#[test]
fn block_on_result_bounds_runtime_shutdown_after_search_deadline() {
    let (_tmp, db) = new_temp_db();
    let config = Config::default();
    let (initialization_started_tx, initialization_started_rx) = std::sync::mpsc::sync_channel(1);
    let (initialization_finished_tx, initialization_finished_rx) = std::sync::mpsc::sync_channel(1);
    let started_at = std::time::Instant::now();

    let error = block_on_result(search_command_with_embedder_initializer(
        &db,
        &config,
        SearchCommandOptions {
            query: "deadline-bound runtime shutdown",
            wing: None,
            room: None,
            session: None,
            filters: SearchFilters::default(),
            top_k: 0,
            project: None,
            include_global: false,
            all_projects: true,
            json: true,
            with_neighbors: false,
            include_raw_turns: false,
            include_expired: false,
        },
        std::time::Duration::from_millis(20),
        move || async move {
            let initialization = tokio::task::spawn_blocking(move || {
                initialization_started_tx
                    .send(())
                    .expect("report that blocking initialization started");
                std::thread::sleep(std::time::Duration::from_secs(3));
                initialization_finished_tx
                    .send(())
                    .expect("report that blocking initialization finished");
            });
            initialization_started_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("blocking initialization should start before its deadline");
            initialization
                .await
                .expect("blocking embedder initialization task should not panic");
            Ok::<Box<dyn Embedder>, anyhow::Error>(Box::new(RecordingEmbedder::default()))
        },
    ))
    .expect_err("the command-level deadline must fail");

    assert!(format!("{error:#}").contains("CLI search total deadline exceeded"));
    assert!(
        started_at.elapsed() < std::time::Duration::from_millis(1500),
        "runtime teardown must not wait for a blocking initializer after the search deadline"
    );
    initialization_finished_rx
        .recv_timeout(std::time::Duration::from_secs(4))
        .expect("the detached blocking initializer should finish independently");
}

fn run_cli_content_write_while_sqlite_lock_is_held<T>(
    db: &Database,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    db.conn()
        .busy_timeout(std::time::Duration::ZERO)
        .expect("make CLI content write fail fast while the blocker holds SQLite's write lock");
    let blocker = rusqlite::Connection::open(db.path()).expect("open lock holder connection");
    blocker
        .busy_timeout(std::time::Duration::ZERO)
        .expect("make lock holder fail fast");
    blocker
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold SQLite write lock");
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        blocker
            .execute_batch("COMMIT;")
            .expect("release SQLite write lock");
    });

    let result = operation();
    release.join().expect("join SQLite lock release");
    result
}

#[test]
fn cli_content_writes_retry_for_a_three_second_sqlite_lock() {
    let (_tmp, db) = new_temp_db();
    insert_cli_test_drawer(&db, "cli-write-retry-pin");
    insert_cli_test_drawer(&db, "cli-write-retry-delete");

    run_cli_content_write_while_sqlite_lock_is_held(&db, || {
        pin_command(&db, "cli-write-retry-pin")
    })
    .expect("pin must retry a three-second transient SQLite lock");
    assert!(
        db.get_pinned_facts(None, 10)
            .expect("load pinned facts")
            .iter()
            .any(|drawer| drawer.id == "cli-write-retry-pin")
    );

    run_cli_content_write_while_sqlite_lock_is_held(&db, || {
        unpin_command(&db, "cli-write-retry-pin")
    })
    .expect("unpin must retry a three-second transient SQLite lock");
    assert!(
        !db.get_pinned_facts(None, 10)
            .expect("load pinned facts")
            .iter()
            .any(|drawer| drawer.id == "cli-write-retry-pin")
    );

    run_cli_content_write_while_sqlite_lock_is_held(&db, || {
        delete_command(
            &db,
            &Config::default(),
            DeleteCommandOptions {
                drawer_id: "cli-write-retry-delete",
                project: None,
                include_global: false,
                all_projects: true,
            },
        )
    })
    .expect("delete must retry a three-second transient SQLite lock");
    assert!(drawer_deleted_at(&db, "cli-write-retry-delete").is_some());
}
