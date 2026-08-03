use nitinol_persistence::store::DeliveryMode;
use nitinol_persistence::ProjectionId;

pub struct ProjectionContext<'a, Tx> {
    projection_id: &'a ProjectionId,
    current_sequence: u64,
    delivery_mode: DeliveryMode,
    tx: Option<Tx>,
}

impl<'a, Tx> ProjectionContext<'a, Tx> {
    pub(crate) fn new(
        projection_id: &'a ProjectionId,
        current_sequence: u64,
        delivery_mode: DeliveryMode,
        tx: Option<Tx>,
    ) -> Self {
        Self {
            projection_id,
            current_sequence,
            delivery_mode,
            tx,
        }
    }

    /// The identifier of this projection, as configured in `ProjectorProps`.
    pub fn projection_id(&self) -> &ProjectionId {
        self.projection_id
    }

    /// The sequence number of the event currently being projected.
    ///
    /// For aggregate-scoped catch-up this is the per-aggregate sequence.
    /// For global catch-up this is the global sequence.
    pub fn current_sequence(&self) -> u64 {
        self.current_sequence
    }

    /// The delivery mode configured for this projection.
    pub fn delivery_mode(&self) -> DeliveryMode {
        self.delivery_mode
    }

    pub fn tx(&mut self) -> Option<&mut Tx> {
        self.tx.as_mut()
    }

    pub(crate) fn take_tx(&mut self) -> Option<Tx> {
        self.tx.take()
    }
}
