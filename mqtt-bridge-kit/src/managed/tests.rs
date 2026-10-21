use super::*;
use crate::{CommandCall, DriverInfo, LinkContext, LinkError, OpError, PropertyMap, Scope, TargetState};
use mqtt_test_broker::FakeBroker;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

struct Driver;
impl DeviceDriver for Driver {
    fn info(&self) -> DriverInfo {
        DriverInfo::default()
    }
    async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> {
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        Ok(vec![])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<serde_json::Value>, OpError> {
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&serde_json::Value>) -> Result<(), OpError> {
        Ok(())
    }
}
struct Live(Arc<AtomicBool>);
impl Drop for Live {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
struct Healthy {
    live: Arc<AtomicBool>,
}
impl ProcessService for Healthy {
    type Driver = Driver;
    async fn run(self, mut ctx: ServiceContext<Driver>) -> Result<(), String> {
        let _live = Live(self.live.clone());
        self.live.store(true, Ordering::SeqCst);
        ctx.ready(Driver)?;
        ctx.cancelled().await;
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_process_service_stays_live_while_the_broker_refuses_connections_and_closes_under_retained_handles() {
    let broker = FakeBroker::start().await;
    broker.refuse_connections(true);
    let live = Arc::new(AtomicBool::new(false));
    let running = Bridge::new("Managed")
        .instance("controller")
        .broker(&broker.address())
        .start_managed(Healthy { live: live.clone() })
        .await
        .expect("managed service did not reach readiness independently of MQTT");
    assert!(live.load(Ordering::SeqCst));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(live.load(Ordering::SeqCst), "broker outage canceled the process service");
    assert!(running.stop().await, "managed service failed bounded stop");
    assert!(!live.load(Ordering::SeqCst), "successful managed stop retained service-owned resources");
}

struct NeverReady {
    live: Arc<AtomicBool>,
}
impl ProcessService for NeverReady {
    type Driver = Driver;
    async fn run(self, mut ctx: ServiceContext<Driver>) -> Result<(), String> {
        let _live = Live(self.live.clone());
        self.live.store(true, Ordering::SeqCst);
        ctx.cancelled().await;
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_without_readiness_is_bounded_and_tears_down_service_resources() {
    let broker = FakeBroker::start().await;
    let live = Arc::new(AtomicBool::new(false));
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        Bridge::new("Managed").instance("controller").broker(&broker.address()).start_managed_with(
            NeverReady { live: live.clone() },
            ServiceTiming { startup: Duration::from_millis(50), stop: Duration::from_millis(100) },
        ),
    )
    .await
    .expect("managed startup had no parent-side bound");
    assert!(result.is_err());
    assert!(!live.load(Ordering::SeqCst), "timed-out startup retained service resources");
}

struct FailsOnSignal {
    signal: Arc<tokio::sync::Notify>,
    panic: bool,
}
impl ProcessService for FailsOnSignal {
    type Driver = Driver;
    async fn run(self, mut ctx: ServiceContext<Driver>) -> Result<(), String> {
        ctx.ready(Driver)?;
        self.signal.notified().await;
        assert!(!self.panic, "managed service test panic");
        Err("managed service test failure".into())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unpolled_embedded_service_failure_closes_mqtt_and_retains_its_fatal_reason() {
    let broker = FakeBroker::start().await;
    let signal = Arc::new(tokio::sync::Notify::new());
    let mut running = Bridge::new("Managed")
        .instance("controller")
        .broker(&broker.address())
        .start_managed(FailsOnSignal { signal: signal.clone(), panic: false })
        .await
        .unwrap();
    assert!(
        broker
            .wait_until(Duration::from_secs(2), |b| b
                .received_on("Managed/Active/controller")
                .iter()
                .any(|r| r.payload.as_ref() == b"true"))
            .await
    );
    signal.notify_one();
    assert!(
        broker
            .wait_until(Duration::from_secs(3), |b| b
                .received_on("Managed/Active/controller")
                .iter()
                .any(|r| r.payload.as_ref() == b"false"))
            .await,
        "unpolled service failure left MQTT admission live"
    );
    let died = tokio::time::timeout(Duration::from_secs(1), running.ended()).await.unwrap();
    assert_eq!(died.part, "service");
    assert!(died.cause.contains("managed service test failure"));
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_ready_service_panic_is_a_persistent_component_death() {
    let broker = FakeBroker::start().await;
    let signal = Arc::new(tokio::sync::Notify::new());
    let mut running = Bridge::new("Managed")
        .instance("controller")
        .broker(&broker.address())
        .start_managed(FailsOnSignal { signal: signal.clone(), panic: true })
        .await
        .unwrap();
    signal.notify_one();
    let died = tokio::time::timeout(Duration::from_secs(2), running.ended()).await.unwrap();
    assert_eq!(died.part, "service");
    assert!(died.cause.contains("panicked"));
    running.stop().await;
}

struct IgnoresStop {
    live: Arc<AtomicBool>,
}
impl ProcessService for IgnoresStop {
    type Driver = Driver;
    async fn run(self, mut ctx: ServiceContext<Driver>) -> Result<(), String> {
        let _live = Live(self.live.clone());
        self.live.store(true, Ordering::SeqCst);
        ctx.ready(Driver)?;
        std::future::pending().await
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignored_soft_stop_is_forced_and_success_requires_resource_release() {
    let broker = FakeBroker::start().await;
    broker.refuse_connections(true);
    let live = Arc::new(AtomicBool::new(false));
    let running = Bridge::new("Managed")
        .instance("controller")
        .broker(&broker.address())
        .start_managed_with(
            IgnoresStop { live: live.clone() },
            ServiceTiming { startup: Duration::from_secs(1), stop: Duration::from_millis(50) },
        )
        .await
        .unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(1), running.stop()).await.expect("ignored cooperative stop held shutdown"));
    assert!(!live.load(Ordering::SeqCst), "force flag was mistaken for completed teardown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_startup_future_cancels_the_already_running_service() {
    let broker = FakeBroker::start().await;
    let live = Arc::new(AtomicBool::new(false));
    let started = tokio::spawn(
        Bridge::new("Managed").instance("controller").broker(&broker.address()).start_managed(NeverReady { live: live.clone() }),
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while !live.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    started.abort();
    let _ = started.await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while live.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("canceled startup orphaned its service runtime");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_static_settings_and_service_bounds_never_execute_service_code() {
    let live = Arc::new(AtomicBool::new(false));
    let result = Bridge::new("Managed").broker("127.0.0.1:9").start_managed(Healthy { live: live.clone() }).await;
    assert!(matches!(result,Err(ref e) if e.contains("instance")));
    assert!(!live.load(Ordering::SeqCst));
    let result = Bridge::new("Managed")
        .instance("controller")
        .broker("127.0.0.1:9")
        .start_managed_with(Healthy { live: live.clone() }, ServiceTiming { startup: Duration::ZERO, stop: Duration::from_secs(1) })
        .await;
    assert!(matches!(result,Err(ref e) if e.contains("service bounds")));
    assert!(!live.load(Ordering::SeqCst));
}

struct Blocked {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    on_drop: bool,
}
impl Drop for Blocked {
    fn drop(&mut self) {
        if self.on_drop {
            block(&self.entered, &self.release);
        }
    }
}
fn block(entered: &AtomicBool, release: &AtomicBool) {
    entered.store(true, Ordering::SeqCst);
    // A safety valve for failed tests; assertions require release before this expires.
    let until = Instant::now() + Duration::from_secs(15);
    while !release.load(Ordering::SeqCst) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
}
impl ProcessService for Blocked {
    type Driver = Driver;
    async fn run(self, mut ctx: ServiceContext<Driver>) -> Result<(), String> {
        ctx.ready(Driver)?;
        if self.on_drop {
            ctx.cancelled().await;
        } else {
            block(&self.entered, &self.release);
            ctx.cancelled().await;
        }
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_immediately_after_readiness_is_detected_without_the_services_heartbeat_task_running() {
    let broker = FakeBroker::start().await;
    broker.refuse_connections(true);
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let mut running = Bridge::new("Managed")
        .instance("controller")
        .broker(&broker.address())
        .start_managed(Blocked { entered: entered.clone(), release: release.clone(), on_drop: false })
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(8), running.ended()).await;
    let held = entered.load(Ordering::SeqCst) && !release.load(Ordering::SeqCst);
    release.store(true, Ordering::SeqCst);
    let _ = running.stop().await;
    assert!(held, "the test never blocked the service thread");
    let died = result.expect("blocked service escaped the independent watchdog");
    assert_eq!(died.part, "service");
    assert!(died.cause.contains("service worker"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_resource_destruction_does_not_publish_completion_or_hold_the_parent_past_its_ceiling() {
    let broker = FakeBroker::start().await;
    broker.refuse_connections(true);
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let running = Bridge::new("Managed")
        .instance("controller")
        .broker(&broker.address())
        .start_managed(Blocked { entered: entered.clone(), release: release.clone(), on_drop: true })
        .await
        .unwrap();
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(7), running.stop()).await;
    let blocked = entered.load(Ordering::SeqCst);
    release.store(true, Ordering::SeqCst);
    assert!(blocked, "the resource destructor was never entered");
    assert!(!result.expect("blocked destructor held the parent shutdown"), "force cancellation was mistaken for completed destruction");
    assert!(started.elapsed() < Duration::from_secs(6), "shutdown received sequential budgets");
}

struct RetainsContext {
    retained: oneshot::Sender<ServiceContext<Driver>>,
}
impl ProcessService for RetainsContext {
    type Driver = Driver;
    async fn run(self, ctx: ServiceContext<Driver>) -> Result<(), String> {
        let _ = self.retained.send(ctx);
        Err("failed while retaining context".into())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_worker_death_ends_startup_even_when_a_retained_context_keeps_readiness_open() {
    let (retained, receive) = oneshot::channel();
    let starting =
        tokio::spawn(Bridge::new("Managed").instance("controller").broker("127.0.0.1:9").start_managed(RetainsContext { retained }));
    let _context = receive.await.unwrap();
    let result = tokio::time::timeout(Duration::from_millis(500), starting).await;
    assert!(result.is_ok(), "a retained context hid the already-completed worker from startup");
    assert!(result.unwrap().unwrap().is_err());
}

struct BlockingHelper {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}
impl ProcessService for BlockingHelper {
    type Driver = Driver;
    async fn run(self, mut ctx: ServiceContext<Driver>) -> Result<(), String> {
        let entered = self.entered.clone();
        tokio::task::spawn_blocking(move || block(&entered, &self.release));
        while !self.entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        ctx.ready(Driver)?;
        ctx.cancelled().await;
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_blocking_helper_prevents_successful_service_teardown() {
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let bridge = Bridge::new("Managed").instance("controller").broker("127.0.0.1:9");
    let (managed, ready) = spawn(
        BlockingHelper { entered: entered.clone(), release: release.clone() },
        bridge.settings().clone(),
        ServiceTiming::default(),
        &StartupSlot::default(),
    )
    .unwrap();
    ready.await.unwrap();
    let closed = managed.stop_until(tokio::time::Instant::now() + Duration::from_millis(350)).await;
    let still_running = !release.load(Ordering::SeqCst);
    release.store(true, Ordering::SeqCst);
    drop(managed);
    assert!(still_running && entered.load(Ordering::SeqCst));
    assert!(!closed, "runtime timeout was mistaken for teardown of a running blocking helper");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn death_between_the_readiness_check_and_launch_prevents_all_worker_admission() {
    let bridge = Bridge::new("Managed").instance("controller").broker("127.0.0.1:9");
    let live = Arc::new(AtomicBool::new(false));
    let (managed, ready) = spawn(Healthy { live }, bridge.settings().clone(), ServiceTiming::default(), &StartupSlot::default()).unwrap();
    ready.await.unwrap();
    assert!(managed.already_failed().is_none());
    // Deterministically hold the old check-to-launch boundary until fatal wins.
    managed.control.fatal("death at launch boundary".into());
    let called = AtomicBool::new(false);
    let result: Result<(), _> = managed.admit(|| {
        called.store(true, Ordering::SeqCst);
        Err("launch should not run".into())
    });
    assert!(result.is_err());
    assert!(!called.load(Ordering::SeqCst), "new workers were admitted after service death");
    let _ = managed.stop_until(tokio::time::Instant::now() + Duration::from_secs(1)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_stop_cannot_extend_the_components_original_shutdown_deadline() {
    let bridge = Bridge::new("Managed").instance("controller").broker("127.0.0.1:9");
    let live = Arc::new(AtomicBool::new(false));
    let (managed, ready) = spawn(Healthy { live }, bridge.settings().clone(), ServiceTiming::default(), &StartupSlot::default()).unwrap();
    ready.await.unwrap();
    let original = managed.control.deadline(tokio::time::Instant::now() + Duration::from_millis(20));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(managed.shutdown_deadline(), original, "explicit stop renewed the fatal component budget");
    let _ = managed.stop_until(original).await;
}

struct DescribedDriver {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    invalid: bool,
    panic: bool,
}
impl DeviceDriver for DescribedDriver {
    fn info(&self) -> DriverInfo {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "service info panic");
        let mut info = DriverInfo::default();
        if self.invalid {
            info.timing.retry = Duration::ZERO;
        }
        info
    }
    async fn connect(&mut self, ctx: &mut LinkContext) -> Result<(), LinkError> {
        Driver.connect(ctx).await
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        Ok(vec![])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<serde_json::Value>, OpError> {
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&serde_json::Value>) -> Result<(), OpError> {
        Ok(())
    }
}
struct Describes {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    invalid: bool,
    panic: bool,
    duplicate_rejected: Arc<AtomicBool>,
}
impl ProcessService for Describes {
    type Driver = DescribedDriver;
    async fn run(self, mut ctx: ServiceContext<DescribedDriver>) -> Result<(), String> {
        let driver = || DescribedDriver { calls: self.calls.clone(), invalid: self.invalid, panic: self.panic };
        ctx.ready(driver())?;
        self.duplicate_rejected.store(ctx.ready(driver()).is_err(), Ordering::SeqCst);
        ctx.cancelled().await;
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_validates_info_once_and_rejects_duplicates_before_calling_info_again() {
    let broker = FakeBroker::start().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rejected = Arc::new(AtomicBool::new(false));
    let running = Bridge::new("Managed")
        .instance("controller")
        .broker(&broker.address())
        .start_managed(Describes { calls: calls.clone(), invalid: false, panic: false, duplicate_rejected: rejected.clone() })
        .await
        .unwrap();
    assert!(
        broker
            .wait_until(Duration::from_secs(2), |b| b
                .received_on("Managed/Active/controller")
                .iter()
                .any(|r| r.payload.as_ref() == b"true"))
            .await
    );
    assert!(rejected.load(Ordering::SeqCst));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "Host launch or duplicate readiness re-evaluated driver info");
    assert!(running.stop().await);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_or_panicking_service_driver_info_never_starts_mqtt() {
    for panic in [false, true] {
        let broker = FakeBroker::start().await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            Bridge::new("Managed").instance("controller").broker(&broker.address()).start_managed(Describes {
                calls: calls.clone(),
                invalid: !panic,
                panic,
                duplicate_rejected: Arc::new(AtomicBool::new(false)),
            }),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(broker.connections(), 0, "invalid service readiness started MQTT");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_stop_enforces_the_original_deadline_for_mqtt_even_after_service_completion() {
    let broker = FakeBroker::start().await;
    let running = Bridge::new("Managed")
        .instance("controller")
        .broker(&broker.address())
        .start_managed(Healthy { live: Arc::new(AtomicBool::new(false)) })
        .await
        .unwrap();
    let managed = running.managed.as_ref().unwrap();
    let deadline = managed.control.deadline(tokio::time::Instant::now() + Duration::from_millis(250));
    assert!(managed.stop_until(deadline).await, "service did not complete within the original budget");
    tokio::time::sleep_until(deadline + Duration::from_millis(10)).await;
    let started = Instant::now();
    assert!(!running.stop().await, "late MQTT/device shutdown was accepted after service completion");
    assert!(started.elapsed() < Duration::from_millis(200), "Running renewed the component stop budget");
}
