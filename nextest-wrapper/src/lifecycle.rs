use chrono::Utc;
use logmon_broker_sdk::Broker;
use serde_json::{json, Value};
use std::{
    env,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub struct Run {
    pub id: String,
    pub name: String,
    pub rerun: Option<String>,
    pub estimated_ms: Option<u64>,
    pub test_timeout_ms: Option<u64>,
    journal: Option<PathBuf>,
    breaker: Option<PathBuf>,
    domain: Option<String>,
    timeout: Duration,
    broker: Option<Broker>,
    refused: bool,
}

impl Run {
    pub fn current() -> Option<Self> {
        let id = env::var("STORE_TEST_LOG_RUN_ID")
            .ok()
            .filter(|s| !s.is_empty())?;
        let name = format!(
            "rust:{}::{}",
            env::var("NEXTEST_BINARY_ID").ok()?,
            env::var("NEXTEST_TEST_NAME").ok()?
        );
        let journal = env::var_os("STORE_TEST_LOG_EVENTS_PATH").map(PathBuf::from);
        let breaker = env::var_os("STORE_TEST_LOG_BREAKER_PATH")
            .map(PathBuf::from)
            .or_else(|| journal.as_ref().map(|p| p.with_extension("breaker")));
        let domain = env::var("STORE_TEST_LOG_DOMAIN").ok().or_else(|| {
            env::var("STORE_DEV_TRACK")
                .ok()
                .and_then(|t| t.parse::<u32>().ok())
                .map(|t| format!("t{t}"))
        });
        let timeout = env::var("STORE_TEST_LOG_ACK_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(100);
        let duration = |key: &str| -> Option<u64> {
            let bytes = std::fs::read(env::var_os(key)?).ok()?;
            serde_json::from_slice::<Value>(&bytes)
                .ok()?
                .get(&name)?
                .as_u64()
        };
        let estimated_ms = duration("STORE_TEST_LOG_ESTIMATES_PATH");
        let test_timeout_ms = duration("STORE_TEST_LOG_TIMEOUTS_PATH").filter(|v| *v > 0);
        Some(Self {
            estimated_ms,
            test_timeout_ms,
            id,
            name,
            rerun: env::var("STORE_TEST_LOG_RERUN_OF").ok(),
            journal,
            breaker,
            domain,
            timeout: Duration::from_millis(timeout),
            broker: None,
            refused: false,
        })
    }

    pub async fn report(&mut self, start: bool, event: &Value) {
        let before = Instant::now();
        let permit = if self.refused {
            None
        } else {
            self.breaker
                .as_deref()
                .map_or(Some(0), |p| permit(p, self.timeout).unwrap_or(Some(0)))
        };
        let (delivery, transport) = if self.refused {
            ("refused", "disabled")
        } else if let Some(generation) = permit {
            let request = async {
                if self.broker.is_none() {
                    let mut builder = Broker::connect().reconnect_max_attempts(0);
                    if let Some(domain) = &self.domain {
                        builder = builder.domain(domain);
                    }
                    self.broker = Some(builder.open().await?);
                }
                self.broker
                    .as_ref()
                    .unwrap()
                    .call(
                        if start {
                            "test_log.start"
                        } else {
                            "test_log.end"
                        },
                        event.clone(),
                    )
                    .await
            };
            let outcome =
                tokio::time::timeout(self.timeout.saturating_sub(before.elapsed()), request).await;
            let result = match outcome {
                Ok(Ok(reply))
                    if matches!(
                        reply["status"].as_str(),
                        Some("accepted" | "already_applied")
                    ) =>
                {
                    ("confirmed", "acknowledged")
                }
                Ok(Ok(reply)) if reply["status"] == "refused" => {
                    self.refused = matches!(
                        reply["code"].as_str(),
                        Some("wrong_run_id" | "missing_run_id" | "no_active_run" | "run_closed")
                    );
                    ("refused", "acknowledged")
                }
                Err(_) => ("unconfirmed", "timeout"),
                _ => ("unconfirmed", "transport_error"),
            };
            if result.0 == "unconfirmed" {
                self.broker = None;
            }
            if let Some(path) = &self.breaker {
                let _ = finish_probe(path, generation, result.0 != "unconfirmed");
            }
            result
        } else {
            ("unconfirmed", "breaker_open")
        };
        if let Some(path) = &self.journal {
            let mut row = json!({"members":[self.name],"transport":transport,"latency_us":before.elapsed().as_micros() as u64});
            if start {
                row["start"] = event.clone();
                row["start_delivery"] = delivery.into();
            } else {
                row["event"] = event.clone();
                row["delivery"] = delivery.into();
            }
            if let Err(e) = append(path, &row) {
                eprintln!("logmon-nextest-wrapper: journal {}: {e}", path.display());
            }
        }
    }
}

fn append(path: &Path, row: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(row)?;
    bytes.push(b'\n');
    // O_APPEND and a single write keep normal local-file appends contiguous
    // across concurrent wrappers. Report a partial append as a journal failure.
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    if f.write(&bytes)? != bytes.len() {
        return Err(io::ErrorKind::WriteZero.into());
    }
    Ok(())
}

// Only the short state read/replace holds the lock. A suspended or killed
// probe cannot strand the run; its lease expires after the RPC deadline.
struct State {
    generation: u64,
    next: i64,
    delay: u64,
}
fn update<T>(path: &Path, busy: T, f: impl FnOnce(&mut State) -> (T, bool)) -> io::Result<T> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    if let Err(e) = file.try_lock() {
        return match e {
            std::fs::TryLockError::WouldBlock => Ok(busy),
            std::fs::TryLockError::Error(e) => Err(e),
        };
    }
    let mut raw = String::new();
    (&mut file).take(128).read_to_string(&mut raw)?;
    let mut fields = raw.split_whitespace();
    let mut s = State {
        generation: fields.next().and_then(|x| x.parse().ok()).unwrap_or(0),
        next: fields.next().and_then(|x| x.parse().ok()).unwrap_or(0),
        delay: fields.next().and_then(|x| x.parse().ok()).unwrap_or(0),
    };
    let (result, changed) = f(&mut s);
    if changed {
        file.seek(SeekFrom::Start(0))?;
        let text = format!("{} {} {}\n", s.generation, s.next, s.delay);
        file.write_all(text.as_bytes())?;
        file.set_len(text.len() as u64)?;
    }
    Ok(result)
}

fn permit(path: &Path, timeout: Duration) -> io::Result<Option<u64>> {
    // Shared reads keep healthy parallel attempts independent. On contention
    // with the state writer skip this transmission; the original end is replayable.
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(0)),
        Err(e) => return Err(e),
    };
    let now = Utc::now().timestamp_millis();
    if file.try_lock_shared().is_err() {
        return Ok(None);
    }
    let mut raw = String::new();
    (&mut file).take(128).read_to_string(&mut raw)?;
    let mut fields = raw.split_whitespace();
    let generation = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let next = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    if next == 0 {
        return Ok(Some(generation));
    }
    if now < next {
        return Ok(None);
    }
    drop(file);
    update(path, None, |s| {
        if s.next == 0 {
            return (Some(s.generation), false);
        }
        if s.next > now {
            return (None, false);
        }
        s.generation = s.generation.saturating_add(1);
        s.next = now
            .saturating_add(i64::try_from(timeout.as_millis()).unwrap_or(i64::MAX))
            .saturating_add(1000);
        (Some(s.generation), true)
    })
}
fn finish_probe(path: &Path, generation: u64, acknowledged: bool) -> io::Result<()> {
    if generation == 0 && acknowledged {
        return Ok(());
    }
    update(path, (), |s| {
        if generation != s.generation {
            return ((), false);
        }
        if acknowledged {
            if s.next == 0 {
                return ((), false);
            }
            s.delay = 0;
            s.next = 0;
        } else {
            s.delay = if s.delay == 0 {
                5000
            } else {
                (s.delay * 2).min(30000)
            };
            s.next = Utc::now().timestamp_millis().saturating_add(s.delay as i64);
        }
        s.generation = s.generation.saturating_add(1);
        ((), true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_probe_recovers_the_shared_circuit_and_stale_outcomes_cannot_reopen_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("breaker");
        let ack = Duration::from_millis(100);
        let first = permit(&path, ack).unwrap().unwrap();
        let concurrent = permit(&path, ack).unwrap().unwrap();
        finish_probe(&path, first, false).unwrap();
        assert!(permit(&path, ack).unwrap().is_none());
        finish_probe(&path, concurrent, true).unwrap();
        assert!(permit(&path, ack).unwrap().is_none());

        update(&path, (), |s| {
            s.next = 1;
            ((), true)
        })
        .unwrap();
        let probe = permit(&path, ack).unwrap().unwrap();
        assert!(permit(&path, ack).unwrap().is_none());
        finish_probe(&path, probe, true).unwrap();
        assert!(permit(&path, ack).unwrap().is_some());
        finish_probe(&path, probe, false).unwrap();
        assert!(permit(&path, ack).unwrap().is_some());
    }
}
