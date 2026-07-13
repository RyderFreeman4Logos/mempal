use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};

use mempal::core::db_admission::{
    DbAdmissionConfig, DbAdmissionError, DbAdmissionRequest, DbHolderClass, ProfileDbAdmission,
};

const MIB: u64 = 1024 * 1024;

fn request(class: DbHolderClass, cache_mib: u64) -> DbAdmissionRequest {
    DbAdmissionRequest::new(class, 1, cache_mib * MIB)
}

#[test]
fn exact_profile_budget_is_admitted_and_excess_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config = DbAdmissionConfig::new(2, 64 * MIB);

    let first = ProfileDbAdmission::acquire_with_config(
        &db_path,
        request(DbHolderClass::Daemon, 32),
        config,
    )
    .expect("first holder");
    let second =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Mcp, 32), config)
            .expect("exact holder/cache budget");

    let error =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Cli, 1), config)
            .expect_err("holder over budget must fail");
    assert!(matches!(
        error,
        DbAdmissionError::BudgetExceeded {
            active_holders: 2,
            requested_cache_bytes: MIB,
            ..
        }
    ));

    let snapshot =
        ProfileDbAdmission::snapshot_with_config(&db_path, config).expect("admission snapshot");
    assert_eq!(snapshot.active_holders, 2);
    assert_eq!(snapshot.configured_cache_bytes, 64 * MIB);
    assert_eq!(snapshot.holders[0].generation, first.generation());
    assert_eq!(snapshot.holders[1].generation, second.generation());
}

#[test]
fn dropping_holder_returns_profile_capacity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config = DbAdmissionConfig::new(1, 16 * MIB);

    let first =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Api, 16), config)
            .expect("first holder");
    let first_generation = first.generation();
    drop(first);

    let replacement =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Hook, 16), config)
            .expect("capacity after release");
    assert!(replacement.generation() > first_generation);
}

#[test]
fn concurrent_registration_never_oversubscribes_profile_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = Arc::new(tmp.path().join("palace.db"));
    let start = Arc::new(Barrier::new(5));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let config = DbAdmissionConfig::new(2, 32 * MIB);
    let mut workers = Vec::new();

    for _ in 0..4 {
        let db_path = Arc::clone(&db_path);
        let start = Arc::clone(&start);
        let release = Arc::clone(&release);
        let admitted_tx = admitted_tx.clone();
        workers.push(std::thread::spawn(move || {
            start.wait();
            let admission = ProfileDbAdmission::acquire_with_config(
                &db_path,
                request(DbHolderClass::Cli, 16),
                config,
            );
            admitted_tx
                .send(admission.is_ok())
                .expect("report admission");
            if admission.is_ok() {
                let (released, signal) = &*release;
                let mut released = released.lock().expect("release lock");
                while !*released {
                    released = signal.wait(released).expect("release signal");
                }
            }
            admission.is_ok()
        }));
    }
    drop(admitted_tx);

    start.wait();
    let reported = (0..4)
        .map(|_| {
            admitted_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("worker admission result")
        })
        .filter(|admitted| *admitted)
        .count();
    let (released, signal) = &*release;
    *released.lock().expect("release lock") = true;
    signal.notify_all();
    let admitted = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker"))
        .filter(|admitted| *admitted)
        .count();
    assert_eq!(reported, 2);
    assert_eq!(admitted, 2);
}
