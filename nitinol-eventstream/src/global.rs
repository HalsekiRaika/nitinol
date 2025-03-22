use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::eventstream::EventStream;

static mut GLOBAL_EVENTSTREAM: GlobalEventStream = GlobalEventStream(None);

static EXISTS: AtomicBool = AtomicBool::new(false);
static GLOBAL_INIT: AtomicUsize = AtomicUsize::new(UNINITIALIZED);

const UNINITIALIZED: usize = 0;
const INITIALIZING: usize = 1;
const INITIALIZED: usize = 2;

pub fn init_eventstream(install: EventStream) {
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
            GLOBAL_EVENTSTREAM = GlobalEventStream(Some(install));
        }
        GLOBAL_INIT.store(INITIALIZED, Ordering::SeqCst);
        EXISTS.store(true, Ordering::Release);
    } else {
        panic!("`EventStream` already initialized");
    }
}

pub fn get_event_stream() -> &'static EventStream {
    if EXISTS.load(Ordering::Acquire) {
        unsafe {
            &*std::ptr::addr_of!(GLOBAL_EVENTSTREAM)
        }
    } else {
        panic!("`EventStream` not initialized");
    }
}


pub(crate) struct GlobalEventStream(Option<EventStream>);

impl Deref for GlobalEventStream {
    type Target = EventStream;
    
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("`EventStream` not initialized")
    }
}
