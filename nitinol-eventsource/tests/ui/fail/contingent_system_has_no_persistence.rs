// A contingent system names no store type, so it has no persistence supplier
// to hand out: what it carries is reached through `event_store()` and
// `snapshot_store()`.

use nitinol_eventsource::system::ContingentSystem;

fn persistence_of(system: &ContingentSystem) {
    let _ = system.persistence();
}

fn main() {}
