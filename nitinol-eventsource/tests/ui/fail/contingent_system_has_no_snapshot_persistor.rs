// A contingent system does not know whether the deployment holds a snapshot
// slot, so it has no persistor to hand out: the slot it may carry is reached
// through `snapshot_store()`, whose `Option` says what the type cannot.

use nitinol_eventsource::system::ContingentSystem;

fn persistor_of(system: &ContingentSystem) {
    let _ = system.snapshot_persistor();
}

fn main() {}
