//! Opt-in retained recovery handshake for a fixed managed bridge.

mod publication;
pub use publication::{RecoveryReceipt, RecoverySnapshot};

use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, watch, Notify};

/// Maximum wait for the engine's initial decision after a connection's replay.
pub const INSTALL_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Reserved retained State target and maximum encoded snapshot size.
#[derive(Debug, Clone)]
pub struct RecoveryOptions {
    /// One metadata target, intercepted before ordinary target handling.
    pub target: String,
    /// Maximum retained payload bytes, including its JSON carrier.
    pub max_bytes: usize,
}
impl RecoveryOptions {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !crate::topics::valid_segment(&self.target) || self.target == "_sync" {
            return Err("recovery target must be one topic segment other than _sync".into());
        }
        if self.max_bytes == 0 || self.max_bytes > crate::session::MAX_PACKET_SIZE as usize {
            return Err("recovery max_bytes must be within 1..=MAX_PACKET_SIZE".into());
        }
        Ok(())
    }
}

/// Opaque connection capability. Never persisted or reconstructed from MQTT input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryConnection {
    owner: u64,
    epoch: u64,
}
impl RecoveryConnection {
    pub(crate) fn nonce(&self) -> String {
        format!("{}-{}-{}", std::process::id(), self.owner, self.epoch)
    }
}

/// Which admission mode an installed recovery image permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryReady {
    /// Ordinary requests and model publication may proceed.
    Ready,
    /// Only recovery commands and their metadata may proceed.
    RecoveryOnly,
}

/// Complete recovery seed. Ordinary State replay never merges into this image.
#[derive(Debug, Clone)]
pub struct RecoveryImage {
    /// Original triggering request's cause and expiry; default for autonomous startup.
    pub context: crate::RequestContext,
    /// Complete instance and target routing authority.
    pub generations: crate::TargetGenerations,
    /// Complete assumed model and conflict-rule seed, by target.
    pub states: BTreeMap<String, crate::PropertyMap>,
    /// Complete accepted target setups, used to replace configuration caches.
    pub configurations: BTreeMap<String, crate::Value>,
    /// Selected immutable retained snapshot, installed with this exact image.
    pub snapshot: Option<RecoverySnapshot>,
}
impl RecoveryImage {
    pub(crate) fn check_deadline(&self) -> Result<(), String> {
        if self.context.deadline().is_some_and(|deadline| deadline <= std::time::Instant::now()) {
            return Err("the original recovery request expired before installation".into());
        }
        Ok(())
    }
    pub(crate) fn validate(&self, options: &RecoveryOptions, ready: RecoveryReady) -> Result<(), String> {
        self.check_deadline()?;
        self.generations.validate()?;
        if self.snapshot.as_ref().is_some_and(|snapshot| snapshot.document().len() > options.max_bytes) {
            return Err("recovery snapshot exceeds max_bytes".into());
        }
        if ready == RecoveryReady::RecoveryOnly && self.snapshot.is_some() {
            return Err("recovery-only mode cannot replace retained recovery evidence".into());
        }
        if self.generations.targets.contains_key(&options.target)
            || self.states.contains_key(&options.target)
            || self.configurations.contains_key(&options.target)
        {
            return Err("the recovery metadata target cannot become an ordinary target".into());
        }
        if ready == RecoveryReady::RecoveryOnly
            && (!self.generations.targets.is_empty() || !self.states.is_empty() || !self.configurations.is_empty())
        {
            return Err("recovery-only mode requires an empty target table, model and setup".into());
        }
        for target in self.states.keys().chain(self.configurations.keys()) {
            if !self.generations.targets.contains_key(target) {
                return Err("recovery image contains a target outside its complete generation table".into());
            }
        }
        let bytes = serde_json::to_vec(&serde_json::json!({
            "generations": {"instance":self.generations.instance,"targets":self.generations.targets},
            "states":self.states,"configurations":self.configurations,
        }))
        .map_err(|error| error.to_string())?
        .len();
        if bytes > crate::session::MAX_PACKET_SIZE as usize {
            return Err("recovery model seed exceeds MAX_PACKET_SIZE".into());
        }
        Ok(())
    }
}

/// One completed retained replay. An invalid replay is never represented as absence.
#[derive(Debug, Clone)]
pub struct RecoveryReadback {
    /// Capability of the connection whose matched barrier completed this replay.
    pub connection: RecoveryConnection,
    /// Absent, bounded encoded payload, or a fault that requires recovery-only admission.
    pub document: Result<Option<Bytes>, String>,
}

/// Engine-side receiver for the latest completed replay; no unbounded event queue.
#[derive(Debug, Clone)]
pub struct RecoveryHandle {
    shared: Arc<RecoveryShared>,
    readback: watch::Receiver<Option<RecoveryReadback>>,
    initial: bool,
}
impl RecoveryHandle {
    /// Replace the pending snapshot on the current Ready connection. This does
    /// not wait for MQTT or imply broker acceptance.
    pub fn publish(&self, connection: RecoveryConnection, snapshot: RecoverySnapshot) -> Result<(), String> {
        self.shared.offer(connection, snapshot)
    }
    /// Latest exact broker receipt, if any; a receipt is not broker disk durability.
    pub fn receipt(&self) -> Option<RecoveryReceipt> {
        // WAIT: recovery-state
        self.shared.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).publication.receipt.clone()
    }
    /// Ask the current session to atomically install a complete seed, then open
    /// its gates. Success confirms installation, not snapshot replication.
    pub async fn complete(
        &self,
        connection: RecoveryConnection,
        image: RecoveryImage,
        ready: RecoveryReady,
        completion: Option<crate::CommandCompletion>,
    ) -> Result<(), String> {
        image.validate(&self.shared.options, ready)?;
        let (reply, result) = oneshot::channel();
        {
            // WAIT: recovery-state
            let mut state = self.shared.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.check_install(connection, ready)?;
            if state.pending.as_ref().is_some_and(|pending| !pending.reply.is_closed()) {
                return Err("a recovery installation is already pending".into());
            }
            state.pending = Some(Install { connection, image, ready, completion, reply });
        }
        self.shared.changed.notify_one();
        // WAIT: recovery-install
        result.await.map_err(|_| "recovery installation was abandoned".to_owned())?
    }
    /// Wait for a completed replay. Race this with service cancellation.
    pub async fn readback(&mut self) -> Result<RecoveryReadback, String> {
        if self.initial {
            self.initial = false;
            if let Some(readback) = self.readback.borrow_and_update().clone() {
                return Ok(readback);
            }
        }
        loop {
            // WAIT: recovery-readback
            self.readback.changed().await.map_err(|_| "recovery runtime ended".to_owned())?;
            if let Some(readback) = self.readback.borrow_and_update().clone() {
                return Ok(readback);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    Closed,
    Reading,
    Awaiting { valid: bool },
    Open(RecoveryReady),
}
#[derive(Debug)]
struct State {
    epoch: u64,
    connection: Option<RecoveryConnection>,
    phase: Phase,
    pending: Option<Install>,
    install_until: Option<std::time::Instant>,
    publication: publication::Publication,
}
impl State {
    fn check_install(&self, connection: RecoveryConnection, ready: RecoveryReady) -> Result<(), String> {
        if self.connection != Some(connection) {
            return Err("recovery completion belongs to no current connection".into());
        }
        if self.install_until.is_some_and(|until| until <= std::time::Instant::now()) {
            return Err("the recovery engine decision exceeded the connection bound".into());
        }
        match (self.phase, ready) {
            (Phase::Awaiting { valid: true }, _)
            | (Phase::Awaiting { valid: false }, RecoveryReady::RecoveryOnly)
            | (Phase::Open(RecoveryReady::RecoveryOnly), _) => Ok(()),
            _ => Err("recovery replay is incomplete, invalid or already installed".into()),
        }
    }
}
#[derive(Debug)]
pub(crate) struct Install {
    pub connection: RecoveryConnection,
    pub image: RecoveryImage,
    pub ready: RecoveryReady,
    pub completion: Option<crate::CommandCompletion>,
    reply: oneshot::Sender<Result<(), String>>,
}

/// Shared coordinator; only the session can acknowledge a connection's replay/install.
#[derive(Debug)]
pub(crate) struct RecoveryShared {
    pub(crate) options: RecoveryOptions,
    owner: u64,
    state: Mutex<State>,
    readback: watch::Sender<Option<RecoveryReadback>>,
    changed: Notify,
}
impl RecoveryShared {
    pub(crate) fn new(options: RecoveryOptions) -> Result<Arc<Self>, String> {
        static OWNERS: Mutex<u64> = Mutex::new(0);
        options.validate()?;
        // WAIT: recovery-identity
        let owner = {
            let mut owners = OWNERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *owners = owners.checked_add(1).ok_or("recovery owner identity exhausted")?;
            *owners
        };
        let (readback, _) = watch::channel(None);
        Ok(Arc::new(Self {
            options,
            owner,
            state: Mutex::new(State {
                epoch: 0,
                connection: None,
                phase: Phase::Closed,
                pending: None,
                install_until: None,
                publication: Default::default(),
            }),
            readback,
            changed: Notify::new(),
        }))
    }
    pub(crate) fn handle(self: &Arc<Self>) -> RecoveryHandle {
        RecoveryHandle { shared: self.clone(), readback: self.readback.subscribe(), initial: true }
    }
    pub(crate) fn begin(&self) -> Result<RecoveryConnection, String> {
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.epoch = state.epoch.checked_add(1).ok_or("recovery connection identity exhausted")?;
        let connection = RecoveryConnection { owner: self.owner, epoch: state.epoch };
        state.connection = Some(connection);
        state.phase = Phase::Reading;
        state.install_until = None;
        state.pending.take();
        state.publication.reset(None);
        self.readback.send_replace(None);
        Ok(connection)
    }
    #[cfg(test)]
    pub(crate) fn readback(&self, connection: RecoveryConnection, document: Option<Bytes>) -> Result<(), String> {
        self.readback_result(connection, Ok(document))
    }
    pub(crate) fn readback_result(&self, connection: RecoveryConnection, document: Result<Option<Bytes>, String>) -> Result<(), String> {
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.connection != Some(connection) || !matches!(state.phase, Phase::Reading) {
            return Err("recovery replay belongs to no current reading connection".into());
        }
        let document = if document.as_ref().ok().and_then(Option::as_ref).is_some_and(|document| document.len() > self.options.max_bytes) {
            Err("retained recovery payload exceeds max_bytes".into())
        } else {
            document
        };
        state.phase = Phase::Awaiting { valid: document.is_ok() };
        state.install_until =
            Some(std::time::Instant::now().checked_add(INSTALL_WAIT).ok_or("recovery decision deadline is unrepresentable")?);
        self.readback.send_replace(Some(RecoveryReadback { connection, document }));
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn finish(&self, connection: RecoveryConnection, ready: RecoveryReady) -> Result<(), String> {
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.check_install(connection, ready)?;
        state.phase = Phase::Open(ready);
        state.install_until = None;
        Ok(())
    }
    pub(crate) fn lost(&self) {
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.connection = None;
        state.phase = Phase::Closed;
        state.install_until = None;
        state.pending.take();
        state.publication.reset(None);
        self.readback.send_replace(None);
    }
    pub(crate) fn readiness(&self) -> Option<RecoveryReady> {
        // WAIT: recovery-state
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.phase {
            Phase::Open(ready) => Some(ready),
            _ => None,
        }
    }
    pub(crate) async fn changed(&self) {
        // WAIT: recovery-install
        self.changed.notified().await;
    }
    pub(crate) fn install_deadline(&self) -> Option<std::time::Instant> {
        // WAIT: recovery-state
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).install_until
    }
    pub(crate) fn install_pending(&self, apply: impl FnOnce(&Install) -> Result<(), String>) -> Result<bool, String> {
        // WAIT: recovery-state
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(mut install) = state.pending.take() else { return Ok(false) };
        if install.reply.is_closed() {
            return Ok(false);
        }
        install.image.context = install.image.context.clone().bounded_deadline(state.install_until);
        let result = state
            .check_install(install.connection, install.ready)
            .and_then(|()| install.image.check_deadline())
            .and_then(|()| apply(&install));
        let installed = result.is_ok();
        if installed {
            state.publication.reset(install.image.snapshot.clone());
            state.phase = Phase::Open(install.ready);
            state.install_until = None;
        }
        let _ = install.reply.send(result);
        Ok(installed)
    }
}

#[cfg(test)]
mod runtime_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_image() -> RecoveryImage {
        RecoveryImage {
            context: crate::RequestContext::default(),
            generations: crate::TargetGenerations { instance: "instance.1".into(), targets: BTreeMap::new() },
            states: BTreeMap::new(),
            configurations: BTreeMap::new(),
            snapshot: None,
        }
    }

    #[test]
    fn expired_image_cannot_change_authority_model_or_queued_work_at_the_final_swap() {
        let reports = crate::reports::Reports::new();
        let mailbox = crate::mailbox::Mailbox::new();
        mailbox.bind_reports(&reports);
        let mut image = empty_image();
        image.generations.targets.insert("a".into(), "generation.a".into());
        let table = image.generations.clone();
        reports.install_policy(crate::RequestPolicy::Fenced(table.clone()));
        let context = crate::RequestContext::new("generation.a").unwrap();
        let original = serde_json::json!({"level":7}).as_object().unwrap().clone();
        reports.state_with_context("a", original.clone(), false, &context);
        mailbox.connected();
        mailbox.post_config_with_context("a", Some(serde_json::json!({"channel":1})), context);
        let properties = rumqttc::v5::mqttbytes::v5::PublishProperties { message_expiry_interval: Some(1), ..Default::default() };
        image.context =
            crate::wire::request_meta(Some(&properties), std::time::Instant::now() - std::time::Duration::from_secs(2)).unwrap().context;
        image.generations.instance = "next.instance".into();
        image.states.insert("a".into(), serde_json::json!({"level":22}).as_object().unwrap().clone());
        assert!(
            mailbox.replace_recovery(&reports, &image, None).unwrap_err().contains("expired"),
            "expired image changed the runtime at its final swap"
        );
        assert_eq!(reports.current_policy(), Some(crate::RequestPolicy::Fenced(table)));
        assert_eq!(reports.snapshot(), [("a".into(), original)].into());
        assert!(reports.recovery_cache(0).is_none());
        assert!(matches!(mailbox.try_take(), Some(crate::mailbox::Work::Configure { .. })));
    }

    #[test]
    fn exact_setup_restores_feedback_overrides_and_later_absence_clears_them() {
        let reports = crate::reports::Reports::new();
        let mailbox = crate::mailbox::Mailbox::new();
        mailbox.bind_reports(&reports);
        let mut image = empty_image();
        image.generations.targets.insert("a".into(), "generation.a".into());
        reports.install_policy(crate::RequestPolicy::Fenced(image.generations.clone()));
        mailbox.connected();
        let context = crate::RequestContext::new("generation.a").unwrap();
        let original = serde_json::json!({"feedback":"assumed"});
        mailbox.post_config_with_context("a", Some(original.clone()), context.clone());
        assert!(matches!(mailbox.try_take(), Some(crate::mailbox::Work::Configure { .. })));
        assert!(mailbox.configured_with_context("a", Some(&original), &context));
        assert_eq!(mailbox.feedback("a", crate::Feedback::Observed), crate::Feedback::Assumed);
        image.configurations.insert("a".into(), serde_json::json!({"feedback":"observed"}));
        mailbox.replace_recovery(&reports, &image, None).unwrap();
        assert_eq!(
            mailbox.feedback("a", crate::Feedback::Assumed),
            crate::Feedback::Observed,
            "exact recovery lost the setup feedback override"
        );
        image.configurations.insert("a".into(), serde_json::json!({"channel":3}));
        mailbox.replace_recovery(&reports, &image, None).unwrap();
        assert_eq!(
            mailbox.feedback("a", crate::Feedback::Assumed),
            crate::Feedback::Assumed,
            "an absent recovery override retained prior feedback"
        );
    }

    #[test]
    fn exact_replacement_removes_old_model_caches_and_queued_work_even_when_a_generation_stays_the_same() {
        let reports = crate::reports::Reports::new();
        let mailbox = crate::mailbox::Mailbox::new();
        mailbox.bind_reports(&reports);
        let a = crate::RequestContext::new("generation.a").unwrap();
        let b = crate::RequestContext::new("generation.b").unwrap();
        let table = crate::TargetGenerations {
            instance: "instance.1".into(),
            targets: [("a".into(), "generation.a".into()), ("b".into(), "generation.b".into())].into(),
        };
        reports.install_policy(crate::RequestPolicy::Fenced(table));
        let values = |document: serde_json::Value| document.as_object().unwrap().clone();
        reports.state_with_context("a", values(serde_json::json!({"level":5,"old_only":9})), false, &a);
        reports.state_with_context("b", values(serde_json::json!({"level":6})), false, &b);
        reports.read_back_with_context("b", values(serde_json::json!({"level":7})), &b);
        reports.read_back_complete();
        reports.written_with_context("a", &values(serde_json::json!({"level":8})), reports.reports_for("a"), &a);
        reports.error(crate::wire::ErrorReport::new(crate::wire::Reason::Rejected, "old output"));
        mailbox.connected();
        mailbox.post_config_with_context("b", Some(serde_json::json!({"old":true})), b);
        mailbox.post_state(
            crate::mailbox::StateValue {
                context: a.clone(),
                target: "a".into(),
                property: "level".into(),
                value: serde_json::json!(99),
                deadline: None,
                conflict: crate::Conflict::RequestWins,
                check: Default::default(),
                basis: None,
                reheld: false,
            },
            false,
        );
        mailbox.post_command_with_context(
            Some("a".into()),
            crate::CommandCall { command: "pulse".into(), params: Default::default() },
            None,
            None,
            a,
        );
        let mut image = empty_image();
        image.generations.targets.insert("a".into(), "generation.a".into());
        image.states.insert("a".into(), values(serde_json::json!({"level":22})));
        image.configurations.insert("a".into(), serde_json::json!({"channel":2}));
        let trace = "00-112233445566778899aabbccddeeff00-1234567890abcdef-01";
        image.context = crate::RequestContext::new("retired.instance").unwrap().with_traceparent(Some(trace));
        let refused = mailbox.replace_recovery(&reports, &image, None).unwrap();
        assert_eq!(refused.len(), 1, "same-generation queued work was not retired by exact replacement");
        assert!(mailbox.try_take().is_none(), "old queued work survived exact recovery replacement");
        assert_eq!(reports.snapshot(), image.states, "recovery merged old model or acknowledged-write values");
        assert_eq!(reports.read_back_view(), (true, image.states.clone()), "old conflict readback survived replacement");
        let cache = reports.recovery_cache(0).unwrap();
        assert_eq!(cache.epoch, 1);
        assert_eq!(cache.configurations, image.configurations);
        assert_eq!(cache.states, image.states);
        assert!(reports.recovery_cache(cache.epoch).is_none());
        let output = reports.take();
        assert_eq!(output.states.len(), 1);
        assert_eq!(output.states[0].2.traceparent(), Some(trace), "recovery model lost the initiating request's cause");
        assert_eq!(output.states[0].2.generation(), Some("generation.a"), "the old request token relabeled recovery output");
        assert!(output.errors.is_empty());
        assert!(output.answer_errors.is_empty());
    }

    #[tokio::test]
    async fn engine_completion_waits_for_exact_install_and_failed_install_keeps_admission_closed() {
        for fail in [true, false] {
            let shared = RecoveryShared::new(RecoveryOptions { target: "_recovery".into(), max_bytes: 1024 }).unwrap();
            let connection = shared.begin().unwrap();
            shared.readback(connection, None).unwrap();
            let handle = shared.handle();
            let mut waiting = tokio::spawn(async move { handle.complete(connection, empty_image(), RecoveryReady::Ready, None).await });
            shared.changed().await;
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), &mut waiting).await.is_err(),
                "completion acknowledged an image before installation"
            );
            assert_eq!(shared.readiness(), None);
            let mut applied = 0;
            assert_eq!(
                shared
                    .install_pending(|install| {
                        applied += 1;
                        assert_eq!(install.connection, connection);
                        if fail {
                            Err("refused install".into())
                        } else {
                            Ok(())
                        }
                    })
                    .unwrap(),
                !fail
            );
            assert_eq!(applied, 1);
            assert_eq!(waiting.await.unwrap().is_err(), fail);
            assert_eq!(shared.readiness(), if fail { None } else { Some(RecoveryReady::Ready) });
        }
    }

    #[tokio::test]
    async fn cancelled_or_disconnected_installations_never_reach_the_install_callback() {
        for disconnect in [false, true] {
            let shared = RecoveryShared::new(RecoveryOptions { target: "_recovery".into(), max_bytes: 1024 }).unwrap();
            let connection = shared.begin().unwrap();
            shared.readback(connection, None).unwrap();
            let handle = shared.handle();
            let waiting = tokio::spawn(async move { handle.complete(connection, empty_image(), RecoveryReady::Ready, None).await });
            shared.changed().await;
            if disconnect {
                shared.begin().unwrap();
                assert!(waiting.await.unwrap().is_err());
            } else {
                waiting.abort();
                assert!(waiting.await.is_err());
            }
            assert!(!shared.install_pending(|_| panic!("abandoned image was installed")).unwrap());
            assert_eq!(shared.readiness(), None);
        }
    }

    #[tokio::test]
    async fn original_expiry_is_rechecked_at_the_serialized_recovery_install() {
        let shared = RecoveryShared::new(RecoveryOptions { target: "_recovery".into(), max_bytes: 1024 }).unwrap();
        let connection = shared.begin().unwrap();
        shared.readback(connection, None).unwrap();
        let mut image = empty_image();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        image.context = crate::RequestContext::default().with_deadline(Some(deadline));
        let handle = shared.handle();
        let waiting = tokio::spawn(async move { handle.complete(connection, image, RecoveryReady::Ready, None).await });
        tokio::time::timeout(std::time::Duration::from_secs(1), shared.changed())
            .await
            .expect("image did not reach the pending installation slot");
        tokio::time::sleep_until(deadline.into()).await;
        assert!(!shared.install_pending(|_| panic!("an expired recovery image reached the commit callback")).unwrap());
        assert!(waiting.await.unwrap().unwrap_err().contains("expired"));
        assert_eq!(shared.readiness(), None);
    }

    #[test]
    fn connection_capabilities_are_not_interchangeable_between_bridge_owners() {
        let a = RecoveryShared::new(RecoveryOptions { target: "_recovery".into(), max_bytes: 1024 }).unwrap();
        let b = RecoveryShared::new(RecoveryOptions { target: "_recovery".into(), max_bytes: 1024 }).unwrap();
        let old = a.begin().unwrap();
        let current = b.begin().unwrap();
        b.readback(current, None).unwrap();
        assert!(b.readback(old, None).is_err());
        assert!(b.finish(old, RecoveryReady::Ready).is_err());
        assert_eq!(b.readiness(), None);
    }

    #[test]
    fn every_connection_closes_the_gate_and_stale_completions_cannot_reopen_it() {
        let shared = RecoveryShared::new(RecoveryOptions { target: "_recovery".into(), max_bytes: 1024 }).unwrap();
        let first = shared.begin().unwrap();
        shared.readback(first, None).unwrap();
        shared.finish(first, RecoveryReady::Ready).unwrap();
        assert_eq!(shared.readiness(), Some(RecoveryReady::Ready));
        let second = shared.begin().unwrap();
        assert_ne!(first, second);
        assert_eq!(shared.readiness(), None);
        assert!(shared.finish(first, RecoveryReady::Ready).is_err());
        assert_eq!(shared.readiness(), None, "a stale connection reopened recovery output");
        assert!(shared.finish(second, RecoveryReady::Ready).is_err(), "completion bypassed the replay barrier");
        shared.readback(second, None).unwrap();
        assert!(shared.finish(first, RecoveryReady::Ready).is_err(), "old completion installed into a newer replay");
        assert_eq!(shared.readiness(), None);
        shared.finish(second, RecoveryReady::RecoveryOnly).unwrap();
        assert_eq!(shared.readiness(), Some(RecoveryReady::RecoveryOnly));
        shared.lost();
        assert_eq!(shared.readiness(), None);
        assert!(shared.finish(second, RecoveryReady::Ready).is_err());
    }

    #[tokio::test]
    async fn readback_watch_distinguishes_absence_from_invalid_and_keeps_only_the_latest_connection() {
        let shared = RecoveryShared::new(RecoveryOptions { target: "_recovery".into(), max_bytes: 4 }).unwrap();
        let mut handle = shared.handle();
        let old = shared.begin().unwrap();
        shared.readback(old, Some(Bytes::from_static(b"old"))).unwrap();
        let current = shared.begin().unwrap();
        assert!(shared.readback(old, None).is_err());
        shared.readback(current, None).unwrap();
        let event = handle.readback().await.unwrap();
        assert_eq!(event.connection, current);
        assert_eq!(event.document.unwrap(), None);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), handle.readback()).await.is_err(),
            "the same replay was delivered twice"
        );
        let next = shared.begin().unwrap();
        shared.readback(next, Some(Bytes::from_static(b"12345"))).unwrap();
        let event = handle.readback().await.unwrap();
        assert_eq!(event.connection, next);
        assert!(event.document.is_err(), "oversized replay was mistaken for absence");
        assert!(shared.finish(next, RecoveryReady::Ready).is_err(), "invalid replay opened ordinary admission");
        shared.finish(next, RecoveryReady::RecoveryOnly).unwrap();
    }

    #[test]
    fn options_bound_the_reserved_metadata_target_and_snapshot_memory() {
        for (target, max_bytes) in [("", 1), ("a/b", 1), ("_sync", 1), ("_recovery", 0), ("_recovery", usize::MAX)] {
            assert!(RecoveryShared::new(RecoveryOptions { target: target.into(), max_bytes }).is_err());
        }
    }

    #[test]
    fn exact_image_refuses_unknown_targets_reserved_metadata_and_provisional_state() {
        let options = RecoveryOptions { target: "_recovery".into(), max_bytes: 1024 };
        let mut image = RecoveryImage {
            context: crate::RequestContext::default(),
            generations: crate::TargetGenerations { instance: "instance.1".into(), targets: [("a".into(), "generation.1".into())].into() },
            states: [("a".into(), serde_json::json!({"level":22}).as_object().unwrap().clone())].into(),
            configurations: [("a".into(), serde_json::json!({"channel":1}))].into(),
            snapshot: None,
        };
        image.validate(&options, RecoveryReady::Ready).unwrap();
        assert!(image.validate(&options, RecoveryReady::RecoveryOnly).is_err());
        image.states.insert("unknown".into(), Default::default());
        assert!(image.validate(&options, RecoveryReady::Ready).is_err());
        image.states.remove("unknown");
        image.generations.targets.insert("_recovery".into(), "generation.2".into());
        assert!(image.validate(&options, RecoveryReady::Ready).is_err());
        image.generations.targets.remove("_recovery");
        image.configurations.insert("unknown".into(), serde_json::Value::Null);
        assert!(image.validate(&options, RecoveryReady::Ready).is_err());
        image.configurations.remove("unknown");
        image.states.get_mut("a").unwrap().insert("large".into(), serde_json::json!("x".repeat(1100)));
        image.validate(&options, RecoveryReady::Ready).expect("model admission incorrectly reused the retained payload limit");
        image.states.get_mut("a").unwrap().insert("large".into(), serde_json::json!("x".repeat(crate::session::MAX_PACKET_SIZE as usize)));
        assert!(image.validate(&options, RecoveryReady::Ready).is_err(), "oversized runtime image passed model admission");
        image.states.clear();
        image.configurations.clear();
        assert!(image.validate(&options, RecoveryReady::RecoveryOnly).is_err());
        image.generations.targets.clear();
        image.validate(&options, RecoveryReady::RecoveryOnly).unwrap();
    }
}
