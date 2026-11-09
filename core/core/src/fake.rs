//! Deterministic fake backend for lifecycle and queue contract tests.
//!
//! This backend does not execute JavaScript. It exercises the same worker,
//! queue, cancellation, deadline and close paths as the real backend without
//! compiling Deno/V8.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{json, Map, Value};

use crate::backend::{CreatedDriver, Driver, DriverControl, DriverFactory, Invocation};
use crate::config::{BackendKind, CloseReport, SandboxOptions};
use crate::plugin::PreparedPlugin;
use crate::{Error, Result};

/// Thread-safe fake cancellation flag.
#[derive(Debug, Default)]
pub struct FakeControl {
    cancelled: AtomicBool,
}

impl DriverControl for FakeControl {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Factory for the deterministic fake backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct FakeFactory;

impl FakeFactory {
    /// Creates a fake factory.
    pub fn new() -> Self {
        Self
    }
}

impl DriverFactory for FakeFactory {
    fn kind(&self) -> BackendKind {
        BackendKind::Fake
    }

    fn create(&self, plugin: PreparedPlugin, _options: &SandboxOptions) -> Result<CreatedDriver> {
        let control = Arc::new(FakeControl::default());
        let driver_control: Arc<dyn DriverControl> = control.clone();
        let init_control = control;
        Ok(CreatedDriver {
            init: Box::new(move || {
                Ok(Box::new(FakeDriver {
                    plugin,
                    control: init_control,
                    state: BTreeMap::new(),
                    completed: 0,
                    cancelled: 0,
                }) as Box<dyn Driver>)
            }),
            control: driver_control,
        })
    }
}

struct FakeDriver {
    plugin: PreparedPlugin,
    control: Arc<FakeControl>,
    state: BTreeMap<String, Value>,
    completed: u64,
    cancelled: u64,
}

impl Driver for FakeDriver {
    fn invoke(&mut self, invocation: Invocation) -> Result<Value> {
        if self.control.is_cancelled() {
            self.cancelled += 1;
            return Err(Error::Cancelled);
        }
        if invocation.deadline_elapsed() {
            return Err(Error::DeadlineExceeded);
        }
        let result = match invocation.export.as_str() {
            "echo" => Ok(invocation.input),
            "whoami" => Ok(json!({
              "entrypoint": self.plugin.entrypoint(),
              "modules": self.plugin.module_count(),
            })),
            "set" => {
                let (key, value) = key_value(&invocation.input)?;
                self.state.insert(key, value.clone());
                Ok(value)
            }
            "get" => {
                let key = key_string(&invocation.input)?;
                Ok(self.state.get(&key).cloned().unwrap_or(Value::Null))
            }
            "increment" => {
                let by = invocation
                    .input
                    .get("by")
                    .and_then(Value::as_i64)
                    .unwrap_or(1);
                let current = self
                    .state
                    .get("counter")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let next = current + by;
                self.state.insert("counter".to_string(), json!(next));
                Ok(json!(next))
            }
            "state" => {
                let mut map = Map::new();
                for (key, value) in &self.state {
                    map.insert(key.clone(), value.clone());
                }
                Ok(Value::Object(map))
            }
            "sleep" => {
                let ms = invocation
                    .input
                    .get("ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(10);
                self.pause(Duration::from_millis(ms), invocation.deadline)?;
                Ok(json!({"slept_ms": ms}))
            }
            "block" => {
                self.pause(Duration::from_secs(3600), invocation.deadline)?;
                Ok(json!({"blocked_until": "cancelled"}))
            }
            "boom" => Err(Error::Execution {
                message: "fake backend boom".to_string(),
            }),
            other => Err(Error::Execution {
                message: format!("fake backend has no export '{other}'"),
            }),
        };
        match result {
            Ok(value) => {
                self.completed += 1;
                Ok(value)
            }
            Err(Error::Cancelled) => {
                self.cancelled += 1;
                Err(Error::Cancelled)
            }
            Err(err) => Err(err),
        }
    }

    fn close(self: Box<Self>) -> Result<CloseReport> {
        Ok(CloseReport {
            invocations_completed: self.completed,
            invocations_cancelled: self.cancelled,
            forced_termination: self.control.is_cancelled(),
            closed_cleanly: true,
        })
    }
}

impl FakeDriver {
    fn pause(&self, max: Duration, deadline: Option<SystemTime>) -> Result<()> {
        let start = std::time::Instant::now();
        loop {
            if self.control.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if let Some(deadline) = deadline {
                if SystemTime::now() >= deadline {
                    return Err(Error::DeadlineExceeded);
                }
            }
            if start.elapsed() >= max {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn key_string(input: &Value) -> Result<String> {
    input
        .get("key")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| Error::Execution {
            message: "fake backend expects input.key to be a string".to_string(),
        })
}

fn key_value(input: &Value) -> Result<(String, Value)> {
    let key = key_string(input)?;
    let value = input
        .get("value")
        .cloned()
        .ok_or_else(|| Error::Execution {
            message: "fake backend expects input.value".to_string(),
        })?;
    Ok((key, value))
}
