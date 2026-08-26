use super::*;

fn request(key: &str, payload: &str) -> HookIpcEnqueueRequest {
    HookIpcEnqueueRequest {
        kind: "hook_user_prompt".to_string(),
        payload: payload.to_string(),
        idempotency_key: key.to_string(),
    }
}

#[test]
fn parent_sync_fault_is_scoped_to_the_armed_spool() {
    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    let spool_a = IngressSpool::new(dir_a.path());
    let spool_b = IngressSpool::new(dir_b.path());
    let _fault = fail_next_parent_namespace_sync_for(&spool_a);

    std::thread::scope(|scope| {
        let thread_a = scope.spawn(|| spool_a.append(&request("a-key", "payload-a")));
        let thread_b = scope.spawn(|| spool_b.append(&request("b-key", "payload-b")));
        let a_result = thread_a.join().expect("spool a thread");
        let b_result = thread_b.join().expect("spool b thread");
        assert!(
            b_result.is_ok(),
            "unarmed spool must not consume A's parent-sync fault: {b_result:?}"
        );
        assert!(
            matches!(a_result, Err(IngressSpoolError::Uncertain(_))),
            "armed spool must fail parent sync: {a_result:?}"
        );
    });
}
