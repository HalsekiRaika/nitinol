//! `#[derive(SagaSubscribe)]` on an alphabet of two external sources and one
//! deadline must expand: the enum stays an ordinary enum its author can match
//! on, and the binding typestate the spawn builder steps through comes from the
//! declaration rather than from the wiring.

use bytes::Bytes;
use nitinol::contract::Codec;
use nitinol::eventsource::Event;
use nitinol::saga::{DeadlineMessage, SagaSubscribe};

#[derive(Clone, Event)]
#[event(family = "shop.subscribe", codec = OrderPlacedCodec)]
struct OrderPlaced;

struct OrderPlacedCodec;

impl Codec<OrderPlaced> for OrderPlacedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &OrderPlaced) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<OrderPlaced, Self::Error> {
        Ok(OrderPlaced)
    }
}

#[derive(Clone, Event)]
#[event(family = "shop.subscribe", codec = StockReservedCodec)]
struct StockReserved;

struct StockReservedCodec;

impl Codec<StockReserved> for StockReservedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &StockReserved) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<StockReserved, Self::Error> {
        Ok(StockReserved)
    }
}

/// A deadline message is not an event: it is never a row of the saga's own
/// stream, only a value persisted inside a schedule fact.  So it names a codec
/// without naming an event type.
#[derive(Clone)]
struct Reminder;

struct ReminderCodec;

impl Codec<Reminder> for ReminderCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Reminder, Self::Error> {
        Ok(Reminder)
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "reminder";
    const CODEC_NAME: &'static str = "reminder-codec";
    type Codec = ReminderCodec;
}

#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
    Stock(StockReserved),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

fn main() {
    // The binding typestate the builder starts from: every declared external
    // source unbound, and nothing allotted to the deadline variant.
    let _bindings: <Observed as SagaSubscribe>::Bindings = Default::default();

    // The enum is still the author's to construct and match on — the derive adds
    // to it rather than sealing it.
    let inputs = [
        Observed::Order(OrderPlaced),
        Observed::Stock(StockReserved),
        Observed::Reminder(Reminder),
    ];
    for input in &inputs {
        match input {
            Observed::Order(_) | Observed::Stock(_) | Observed::Reminder(_) => {}
        }
    }
}
