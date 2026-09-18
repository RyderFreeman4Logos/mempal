use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};

static PENDING: AtomicUsize = AtomicUsize::new(0);

pub(super) struct PendingForkWriter;

impl PendingForkWriter {
    pub(super) fn register() -> io::Result<Self> {
        PENDING
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |pending| {
                pending.checked_add(1)
            })
            .map(|_| Self)
            .map_err(|_| io::Error::other("test process fork writer counter overflow"))
    }
}

impl Drop for PendingForkWriter {
    fn drop(&mut self) {
        let previous = PENDING.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "fork writer counter underflow");
    }
}

pub(super) fn pending() -> bool {
    PENDING.load(Ordering::SeqCst) != 0
}
