//! Opt-in bounded MQTT buffering; the managed engine survives connection overload.
use lock_order::sync::Mutex;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Notify;

/// Limits for queued MQTT input and terminal replies, including per-message metadata.
#[derive(Debug, Clone, Copy)]
pub struct BufferLimits {
    /// Maximum accounted decoded incoming PUBLISH bytes awaiting session dispatch.
    pub incoming_bytes: usize,
    /// Maximum accounted reply/error bytes, through the broker's acknowledgment.
    pub reply_bytes: usize,
}

impl BufferLimits {
    pub(crate) fn validate(self) -> Result<(), String> {
        if self.incoming_bytes == 0 || self.reply_bytes == 0 {
            return Err("MQTT buffer limits must be positive".into());
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Budget {
    limit: usize,
    used: AtomicUsize,
}
impl Budget {
    fn try_charge(self: &Arc<Self>, bytes: usize) -> Option<Charge> {
        let bytes = bytes.max(1);
        let mut used = self.used.load(Ordering::SeqCst);
        loop {
            let next = used.checked_add(bytes)?;
            if next > self.limit {
                return None;
            }
            match self.used.compare_exchange_weak(used, next, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(Charge { budget: self.clone(), bytes }),
                Err(current) => used = current,
            }
        }
    }
}
#[derive(Debug)]
pub(crate) struct Charge {
    budget: Arc<Budget>,
    bytes: usize,
}
impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

#[derive(Debug, Default)]
pub(crate) struct AnswerCharges {
    pub errors: VecDeque<Charge>,
    pub replies: VecDeque<Charge>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Overflow {
    Incoming,
    Replies,
}
impl Overflow {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Incoming => "incoming_publish_bytes",
            Self::Replies => "terminal_reply_bytes",
        }
    }
}
#[derive(Debug)]
struct Flight {
    packet: Option<u16>,
    _charge: Charge,
}
#[derive(Debug)]
struct State {
    started: bool,
    epoch: Arc<()>,
    overflow: Option<Overflow>,
    loss_taken: bool,
    sent: BTreeMap<u64, Flight>,
}

#[derive(Debug)]
pub(crate) struct Buffers {
    incoming: Arc<Budget>,
    replies: Arc<Budget>,
    state: Mutex<State>,
    changed: Notify,
}
impl Buffers {
    pub(crate) fn new(limits: BufferLimits) -> Arc<Self> {
        Arc::new(Self {
            incoming: Arc::new(Budget { limit: limits.incoming_bytes, used: AtomicUsize::new(0) }),
            replies: Arc::new(Budget { limit: limits.reply_bytes, used: AtomicUsize::new(0) }),
            state: Mutex::new(
                "mqtt-buffer-state",
                State { started: false, epoch: Arc::new(()), overflow: None, loss_taken: false, sent: BTreeMap::new() },
            ),
            changed: Notify::new(),
        })
    }
    pub(crate) fn begin(self: &Arc<Self>) -> Connection {
        // WAIT: mqtt-buffer-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.epoch = Arc::new(());
        if state.started {
            state.overflow = None;
        }
        state.started = true;
        state.loss_taken = false;
        state.sent.clear();
        Connection { owner: self.clone(), epoch: state.epoch.clone() }
    }
    pub(crate) fn reserve_reply(&self, bytes: usize) -> Option<Charge> {
        // WAIT: mqtt-buffer-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.overflow.is_some() {
            return None;
        }
        let charge = self.replies.try_charge(bytes);
        if charge.is_none() {
            state.overflow = Some(Overflow::Replies);
            self.changed.notify_waiters();
        }
        charge
    }
    pub(crate) fn sent(&self, sequence: u64, charge: Charge) {
        // WAIT: mqtt-buffer-state
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).sent.insert(sequence, Flight { packet: None, _charge: charge });
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Connection {
    owner: Arc<Buffers>,
    epoch: Arc<()>,
}
impl Connection {
    fn current(&self, state: &State) -> bool {
        Arc::ptr_eq(&self.epoch, &state.epoch)
    }
    pub(crate) fn reserve_incoming(&self, bytes: usize) -> Option<Charge> {
        // WAIT: mqtt-buffer-state
        let mut state = self.owner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.current(&state) || state.overflow.is_some() {
            return None;
        }
        let charge = self.owner.incoming.try_charge(bytes);
        if charge.is_none() {
            state.overflow = Some(Overflow::Incoming);
            self.owner.changed.notify_waiters();
        }
        charge
    }
    pub(crate) fn reason(&self) -> Option<Overflow> {
        // WAIT: mqtt-buffer-state
        let state = self.owner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.current(&state).then_some(state.overflow).flatten()
    }
    pub(crate) fn claim_loss(&self) -> bool {
        // WAIT: mqtt-buffer-state
        let mut state = self.owner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.current(&state) || state.loss_taken {
            return false;
        }
        state.loss_taken = true;
        true
    }
    pub(crate) fn written(&self, sequence: u64, packet: u16) {
        // WAIT: mqtt-buffer-state
        let mut state = self.owner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.current(&state) {
            return;
        }
        if packet != 0 {
            if let Some(flight) = state.sent.get_mut(&sequence) {
                flight.packet = Some(packet);
            }
        }
    }
    pub(crate) fn acknowledged(&self, packet: u16) {
        // WAIT: mqtt-buffer-state
        let mut state = self.owner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.current(&state) {
            return;
        }
        state.sent.retain(|_, flight| flight.packet != Some(packet));
    }
}

pub(crate) async fn overflowed(connection: Option<Connection>) {
    let Some(connection) = connection else {
        // WAIT: mqtt-buffer-overflow
        return std::future::pending::<()>().await;
    };
    loop {
        // WAIT: mqtt-buffer-overflow
        let notified = connection.owner.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if connection.reason().is_some() {
            return;
        }
        // WAIT: mqtt-buffer-overflow
        notified.await;
    }
}

pub(crate) fn incoming_bytes(publish: &rumqttc::v5::mqttbytes::v5::Publish) -> usize {
    let mut bytes = publish.payload.len().saturating_add(publish.topic.len()).saturating_add(std::mem::size_of_val(publish));
    if let Some(properties) = &publish.properties {
        bytes = bytes.saturating_add(properties.user_properties.capacity().saturating_mul(std::mem::size_of::<(String, String)>()));
        for (key, value) in &properties.user_properties {
            bytes = bytes.saturating_add(key.capacity()).saturating_add(value.capacity());
        }
        bytes = bytes.saturating_add(properties.subscription_identifiers.capacity().saturating_mul(std::mem::size_of::<usize>()));
        bytes = bytes.saturating_add(properties.response_topic.as_ref().map_or(0, String::capacity));
        bytes = bytes.saturating_add(properties.content_type.as_ref().map_or(0, String::capacity));
        bytes = bytes.saturating_add(properties.correlation_data.as_ref().map_or(0, bytes::Bytes::len));
    }
    bytes
}

pub(crate) fn context_bytes(context: &crate::RequestContext) -> usize {
    context
        .generation()
        .map_or(0, str::len)
        .saturating_add(context.traceparent().map_or(0, str::len))
        .saturating_add(context.request_id().map_or(0, str::len))
}

#[cfg(test)]
mod runtime_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn charges_survive_handoff_until_the_exact_ack_and_reset_without_cross_connection_aba() {
        let buffers = Buffers::new(BufferLimits { incoming_bytes: 100, reply_bytes: 100 });
        let first = buffers.begin();
        let charge = buffers.reserve_reply(100).unwrap();
        buffers.sent(7, charge);
        first.written(6, 11);
        first.acknowledged(11);
        assert_eq!(buffers.replies.used.load(Ordering::SeqCst), 100, "an unrelated outgoing sequence released reply capacity");
        first.written(7, 12);
        first.acknowledged(11);
        assert_eq!(buffers.replies.used.load(Ordering::SeqCst), 100, "an unrelated PUBACK released reply capacity");
        first.acknowledged(12);
        assert_eq!(buffers.replies.used.load(Ordering::SeqCst), 0, "exact PUBACK did not release reply capacity");
        let charge = buffers.reserve_reply(100).unwrap();
        buffers.sent(8, charge);
        let next = buffers.begin();
        assert_eq!(buffers.replies.used.load(Ordering::SeqCst), 0, "new connection kept the old in-flight reply budget");
        buffers.sent(7, buffers.reserve_reply(100).unwrap());
        next.written(7, 12);
        first.written(7, 13);
        next.acknowledged(13);
        assert_eq!(buffers.replies.used.load(Ordering::SeqCst), 100, "old connection rebound a new outgoing packet");
        first.acknowledged(12);
        assert_eq!(buffers.replies.used.load(Ordering::SeqCst), 100, "old connection credited a reused packet identifier");
        next.acknowledged(12);
        assert_eq!(buffers.replies.used.load(Ordering::SeqCst), 0);
        assert!(first.reserve_incoming(101).is_none());
        assert!(next.reason().is_none(), "an old pump tripped the new connection's budget");
        assert!(!first.claim_loss());
        assert!(next.claim_loss());
        assert!(!next.claim_loss(), "broker-loss snapshot was taken twice");
    }
    #[tokio::test]
    async fn overflow_notifications_survive_idle_consumers_and_early_startup() {
        let buffers = Buffers::new(BufferLimits { incoming_bytes: 10, reply_bytes: 10 });
        assert!(buffers.reserve_reply(11).is_none());
        let first = buffers.begin();
        assert!(matches!(first.reason(), Some(Overflow::Replies)), "startup erased an earlier reply overflow");
        tokio::time::timeout(std::time::Duration::from_millis(100), overflowed(Some(first)))
            .await
            .expect("overflow was lost before the session registered its waiter");
        let next = buffers.begin();
        assert!(next.reason().is_none());
        let charge = next.reserve_incoming(10).unwrap();
        assert!(next.reserve_incoming(1).is_none(), "aggregate incoming bytes exceeded capacity");
        assert!(matches!(next.reason(), Some(Overflow::Incoming)));
        drop(charge);
        assert_eq!(buffers.incoming.used.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn byte_reservation_rejects_overflow_and_counts_decoded_metadata() {
        let budget = Arc::new(Budget { limit: usize::MAX, used: AtomicUsize::new(0) });
        let one = budget.try_charge(1).unwrap();
        assert!(budget.try_charge(usize::MAX).is_none(), "byte accounting wrapped past its ceiling");
        assert_eq!(budget.used.load(Ordering::SeqCst), 1);
        drop(one);
        let properties = rumqttc::v5::mqttbytes::v5::PublishProperties {
            user_properties: (0..100).map(|_| (String::new(), String::new())).collect(),
            ..Default::default()
        };
        let publish = rumqttc::v5::mqttbytes::v5::Publish::new("x", rumqttc::v5::mqttbytes::QoS::AtMostOnce, "", Some(properties));
        assert!(
            incoming_bytes(&publish) >= 100 * std::mem::size_of::<(String, String)>(),
            "decoded MQTT metadata bypassed incoming accounting"
        );
        assert!(BufferLimits { incoming_bytes: 0, reply_bytes: 1 }.validate().is_err());
        assert!(BufferLimits { incoming_bytes: 1, reply_bytes: 0 }.validate().is_err());
    }

    #[test]
    fn terminal_errors_and_batch_refusals_share_the_reply_budget_without_waiting() {
        let buffers = Buffers::new(BufferLimits { incoming_bytes: 4096, reply_bytes: 1024 });
        let connection = buffers.begin();
        let reports = crate::reports::Reports::with_buffers(Arc::new(Notify::new()), Some(buffers.clone()));
        for _ in 0..10 {
            reports.request_error(crate::wire::ErrorReport::new(crate::wire::Reason::Refused, "x".repeat(128)));
        }
        assert!(matches!(connection.reason(), Some(Overflow::Replies)), "terminal Errors escaped reply accounting");
        let sealed = reports.take_sealed();
        assert!(!sealed.snapshot.answer_errors.is_empty() && sealed.snapshot.answer_errors.len() < 10);
        assert_eq!(sealed.snapshot.answer_errors.len(), sealed.charges.errors.len());
        assert!(buffers.replies.used.load(Ordering::SeqCst) > 0, "sealing terminal Errors released their queued capacity");
        drop(sealed);
        assert_eq!(buffers.replies.used.load(Ordering::SeqCst), 0);
        let next = buffers.begin();
        reports.refused((0..10).map(|_| crate::wire::ErrorReport::new(crate::wire::Reason::Refused, "x".repeat(128))).collect(), vec![]);
        assert!(matches!(next.reason(), Some(Overflow::Replies)), "batch refusals bypassed terminal reply accounting");
        assert!(reports.discard_buffered_answers() > 0);
        assert_eq!(buffers.replies.used.load(Ordering::SeqCst), 0, "discarded terminal answers stayed charged across reconnect");
    }
}
