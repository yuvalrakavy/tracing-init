//! Immutable snapshot identity and exact broker acceptance receipts.
use super::*;
use std::time::{Duration, Instant};

const PUBLISH_INTERVAL: Duration = Duration::from_millis(100);

/// An immutable encoded State carrier. The owning engine validates its schema.
#[derive(Debug, Clone)]
pub struct RecoverySnapshot {
    id: u64,
    revision: u64,
    document: Bytes,
    context: crate::RequestContext,
}
impl RecoverySnapshot {
    /// Capture a complete encoded snapshot without copying its byte storage.
    pub fn new(revision: u64, document: Bytes, context: crate::RequestContext) -> Result<Self, String> {
        if document.is_empty() || document.len() > crate::session::MAX_PACKET_SIZE as usize {
            return Err("recovery snapshot must be nonempty and fit MAX_PACKET_SIZE".into());
        }
        static IDS: Mutex<u64> = Mutex::new(0);
        // WAIT: recovery-snapshot-identity
        let id = {
            let mut ids = IDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *ids = ids.checked_add(1).ok_or("recovery snapshot identity exhausted")?;
            *ids
        };
        Ok(Self { id, revision, document, context })
    }
    /// Process-local identity, distinct even for two snapshots with equal revisions.
    pub fn id(&self) -> u64 {
        self.id
    }
    /// The engine's selected snapshot revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Complete encoded retained State carrier.
    pub fn document(&self) -> &Bytes {
        &self.document
    }
    /// Original cause, used for tracing publication independently of request identity.
    pub fn context(&self) -> &crate::RequestContext {
        &self.context
    }
}

/// Exact result of one snapshot publication on one MQTT connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReceipt {
    /// Connection on which the snapshot was selected for publication.
    pub connection: RecoveryConnection,
    /// Immutable snapshot identity, independent of a potentially reused revision.
    pub snapshot_id: u64,
    /// The revision actually sent, never the latest pending revision.
    pub revision: u64,
    /// MQTT packet identifier; absent when rejected before sending.
    pub packet: Option<u16>,
    /// Broker acceptance or an explicit publication fault.
    pub result: Result<(), String>,
}

#[derive(Debug)]
struct Flight {
    connection: RecoveryConnection,
    snapshot: RecoverySnapshot,
    sequence: Option<u64>,
    packet: Option<u16>,
}

#[derive(Debug, Default)]
pub(super) struct Publication {
    pending: Option<RecoverySnapshot>,
    flight: Option<Flight>,
    next: Option<Instant>,
    pub(super) receipt: Option<RecoveryReceipt>,
}
impl Publication {
    pub(super) fn reset(&mut self, snapshot: Option<RecoverySnapshot>) {
        self.pending = snapshot;
        self.flight = None;
        self.receipt = None;
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        (self.pending.is_some() && self.flight.is_none()).then(|| self.next.unwrap_or_else(Instant::now))
    }
}

impl RecoveryShared {
    pub(super) fn validate_snapshot(&self, snapshot: &RecoverySnapshot) -> Result<(), String> {
        if snapshot.document.len() > self.options.max_bytes {
            return Err("recovery snapshot exceeds max_bytes".into());
        }
        Ok(())
    }
    pub(super) fn offer(&self, connection: RecoveryConnection, snapshot: RecoverySnapshot) -> Result<(), String> {
        self.validate_snapshot(&snapshot)?;
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.connection != Some(connection) || !matches!(state.phase, Phase::Open(RecoveryReady::Ready)) {
            return Err("snapshot publication requires the current Ready connection".into());
        }
        let wake = state.publication.pending.is_none() && state.publication.flight.is_none();
        state.publication.pending = Some(snapshot);
        drop(state);
        if wake {
            self.changed.notify_one();
        }
        Ok(())
    }
    pub(crate) fn publication_deadline(&self) -> Option<Instant> {
        // WAIT: recovery-state
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        matches!(state.phase, Phase::Open(RecoveryReady::Ready)).then(|| state.publication.deadline()).flatten()
    }
    pub(crate) fn take_publication(&self) -> Option<RecoverySnapshot> {
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(state.phase, Phase::Open(RecoveryReady::Ready)) || state.publication.deadline()? > Instant::now() {
            return None;
        }
        let connection = state.connection?;
        let snapshot = state.publication.pending.take()?;
        state.publication.next = Instant::now().checked_add(PUBLISH_INTERVAL);
        state.publication.flight = Some(Flight { connection, snapshot: snapshot.clone(), sequence: None, packet: None });
        Some(snapshot)
    }
    pub(crate) fn publication_sent(&self, id: u64, sequence: Option<u64>) {
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(flight) = &mut state.publication.flight else { return };
        if flight.snapshot.id != id {
            return;
        }
        flight.sequence = sequence;
        if sequence.is_some() {
            state.publication.next = Instant::now().checked_add(PUBLISH_INTERVAL);
        }
        if sequence.is_none() {
            Self::finish_publication(&mut state, Err("MQTT refused the snapshot before sending".into()));
            self.changed.notify_one();
        }
    }
    pub(crate) fn publication_rejected(&self, id: u64, error: String) {
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.publication.flight.as_ref().is_some_and(|flight| flight.snapshot.id == id) {
            Self::finish_publication(&mut state, Err(error));
            self.changed.notify_one();
        }
    }
    pub(crate) fn publication_written(&self, sequence: u64, packet: u16) {
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(flight) = &mut state.publication.flight {
            if flight.sequence == Some(sequence) && packet != 0 {
                flight.packet = Some(packet);
            }
        }
    }
    pub(crate) fn publication_ack(&self, packet: u16, result: Result<(), String>) {
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.publication.flight.as_ref().is_some_and(|flight| flight.packet == Some(packet)) {
            Self::finish_publication(&mut state, result);
            self.changed.notify_one();
        }
    }
    fn finish_publication(state: &mut State, result: Result<(), String>) {
        let Some(flight) = state.publication.flight.take() else { return };
        state.publication.receipt = Some(RecoveryReceipt {
            connection: flight.connection,
            snapshot_id: flight.snapshot.id,
            revision: flight.snapshot.revision,
            packet: flight.packet,
            result,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> (Arc<RecoveryShared>, RecoveryConnection) {
        let shared = RecoveryShared::new(RecoveryOptions { target: "_recovery".into(), max_bytes: 1024 }).unwrap();
        let connection = shared.begin().unwrap();
        shared.readback(connection, None).unwrap();
        shared.finish(connection, RecoveryReady::Ready).unwrap();
        (shared, connection)
    }
    fn snapshot(revision: u64) -> RecoverySnapshot {
        RecoverySnapshot::new(revision, Bytes::from_static(b"{\"snapshot\":\"selected\"}"), crate::RequestContext::default()).unwrap()
    }
    #[test]
    fn receipt_requires_both_the_exact_outgoing_sequence_and_its_packet_ack() {
        let (shared, connection) = ready();
        let snapshot = snapshot(9);
        let handle = shared.handle();
        handle.publish(connection, snapshot.clone()).unwrap();
        assert_eq!(shared.take_publication().unwrap().id(), snapshot.id());
        shared.publication_sent(snapshot.id(), Some(12));
        shared.publication_written(11, 80);
        shared.publication_ack(80, Ok(()));
        assert!(handle.receipt().is_none(), "an unrelated outgoing packet credited the snapshot");
        shared.publication_written(12, 81);
        shared.publication_ack(80, Ok(()));
        assert!(handle.receipt().is_none(), "an unrelated PUBACK credited the snapshot");
        shared.publication_ack(81, Err("NotAuthorized".into()));
        let receipt = handle.receipt().unwrap();
        assert_eq!(receipt.snapshot_id, snapshot.id());
        assert_eq!(receipt.revision, 9);
        assert_eq!(receipt.connection, connection);
        assert_eq!(receipt.packet, Some(81));
        assert_eq!(receipt.result.unwrap_err(), "NotAuthorized");
    }
    #[test]
    fn publication_is_current_ready_only_bounded_and_paced_from_actual_enqueue() {
        let (shared, connection) = ready();
        let handle = shared.handle();
        let first = snapshot(1);
        handle.publish(connection, first.clone()).unwrap();
        let sending = shared.take_publication().unwrap();
        assert_eq!(sending.document.as_ptr(), first.document.as_ptr(), "snapshot handoff copied immutable bytes");
        for revision in 2..=1000 {
            handle.publish(connection, snapshot(revision)).unwrap();
        }
        shared.state.lock().unwrap().publication.next = Some(Instant::now() - Duration::from_secs(1));
        assert!(shared.take_publication().is_none(), "more than one snapshot entered flight");
        // The rotation could have delayed this publish well past its initial due time.
        shared.publication_sent(first.id(), Some(21));
        shared.publication_written(21, 9);
        shared.publication_ack(9, Ok(()));
        assert!(
            shared.publication_deadline().unwrap() >= Instant::now() + Duration::from_millis(90),
            "publication pacing started before the actual enqueue"
        );
        assert!(shared.take_publication().is_none(), "pending snapshot bypassed publication pacing");
        assert_eq!(shared.state.lock().unwrap().publication.pending.as_ref().unwrap().revision, 1000);
        shared.lost();
        assert!(handle.receipt().is_none(), "a lost connection kept its receipt current");
        assert!(handle.publish(connection, snapshot(1001)).is_err());
        let next = shared.begin().unwrap();
        shared.readback(next, None).unwrap();
        shared.finish(next, RecoveryReady::RecoveryOnly).unwrap();
        assert!(handle.publish(next, snapshot(1002)).is_err(), "RecoveryOnly overwrote retained recovery evidence");
        assert!(handle.publish(connection, snapshot(1003)).is_err(), "an old connection replaced the selected snapshot");
        shared.publication_written(21, 9);
        shared.publication_ack(9, Ok(()));
        assert!(handle.receipt().is_none(), "an old packet produced a new connection's receipt");
        assert!(shared.take_publication().is_none());
        shared.finish(next, RecoveryReady::Ready).unwrap();
        assert!(handle.publish(connection, snapshot(1004)).is_err(), "an old connection replaced a new Ready connection's snapshot");
    }
    #[test]
    fn snapshot_identity_is_not_its_revision_and_oversized_offers_do_not_replace_pending() {
        let (shared, connection) = ready();
        let handle = shared.handle();
        let first = snapshot(7);
        let other = snapshot(7);
        assert_ne!(first.id(), other.id(), "equal revisions reused snapshot identity");
        handle.publish(connection, first.clone()).unwrap();
        let oversized = RecoverySnapshot::new(8, Bytes::from(vec![b'x'; 1025]), crate::RequestContext::default()).unwrap();
        assert!(handle.publish(connection, oversized).is_err(), "oversized pending snapshot was admitted");
        assert_eq!(shared.take_publication().unwrap().id(), first.id());
        assert!(RecoverySnapshot::new(0, Bytes::new(), crate::RequestContext::default()).is_err());
        assert!(RecoverySnapshot::new(
            0,
            Bytes::from(vec![0; crate::session::MAX_PACKET_SIZE as usize + 1]),
            crate::RequestContext::default()
        )
        .is_err());
    }

    #[test]
    fn exact_images_cannot_publish_oversized_or_recovery_only_snapshots() {
        let options = RecoveryOptions { target: "_recovery".into(), max_bytes: 1024 };
        let mut image = RecoveryImage {
            context: crate::RequestContext::default(),
            generations: crate::TargetGenerations { instance: "instance.1".into(), targets: BTreeMap::new() },
            states: BTreeMap::new(),
            configurations: BTreeMap::new(),
            snapshot: Some(snapshot(1)),
        };
        image.validate(&options, RecoveryReady::Ready).unwrap();
        assert!(
            image.validate(&options, RecoveryReady::RecoveryOnly).unwrap_err().contains("evidence"),
            "a recovery-only image can overwrite retained evidence"
        );
        image.snapshot = Some(RecoverySnapshot::new(2, Bytes::from(vec![b'x'; 1025]), crate::RequestContext::default()).unwrap());
        assert!(
            image.validate(&options, RecoveryReady::Ready).unwrap_err().contains("max_bytes"),
            "oversized installed snapshot bypassed image validation"
        );
    }

    #[test]
    fn snapshot_packet_size_matches_real_encoding_and_preserves_only_its_explicit_trace() {
        const PARENT: &str = "00-11111111111111111111111111111111-2222222222222222-01";
        let (shared, _) = ready();
        let cause = crate::RequestContext::new("array-generation.1").unwrap().with_traceparent(Some(PARENT));
        let snapshot = RecoverySnapshot::new(1, Bytes::from(vec![b'x'; 1024]), cause).unwrap();
        let out = crate::session::Out::recovery("Recovery/State/i/_recovery".into(), snapshot.clone(), shared);
        assert_eq!(out.traceparent_for_test(), Some(PARENT));
        let properties = rumqttc::v5::mqttbytes::v5::PublishProperties {
            user_properties: vec![("traceparent".into(), PARENT.into())],
            ..Default::default()
        };
        let mut packet = rumqttc::v5::mqttbytes::v5::Publish::new(
            "Recovery/State/i/_recovery",
            rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
            snapshot.document.clone(),
            Some(properties),
        );
        packet.pkid = 1;
        packet.retain = true;
        let mut encoded = bytes::BytesMut::new();
        packet.write(&mut encoded).unwrap();
        assert_eq!(out.packet_size(), encoded.len(), "snapshot packet admission omitted wire overhead or retained an array generation");
    }

    #[test]
    fn an_autonomous_snapshot_does_not_adopt_an_unrelated_ambient_trace() {
        use opentelemetry::trace::TracerProvider;
        use tracing_subscriber::prelude::*;
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(provider.tracer("snapshot-test")));
        tracing::subscriber::with_default(subscriber, || {
            let unrelated = tracing::info_span!("unrelated_lifecycle_operation");
            let _entered = unrelated.enter();
            assert!(tracing_init::traceparent::current().is_some(), "test did not establish an ambient OTel trace");
            let (shared, _) = ready();
            let out = crate::session::Out::recovery("Recovery/State/i/_recovery".into(), snapshot(0), shared);
            assert_eq!(out.traceparent_for_test(), None, "snapshot adopted an unrelated ambient trace");
        });
    }
}
