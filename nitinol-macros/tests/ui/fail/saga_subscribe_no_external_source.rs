//! An alphabet of nothing but a deadline declares no source to observe. Its
//! binding typestate would already be complete at `new`, so "spawning with no
//! subscription is a compile error" would stop holding — the rule generalises
//! to several sources rather than being retired by them.

use nitinol::saga::SagaSubscribe;

struct Reminder;

#[derive(SagaSubscribe)]
enum Observed {
    #[subscribe(deadline)]
    Reminder(Reminder),
}

fn main() {}
