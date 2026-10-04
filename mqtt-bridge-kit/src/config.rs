//! The command line (Bridge Runtime spec §5, §6): `--root`, `--instance`, `--broker host[:port]`,
//! `--log-config`, an override for each of the driver's timings, the MQTT side's timings, and the
//! options a bridge declares for its driver (`Bridge::option`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::driver::Timing;
use crate::session::MqttTiming;

/// Overrides of the driver's timing defaults (`DriverInfo::timing`), from the command line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimingOverrides {
    pub connect: Option<Duration>,
    pub operation: Option<Duration>,
    pub retry: Option<Duration>,
    pub outage_warn: Option<Duration>,
    /// `Some(None)`: no polling.
    pub poll: Option<Option<Duration>>,
    pub probe_interval: Option<Duration>,
    pub probe_deadline: Option<Duration>,
}

impl TimingOverrides {
    /// The driver's timing with these overrides.
    pub fn apply(&self, timing: Timing) -> Timing {
        Timing {
            connect: self.connect.unwrap_or(timing.connect),
            operation: self.operation.unwrap_or(timing.operation),
            retry: self.retry.unwrap_or(timing.retry),
            outage_warn: self.outage_warn.unwrap_or(timing.outage_warn),
            poll: self.poll.unwrap_or(timing.poll),
            probe_interval: self.probe_interval.unwrap_or(timing.probe_interval),
            probe_deadline: self.probe_deadline.unwrap_or(timing.probe_deadline),
        }
    }
}

/// Everything the command line sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub root: String,
    pub instance: Option<String>,
    /// `host` or `host:port` (default port 1883).
    pub broker: Option<String>,
    pub log_config: Option<PathBuf>,
    pub timing: TimingOverrides,
    pub mqtt: MqttTiming,
    /// The driver's options, by name.
    pub options: BTreeMap<String, String>,
}

impl Settings {
    pub fn new(family: &str) -> Settings {
        Settings {
            root: family.to_owned(),
            instance: None,
            broker: None,
            log_config: None,
            timing: TimingOverrides::default(),
            mqtt: MqttTiming::default(),
            options: BTreeMap::new(),
        }
    }
}

/// Why the command line does not start a bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// `--help`.
    Help,
    /// A usage error, said.
    Usage(String),
}

/// A duration: `500ms`, `10s`, `2m`, or a bare number of seconds.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let bad = || format!("`{text}` is not a duration (`500ms`, `10s`, `2m`, or seconds)");
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => text.split_at(i),
        None => (text, "s"),
    };
    let n: u64 = number.parse().map_err(|_| bad())?;
    match unit {
        "ms" => Ok(Duration::from_millis(n)),
        "s" => Ok(Duration::from_secs(n)),
        "m" => n.checked_mul(60).map(Duration::from_secs).ok_or_else(|| format!("`{text}` is too long a duration")),
        _ => Err(bad()),
    }
}

/// The smallest MQTT keep-alive rumqttc takes (it asserts it).
pub const MIN_KEEP_ALIVE: Duration = Duration::from_secs(5);

/// Timings that cannot run: a zero retry (an unpaced reconnect loop), a zero probe interval (a
/// probe loop), a keep-alive under rumqttc's floor (its assertion would end the MQTT task). Checked
/// at parse time and again for the timings set in code ([`crate::Bridge::start`]).
pub fn check_timing(timing: &Timing, mqtt: &MqttTiming) -> Result<(), String> {
    if timing.retry.is_zero() {
        return Err("the device retry interval is zero: an unpaced reconnect loop".into());
    }
    if timing.probe_interval.is_zero() {
        return Err("the probe interval is zero: a probe loop".into());
    }
    if mqtt.keep_alive < MIN_KEEP_ALIVE {
        return Err(format!("the MQTT keep-alive is under {} s, rumqttc's floor", MIN_KEEP_ALIVE.as_secs()));
    }
    Ok(())
}

/// `host` or `host:port`, the port defaulting to 1883.
pub fn broker_host_port(broker: &str) -> Result<(String, u16), String> {
    match broker.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port = port.parse().map_err(|_| format!("`{broker}`: `{port}` is not a port"))?;
            Ok((host.to_owned(), port))
        }
        Some(_) => Err(format!("`{broker}` names no host")),
        None if broker.is_empty() => Err("the broker names no host".into()),
        None => Ok((broker.to_owned(), 1883)),
    }
}

/// The usage text, for the family and the driver's declared options.
pub fn usage(program: &str, family: &str, options: &[(String, String)]) -> String {
    let mut text = format!(
        "Usage: {program} --instance <name> --broker <host[:port]> [options]\n\n\
         The {family} bridge, on the Bridge Runtime (mqtt-bridge-kit).\n\n\
         Options:\n\
         \x20 --root <Root>                MQTT root (default: {family})\n\
         \x20 --instance <name>            this bridge's instance\n\
         \x20 --broker <host[:port]>       the MQTT broker (port 1883 by default)\n\
         \x20 --log-config <path>          tracing-init's logging.toml\n\
         \x20 --connect-timeout <d>        the bound on a device connect\n\
         \x20 --operation-timeout <d>      the bound on every other device operation\n\
         \x20 --retry <d>                  at most one device connect per this\n\
         \x20 --outage-warn <d>            a device outage this long is one WARN\n\
         \x20 --poll <d|off>               read the device this often (0 or off: never)\n\
         \x20 --probe-interval <d>         probe a silent unacknowledged device this often\n\
         \x20 --probe-deadline <d>         the bound on a probe\n\
         \x20 --mqtt-keep-alive <d>        MQTT keep-alive (at least 5s)\n\
         \x20 --mqtt-retry <d>             between MQTT sessions\n\
         \x20 --mqtt-warn <d>              a broker outage this long is one WARN\n\
         \x20 --mqtt-stable <d>            a connection that lasts this long ends a broker outage\n"
    );
    for (name, help) in options {
        text.push_str(&format!("  --{name:<27} {help}\n"));
    }
    text.push_str("  --help                       this text\n\nA duration <d>: 500ms, 10s, 2m, or seconds.\n");
    text
}

/// Parse `args` (without the program name) over `settings`; `options` are the driver's declared
/// option names.
pub fn parse(args: impl IntoIterator<Item = String>, settings: &mut Settings, options: &[(String, String)]) -> Result<(), Problem> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Err(Problem::Help);
        }
        let Some(flag) = arg.strip_prefix("--") else {
            return Err(Problem::Usage(format!("unexpected argument `{arg}`")));
        };
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (flag.to_owned(), None),
        };
        let value = match inline.or_else(|| args.next()) {
            Some(value) => value,
            None => return Err(Problem::Usage(format!("`--{name}` needs a value"))),
        };
        let duration = || parse_duration(&value).map_err(Problem::Usage);
        let nonzero = || match duration()? {
            d if d.is_zero() => Err(Problem::Usage(format!("`--{name}` cannot be zero"))),
            d => Ok(d),
        };
        match name.as_str() {
            "root" => settings.root = value,
            "instance" => settings.instance = Some(value),
            "broker" => settings.broker = Some(value),
            "log-config" => settings.log_config = Some(PathBuf::from(value)),
            "connect-timeout" => settings.timing.connect = Some(duration()?),
            "operation-timeout" => settings.timing.operation = Some(duration()?),
            "retry" => settings.timing.retry = Some(nonzero()?),
            "outage-warn" => settings.timing.outage_warn = Some(duration()?),
            "poll" if value == "off" => settings.timing.poll = Some(None),
            "poll" => settings.timing.poll = Some(Some(duration()?).filter(|d| !d.is_zero())),
            "probe-interval" => settings.timing.probe_interval = Some(nonzero()?),
            "probe-deadline" => settings.timing.probe_deadline = Some(duration()?),
            "mqtt-keep-alive" => match duration()? {
                d if d < MIN_KEEP_ALIVE => {
                    return Err(Problem::Usage(format!("`--mqtt-keep-alive` is at least {} s (rumqttc's floor)", MIN_KEEP_ALIVE.as_secs())))
                }
                d => settings.mqtt.keep_alive = d,
            },
            "mqtt-retry" => settings.mqtt.retry = duration()?,
            "mqtt-warn" => settings.mqtt.warn_after = duration()?,
            "mqtt-stable" => settings.mqtt.stable_after = duration()?,
            other if options.iter().any(|(o, _)| o == other) => {
                settings.options.insert(other.to_owned(), value);
            }
            other => return Err(Problem::Usage(format!("unknown option `--{other}`"))),
        }
    }
    if settings.instance.is_none() {
        return Err(Problem::Usage("`--instance` is required".into()));
    }
    match &settings.broker {
        None => return Err(Problem::Usage("`--broker` is required".into())),
        Some(broker) => {
            broker_host_port(broker).map_err(Problem::Usage)?;
        }
    }
    crate::topics::Topics::new(&settings.root, settings.instance.as_deref().unwrap_or_default()).map_err(Problem::Usage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("10s"), Ok(Duration::from_secs(10)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_duration("4"), Ok(Duration::from_secs(4)));
        assert!(parse_duration("4h").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn the_command_line_sets_the_topics_the_broker_and_each_timing() {
        let mut s = Settings::new("Aircondition");
        let options = vec![("coolmaster".to_owned(), "its address".to_owned())];
        parse(
            args("--instance ctrl --broker 10.0.0.1:1884 --root Test --poll off --retry 2s --operation-timeout=300ms --coolmaster 10.0.1.70 --mqtt-retry 1s"),
            &mut s,
            &options,
        )
        .unwrap();
        assert_eq!((s.root.as_str(), s.instance.as_deref(), s.broker.as_deref()), ("Test", Some("ctrl"), Some("10.0.0.1:1884")));
        assert_eq!(s.timing.poll, Some(None));
        assert_eq!(s.timing.retry, Some(Duration::from_secs(2)));
        assert_eq!(s.timing.operation, Some(Duration::from_millis(300)));
        assert_eq!(s.options.get("coolmaster").map(String::as_str), Some("10.0.1.70"));
        assert_eq!(s.mqtt.retry, Duration::from_secs(1));
        let timing = s.timing.apply(Timing::default());
        assert_eq!((timing.retry, timing.poll, timing.connect), (Duration::from_secs(2), None, Duration::from_secs(10)));
    }

    #[test]
    fn a_usage_error_is_said() {
        let mut s = Settings::new("A");
        assert_eq!(parse(args("--help"), &mut s, &[]), Err(Problem::Help));
        assert!(matches!(parse(args("--broker x"), &mut Settings::new("A"), &[]), Err(Problem::Usage(m)) if m.contains("--instance")));
        assert!(matches!(parse(args("--instance i"), &mut Settings::new("A"), &[]), Err(Problem::Usage(m)) if m.contains("--broker")));
        assert!(
            matches!(parse(args("--instance i --broker x --nope 1"), &mut Settings::new("A"), &[]), Err(Problem::Usage(m)) if m.contains("--nope"))
        );
        assert!(matches!(parse(args("--instance a/b --broker x"), &mut Settings::new("A"), &[]), Err(Problem::Usage(_))));
        assert!(matches!(parse(args("--instance i --broker x:port"), &mut Settings::new("A"), &[]), Err(Problem::Usage(_))));
    }

    /// Durations that would make a hot loop, trip rumqttc's assertion or overflow are usage errors
    /// at parse time, exit 2; a zero poll in any unit is off (review MINOR 2).
    #[test]
    fn durations_out_of_range_are_usage_errors() {
        let usage = |line: &str| matches!(parse(args(line), &mut Settings::new("A"), &[]), Err(Problem::Usage(_)));
        for bad in [
            "--retry 0",
            "--retry 0ms",
            "--probe-interval 0",
            "--mqtt-keep-alive 4s",
            "--mqtt-keep-alive 4999ms",
            "--connect-timeout 18446744073709551615m",
        ] {
            assert!(usage(&format!("--instance i --broker x {bad}")), "`{bad}` was accepted");
        }
        assert!(parse_duration("18446744073709551615m").is_err(), "a minute count that overflows was accepted");
        let mut s = Settings::new("A");
        parse(args("--instance i --broker x --poll 0ms --mqtt-keep-alive 5s --retry 1ms"), &mut s, &[]).unwrap();
        assert_eq!(s.timing.poll, Some(None), "`--poll 0ms` did not turn polling off");
        assert_eq!(s.mqtt.keep_alive, Duration::from_secs(5));
        // Set in code, the same timings are refused when the bridge starts.
        let zero_retry = Timing { retry: Duration::ZERO, ..Timing::default() };
        assert!(check_timing(&zero_retry, &MqttTiming::default()).is_err());
        let short = MqttTiming { keep_alive: Duration::from_secs(1), ..MqttTiming::default() };
        assert!(check_timing(&Timing::default(), &short).is_err());
        assert_eq!(check_timing(&Timing::default(), &MqttTiming::default()), Ok(()));
    }

    #[test]
    fn the_broker_defaults_to_port_1883() {
        assert_eq!(broker_host_port("host"), Ok(("host".into(), 1883)));
        assert_eq!(broker_host_port("host:1884"), Ok(("host".into(), 1884)));
        assert!(broker_host_port(":1").is_err());
    }
}
