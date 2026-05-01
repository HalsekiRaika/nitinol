use futures_core::future::BoxFuture;

use crate::effect::core::Effect;
use crate::error::EffectExecutionError;

/// Execute an `Effect` and return the accumulated events.
///
/// Each variant is handled as follows:
///
/// | Variant           | Behaviour                                           |
/// |-------------------|-----------------------------------------------------|
/// | `None`            | Returns an empty event list immediately.            |
/// | `Persist(events)` | Returns events (persistence handled by Phase 2.4).  |
/// | `Apply(events)`   | Returns events (aggregate apply by Phase 2.4).      |
/// | `Side(s)`         | Awaits `s.execute()`; contributes no events.        |
/// | `Sequence(es)`    | Executes each element in order; concatenates events.|
///
/// In Phase 2.4 (AggregateProcess), this function will be extended to accept
/// an `Aggregate`, `EventStore`, and codec so that `Persist` and `Apply`
/// interact with persistent storage and update aggregate state in place.
pub fn execute_effect<E: Send + 'static>(
    effect: Effect<E>,
) -> BoxFuture<'static, Result<Vec<E>, EffectExecutionError>> {
    Box::pin(async move {
        match effect {
            Effect::None => Ok(vec![]),
            Effect::Persist(events) | Effect::Apply(events) => Ok(events),
            Effect::Side(side) => {
                side.execute().await?;
                Ok(vec![])
            }
            Effect::Sequence(effects) => {
                let mut all = Vec::new();
                for e in effects {
                    let mut events = execute_effect(e).await?;
                    all.append(&mut events);
                }
                Ok(all)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::core::Effect;

    #[tokio::test]
    async fn execute_none_returns_empty_vec() {
        let result = execute_effect::<u32>(Effect::None).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn execute_persist_returns_accumulated_events() {
        let result = execute_effect(Effect::persist(42u32)).await.unwrap();
        assert_eq!(result, vec![42u32]);
    }

    #[tokio::test]
    async fn execute_sequence_concatenates_events_in_order() {
        let effect = Effect::persist(1u32)
            .combine(Effect::persist(2u32))
            .combine(Effect::apply_only(3u32));
        let result = execute_effect(effect).await.unwrap();
        assert_eq!(result, vec![1u32, 2, 3]);
    }
}
