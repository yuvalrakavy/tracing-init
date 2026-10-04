//! A scriptable fake device, as a [`DeviceDriver`]: up and down, values changed at the device,
//! answers scripted per operation (a lost link, a refusal, an unusable reply, unsupported, a delay,
//! a call that never returns, a panic, a call that blocks its thread), a deaf device that takes
//! writes without acting on them, and a log of every call.
//!
//! [`fake_device`] returns the driver, for the bridge, and a [`Device`] handle, for the test.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mqtt_bridge_kit::{
    CommandCall, DeviceDriver, DriverInfo, LinkContext, LinkError, LinkHandle, OpError, PropertyMap, Scope, TargetState, Value,
};
use serde_json::json;

/// A driver operation, as the fake device logs it and a rule matches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Connect,
    Disconnect,
    Read,
    Apply,
    Execute,
    Configure,
    Probe,
}

/// One call the bridge made.
#[derive(Debug, Clone)]
pub struct Call {
    pub op: Op,
    pub target: Option<String>,
    /// `apply`'s values, `configure`'s config, `execute`'s parameters.
    pub values: Option<Value>,
    pub command: Option<String>,
    pub at: Instant,
}

/// How a scripted call answers, in place of the device's own answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Link,
    Rejected,
    Unusable,
    Unsupported,
    /// Never returns (until the bridge drops the call at its deadline).
    Hang,
    /// The device's own answer, this much later.
    Delay(Duration),
    /// The driver panics (a bug).
    Panic,
    /// The driver blocks its thread this long — a synchronous call with no timeout — and then
    /// answers as the device does (a bug the runtime's watchdog catches).
    Block(Duration),
}

/// A helper the fake driver starts through its link's context at its next connect, as a driver
/// starts an unsolicited-input reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    /// Ends this long after the connect, as a reader that reads end of file and gives up without
    /// saying the link is lost.
    ReturnAfter(Duration),
    /// Panics this long after the connect (a bug).
    PanicAfter(Duration),
}

#[derive(Debug, Clone)]
struct Rule {
    op: Op,
    target: Option<String>,
    answer: Answer,
    remaining: Option<usize>,
}

#[derive(Debug, Default)]
struct State {
    up: bool,
    deaf: bool,
    connected: bool,
    targets: BTreeMap<String, PropertyMap>,
    calls: Vec<Call>,
    rules: Vec<Rule>,
    link: Option<LinkHandle>,
    connect_cap: Option<usize>,
    helper: Option<Helper>,
}

impl State {
    /// The scripted answer for `op` on `target`, counted down.
    fn rule(&mut self, op: Op, target: Option<&str>) -> Option<Answer> {
        let i = self.rules.iter().position(|r| r.op == op && (r.target.is_none() || r.target.as_deref() == target))?;
        let answer = self.rules[i].answer;
        if let Some(remaining) = &mut self.rules[i].remaining {
            *remaining -= 1;
            if *remaining == 0 {
                self.rules.remove(i);
            }
        }
        Some(answer)
    }
}

/// The test's handle on the fake device.
#[derive(Debug, Clone)]
pub struct Device {
    shared: Arc<Mutex<State>>,
}

/// The driver given to the bridge.
#[derive(Debug)]
pub struct FakeDevice {
    shared: Arc<Mutex<State>>,
    info: DriverInfo,
}

/// A fake device declaring `info`, up, with no targets yet.
pub fn fake_device(info: DriverInfo) -> (FakeDevice, Device) {
    let shared = Arc::new(Mutex::new(State { up: true, ..State::default() }));
    (FakeDevice { shared: shared.clone(), info }, Device { shared })
}

impl Device {
    fn locked(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Bring the device up, or take it down: a device taken down drops its link at once, as a
    /// reader helper seeing end of file says.
    pub fn set_up(&self, up: bool) {
        let link = {
            let mut state = self.locked();
            state.up = up;
            if up {
                None
            } else {
                state.link.take()
            }
        };
        if let Some(link) = link {
            link.link_lost("the fake device went down");
        }
    }

    /// Take writes without acting on them, and leave probes unanswered — a processor that lost
    /// power behind an adapter that keeps the link up.
    pub fn set_deaf(&self, deaf: bool) {
        self.locked().deaf = deaf;
    }

    /// Set a value at the device, as a person at the wall does.
    pub fn set(&self, target: &str, property: &str, value: Value) {
        self.locked().targets.entry(target.to_owned()).or_default().insert(property.to_owned(), value);
    }

    /// A value at the device.
    pub fn get(&self, target: &str, property: &str) -> Option<Value> {
        self.locked().targets.get(target).and_then(|t| t.get(property)).cloned()
    }

    /// Answer every call of `op` (on `target`, or any) so, from now on.
    pub fn answer(&self, op: Op, target: Option<&str>, answer: Answer) {
        self.locked().rules.push(Rule { op, target: target.map(str::to_owned), answer, remaining: None });
    }

    /// Answer the next `n` calls of `op` (on `target`, or any) so.
    pub fn answer_times(&self, op: Op, target: Option<&str>, answer: Answer, n: usize) {
        self.locked().rules.push(Rule { op, target: target.map(str::to_owned), answer, remaining: Some(n) });
    }

    /// Answer as the device does again.
    pub fn answer_normally(&self) {
        self.locked().rules.clear();
    }

    /// Start `helper` at the next connect (once).
    pub fn helper(&self, helper: Helper) {
        self.locked().helper = Some(helper);
    }

    /// Refuse every connect past `cap` connects in all: a bridge that reconnects in a loop is
    /// caught after a few, not thousands.
    pub fn cap_connects(&self, cap: usize) {
        self.locked().connect_cap = Some(cap);
    }

    /// Every call so far.
    pub fn calls(&self) -> Vec<Call> {
        self.locked().calls.clone()
    }

    /// The calls of `op` so far.
    pub fn calls_of(&self, op: Op) -> Vec<Call> {
        self.calls().into_iter().filter(|c| c.op == op).collect()
    }

    /// The values of every `apply` so far, by target.
    pub fn applies(&self) -> Vec<(String, Value)> {
        self.calls_of(Op::Apply).into_iter().map(|c| (c.target.unwrap_or_default(), c.values.unwrap_or(Value::Null))).collect()
    }

    /// Whether the device holds a link now (between a connect and a disconnect).
    pub fn connected(&self) -> bool {
        self.locked().connected
    }

    /// Report an event through the current link's reporter, as a helper reading the device's
    /// unsolicited input does. `false` with no link.
    pub fn event(&self, event: Value) -> bool {
        let link = self.locked().link.clone();
        match link {
            Some(link) => {
                link.report().event(event);
                true
            }
            None => false,
        }
    }

    /// Report a target's state through the current link's reporter, as a helper does.
    pub fn report(&self, target: &str, values: PropertyMap) -> bool {
        let link = self.locked().link.clone();
        match link {
            Some(link) => {
                link.report().state(target, values, false);
                true
            }
            None => false,
        }
    }
}

impl FakeDevice {
    fn locked(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn log(&self, op: Op, target: Option<&str>, values: Option<Value>, command: Option<&str>) -> Option<Answer> {
        let mut state = self.locked();
        state.calls.push(Call { op, target: target.map(str::to_owned), values, command: command.map(str::to_owned), at: Instant::now() });
        state.rule(op, target)
    }

    /// The scripted answer, played: an error, a hang, or (after a delay) the device's own.
    async fn play(answer: Option<Answer>) -> Result<(), OpError> {
        match answer {
            None => Ok(()),
            Some(Answer::Link) => Err(OpError::Link(LinkError::new("scripted: the link failed"))),
            Some(Answer::Rejected) => Err(OpError::Rejected("scripted: the device refused it".into())),
            Some(Answer::Unusable) => Err(OpError::Unusable("scripted: an answer that cannot be parsed".into())),
            Some(Answer::Unsupported) => Err(OpError::Unsupported("scripted: no such thing".into())),
            Some(Answer::Hang) => std::future::pending().await,
            Some(Answer::Delay(d)) => {
                tokio::time::sleep(d).await;
                Ok(())
            }
            Some(Answer::Panic) => panic!("the fake driver's scripted panic"),
            Some(Answer::Block(d)) => {
                std::thread::sleep(d);
                Ok(())
            }
        }
    }

    /// The device must be up and linked.
    fn reachable(&self) -> Result<(), OpError> {
        let state = self.locked();
        if state.up && state.connected {
            Ok(())
        } else {
            Err(OpError::Link(LinkError::new("the fake device is down")))
        }
    }
}

impl DeviceDriver for FakeDevice {
    fn info(&self) -> DriverInfo {
        self.info.clone()
    }

    async fn connect(&mut self, ctx: &mut LinkContext) -> Result<(), LinkError> {
        let answer = self.log(Op::Connect, None, None, None);
        FakeDevice::play(answer).await.map_err(|e| match e {
            OpError::Link(e) => e,
            other => LinkError::new(other.to_string()),
        })?;
        let mut state = self.locked();
        let connects = state.calls.iter().filter(|c| c.op == Op::Connect).count();
        if state.connect_cap.is_some_and(|cap| connects > cap) {
            return Err(LinkError::new("the fake device's connect cap was reached"));
        }
        if !state.up {
            return Err(LinkError::new("the fake device is down: connection refused"));
        }
        state.connected = true;
        state.link = Some(ctx.handle());
        if let Some(helper) = state.helper.take() {
            ctx.spawn(async move {
                match helper {
                    Helper::ReturnAfter(after) => tokio::time::sleep(after).await,
                    Helper::PanicAfter(after) => {
                        tokio::time::sleep(after).await;
                        panic!("the fake driver's helper's scripted panic");
                    }
                }
            });
        }
        Ok(())
    }

    async fn disconnect(&mut self) {
        let _ = self.log(Op::Disconnect, None, None, None);
        let mut state = self.locked();
        state.connected = false;
        state.link = None;
    }

    async fn read_state(&mut self, scope: Scope) -> Result<Vec<TargetState>, OpError> {
        let target = match &scope {
            Scope::Target(t) => Some(t.clone()),
            _ => None,
        };
        let answer = self.log(Op::Read, target.as_deref(), None, None);
        FakeDevice::play(answer).await?;
        self.reachable()?;
        let state = self.locked();
        if state.deaf {
            return Err(OpError::Link(LinkError::new("the fake device left the read unanswered")));
        }
        Ok(state
            .targets
            .iter()
            .filter(|(t, _)| target.as_ref().map_or(true, |wanted| wanted == *t))
            .map(|(t, values)| TargetState::new(t.clone(), values.clone()))
            .collect())
    }

    async fn apply(&mut self, target: &str, values: &PropertyMap) -> Result<(), OpError> {
        let answer = self.log(Op::Apply, Some(target), Some(Value::Object(values.clone())), None);
        FakeDevice::play(answer).await?;
        self.reachable()?;
        let mut state = self.locked();
        if !state.deaf {
            let entry = state.targets.entry(target.to_owned()).or_default();
            for (property, value) in values {
                entry.insert(property.clone(), value.clone());
            }
        }
        Ok(())
    }

    async fn execute(&mut self, target: &str, call: &CommandCall) -> Result<Option<Value>, OpError> {
        let answer = self.log(Op::Execute, Some(target), Some(Value::Object(call.params.clone())), Some(&call.command));
        FakeDevice::play(answer).await?;
        self.reachable()?;
        Ok(Some(json!({ "executed": call.command })))
    }

    async fn configure(&mut self, target: &str, config: Option<&Value>) -> Result<(), OpError> {
        let answer = self.log(Op::Configure, Some(target), config.cloned(), None);
        FakeDevice::play(answer).await?;
        self.reachable()
    }

    async fn probe(&mut self) -> Result<(), OpError> {
        let answer = self.log(Op::Probe, None, None, None);
        FakeDevice::play(answer).await?;
        self.reachable()?;
        if self.locked().deaf {
            return Err(OpError::Link(LinkError::new("the fake device left the probe unanswered")));
        }
        Ok(())
    }
}
