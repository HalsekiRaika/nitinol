use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::projector::EventProjector;


static mut GLOBAL_PROJECTOR: GlobalEventProjector = GlobalEventProjector(None);

static EXISTS: AtomicBool = AtomicBool::new(false);
static GLOBAL_INIT: AtomicUsize = AtomicUsize::new(UNINITIALIZED);

const UNINITIALIZED: usize = 0;
const INITIALIZING: usize = 1;
const INITIALIZED: usize = 2;

pub fn set_global_projector(install: EventProjector) {
    if GLOBAL_INIT
        .compare_exchange(
            UNINITIALIZED,
            INITIALIZING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
    {
        unsafe {
            GLOBAL_PROJECTOR = GlobalEventProjector(Some(install));
        }
        GLOBAL_INIT.store(INITIALIZED, Ordering::SeqCst);
        EXISTS.store(true, Ordering::Release);
    } else {
        panic!("`EventProjector` already initialized");
    }
}

pub fn get_global_projector() -> &'static EventProjector {
    if EXISTS.load(Ordering::Acquire) {
        unsafe {
            &*std::ptr::addr_of!(GLOBAL_PROJECTOR)
        }
    } else {
        panic!("`EventProjector` not initialized");
    }
}


pub(crate) struct GlobalEventProjector(Option<EventProjector>);

impl Deref for GlobalEventProjector {
    type Target = EventProjector;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("`EventProjector` not initialized")
    }
}
