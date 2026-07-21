use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CrashPoint {
    LeaseCreatedBeforeStatePublish,
    ReleaseStateSavedBeforeLeaseUnlink,
    ReapStateSavedBeforeOrphanSweep,
}

impl CrashPoint {
    pub(super) const fn exit_code(self) -> i32 {
        match self {
            Self::LeaseCreatedBeforeStatePublish => 86,
            Self::ReleaseStateSavedBeforeLeaseUnlink => 87,
            Self::ReapStateSavedBeforeOrphanSweep => 88,
        }
    }
}

thread_local! {
    static ARMED_CRASH_POINT: Cell<Option<CrashPoint>> = const { Cell::new(None) };
}

pub(super) struct CrashPointGuard {
    previous: Option<CrashPoint>,
    _not_send: PhantomData<Rc<()>>,
}

pub(super) fn arm(point: CrashPoint) -> CrashPointGuard {
    let previous = ARMED_CRASH_POINT.with(|armed| armed.replace(Some(point)));
    assert!(previous.is_none(), "admission crash point already armed");
    CrashPointGuard {
        previous,
        _not_send: PhantomData,
    }
}

pub(super) fn exit_if(point: CrashPoint) {
    let armed = ARMED_CRASH_POINT.with(Cell::get);
    if armed == Some(point) {
        ARMED_CRASH_POINT.with(|slot| slot.set(None));
        // SAFETY: This exec'd test fixture deliberately skips destructors to
        // reproduce a process crash at the exact persistence chokepoint.
        unsafe { libc::_exit(point.exit_code()) }
    }
}

impl Drop for CrashPointGuard {
    fn drop(&mut self) {
        ARMED_CRASH_POINT.with(|armed| armed.set(self.previous.take()));
    }
}
