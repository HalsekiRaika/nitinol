use bytes::{BufMut, Bytes, BytesMut};
use nitinol_contract::Event;
use nitinol_persistence::{EventType, Family, TypeName, Variant};

const FAMILY: Family = Family::new("conformance.ledger");
const TYPE_NAME: TypeName = TypeName::new("fact");

/// What happened to the ledger.
///
/// `Credited` and `Debited` do not commute, which is what makes the order of an
/// acceptance's facts observable: a stream that holds them the other way round
/// replays into a balance the decider never described.
#[derive(Clone, Debug, PartialEq)]
pub enum LedgerEvent {
    Opened { holder: String },
    Credited { amount: u64 },
    Debited { amount: u64 },
}

impl Event for LedgerEvent {
    const EVENT_TYPE: EventType = EventType::new(FAMILY, TYPE_NAME);

    fn variant(&self) -> EventType {
        let arm = match self {
            Self::Opened { .. } => Variant::new("Opened"),
            Self::Credited { .. } => Variant::new("Credited"),
            Self::Debited { .. } => Variant::new("Debited"),
        };
        EventType::with_variant(FAMILY, TYPE_NAME, arm)
    }
}

/// The leading byte each kind of fact is written under.
const OPENED: u8 = 1;
const CREDITED: u8 = 2;
const DEBITED: u8 = 3;

/// How many bytes an amount is carried in.
const AMOUNT_BYTES: usize = 8;

impl LedgerEvent {
    /// This fact, as the bytes an interpreter is expected to hand its store.
    ///
    /// The suite reads a stream back and decodes it itself rather than asking
    /// the interpreter what it wrote, so the facts have to travel in a format
    /// the suite owns.  An interpreter wires this to whatever its own store
    /// boundary demands; it never has to agree with the suite about anything
    /// but these bytes.
    ///
    /// Encoding cannot fail: every value of this type has a representation
    /// here, so an interpreter that fails to record one is reporting its own
    /// machinery rather than the format.
    pub fn encode(&self) -> Bytes {
        let mut payload = BytesMut::new();
        match self {
            Self::Opened { holder } => {
                payload.put_u8(OPENED);
                payload.put_slice(holder.as_bytes());
            }
            Self::Credited { amount } => {
                payload.put_u8(CREDITED);
                payload.put_u64(*amount);
            }
            Self::Debited { amount } => {
                payload.put_u8(DEBITED);
                payload.put_u64(*amount);
            }
        }
        payload.freeze()
    }

    /// The fact `payload` carries, or why it carries none.
    pub fn decode(payload: &[u8]) -> Result<Self, MalformedLedgerEvent> {
        let (kind, body) = payload.split_first().ok_or(Malformation::Untagged)?;
        match *kind {
            OPENED => {
                let holder = std::str::from_utf8(body).map_err(Malformation::Holder)?;
                Ok(Self::Opened {
                    holder: holder.to_owned(),
                })
            }
            CREDITED => Ok(Self::Credited {
                amount: amount(body)?,
            }),
            DEBITED => Ok(Self::Debited {
                amount: amount(body)?,
            }),
            unknown => Err(Malformation::UnknownKind(unknown).into()),
        }
    }
}

fn amount(body: &[u8]) -> Result<u64, MalformedLedgerEvent> {
    let carried: [u8; AMOUNT_BYTES] = body
        .try_into()
        .map_err(|_| Malformation::Amount(body.len()))?;
    Ok(u64::from_be_bytes(carried))
}

/// A payload that is not a fact in the suite's own format.
#[derive(Debug, thiserror::Error)]
#[error("a payload is not a ledger fact: {0}")]
pub struct MalformedLedgerEvent(#[from] Malformation);

/// Why a payload could not be read back.
///
/// Kept out of the public surface: what a consumer needs is that the payload
/// was not one of ours, and the reason survives in the message and the source
/// chain.
#[derive(Debug, thiserror::Error)]
enum Malformation {
    #[error("it carries no leading byte to name which fact it is")]
    Untagged,
    #[error("its leading byte {0:#04x} names no fact this suite writes")]
    UnknownKind(u8),
    #[error("an amount is carried in {AMOUNT_BYTES} bytes, and it carries {0}")]
    Amount(usize),
    #[error("a holder name is UTF-8: {0}")]
    Holder(#[source] std::str::Utf8Error),
}
