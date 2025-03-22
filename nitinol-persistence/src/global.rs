use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::writer::EventWriter;

static mut GLOBAL_WRITER: GlobalEventWriter = GlobalEventWriter(None);

static EXISTS: AtomicBool = AtomicBool::new(false);
static GLOBAL_INIT: AtomicUsize = AtomicUsize::new(UNINITIALIZED);

const UNINITIALIZED: usize = 0;
const INITIALIZING: usize = 1;
const INITIALIZED: usize = 2;

pub fn set_writer(install: EventWriter) {
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
            GLOBAL_WRITER = GlobalEventWriter(Some(install));
        }
        GLOBAL_INIT.store(INITIALIZED, Ordering::SeqCst);
        EXISTS.store(true, Ordering::Release);
    } else {
        panic!("`EventWriter` already initialized");
    }
}

pub fn get_global_writer() -> &'static EventWriter {
    if EXISTS.load(Ordering::Acquire) {
        unsafe {
            &*std::ptr::addr_of!(GLOBAL_WRITER)
        }
    } else {
        panic!("`EventWriter` not initialized");
    }
}


pub(crate) struct GlobalEventWriter(Option<EventWriter>);

impl Deref for GlobalEventWriter {
    type Target = EventWriter;
    
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("`EventWriter` not initialized")
    }
}
