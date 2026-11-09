//! Trusted N-API addon. Guest code runs only through the Rust facade.
use denosanbox::{
    policy, Engine, EngineConfig, HostGrant, InvocationOptions, PluginSource, Policy,
    RegisteredCallback, SandboxHandle, SandboxOptions,
};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn native_error(error: denosanbox::Error) -> napi::Error {
    napi::Error::new(
        Status::GenericFailure,
        format!("{}: {}", error.kind(), error),
    )
}

#[napi(js_name = "NativeEngine")]
pub struct NativeEngine {
    engine: Engine,
    callbacks: Mutex<BTreeMap<String, RegisteredCallback>>,
}

#[napi]
impl NativeEngine {
    #[napi(constructor)]
    pub fn new(host_capabilities: Option<Vec<String>>) -> Result<Self> {
        let mut ceiling = Policy::default();
        for name in host_capabilities.unwrap_or_default() {
            ceiling = ceiling.allow_capability(&name).map_err(|_| {
                napi::Error::new(
                    Status::InvalidArg,
                    "invalid_configuration: invalid host capability",
                )
            })?;
        }
        Ok(Self {
            engine: Engine::with_policy(EngineConfig::default(), ceiling).map_err(native_error)?,
            callbacks: Mutex::new(BTreeMap::new()),
        })
    }

    #[napi]
    pub fn register_host(
        &self,
        name: String,
        callback: Function<'_, FnArgs<(String, String)>, Promise<String>>,
        input_limit: u32,
        output_limit: u32,
    ) -> Result<()> {
        let dispatch = Arc::new(
            callback
                .build_threadsafe_function::<FnArgs<(String, String)>>()
                .callee_handled::<false>()
                .weak::<true>()
                .max_queue_size::<32>()
                .build()?,
        );
        let registered=self.engine.register_host_async(&name,input_limit as usize,output_limit as usize,move |context,input| {
            let dispatch=dispatch.clone();
            async move {
                context.check_live()?;
                let metadata=serde_json::json!({"sandboxId":context.sandbox_id().to_string(),"scope":context.scope(),"remainingMs":context.deadline().remaining().map(|d|d.as_millis() as u64)}).to_string();
                let input=String::from_utf8(input).map_err(|_|policy::Error::invalid_argument("invalid host JSON"))?;
                let promise=dispatch.call_async_catch(FnArgs::from((metadata,input))).await.map_err(|_|policy::Error::invalid_argument("Node callback dispatch failed"))?;
                let output=promise.await.map_err(|_|policy::Error::invalid_argument("Node callback failed"))?;
                context.check_live()?;
                if output.len()>output_limit as usize { return Err(policy::Error::limit_exceeded("Node callback output exceeds limit")); }
                Ok(output.into_bytes())
            }
        }).map_err(native_error)?;
        self.callbacks
            .lock()
            .map_err(|_| {
                napi::Error::new(
                    Status::GenericFailure,
                    "internal: callback registry unavailable",
                )
            })?
            .insert(name, registered);
        Ok(())
    }

    #[napi]
    pub fn prepare(&self, source: String, entrypoint: String) -> Result<()> {
        self.engine
            .prepare(PluginSource::single(entrypoint, source))
            .map_err(native_error)?;
        Ok(())
    }

    #[napi]
    pub async fn create(
        &self,
        source: String,
        entrypoint: String,
        grants_json: String,
    ) -> Result<NativeSandbox> {
        let plugin = self
            .engine
            .prepare(PluginSource::single(entrypoint, source))
            .map_err(native_error)?;
        if grants_json.len() > 1024 * 1024 {
            return Err(napi::Error::new(
                Status::InvalidArg,
                "limit_exceeded: host grants too large",
            ));
        }
        let requested_grants: BTreeMap<String, String> = serde_json::from_str(&grants_json)
            .map_err(|_| {
                napi::Error::new(
                    Status::InvalidArg,
                    "invalid_configuration: invalid host grants",
                )
            })?;
        let mut requested = Policy::default();
        let mut grants = Vec::new();
        {
            let registered = self.callbacks.lock().map_err(|_| {
                napi::Error::new(
                    Status::GenericFailure,
                    "internal: callback registry unavailable",
                )
            })?;
            for (name, scope) in requested_grants {
                let callback = registered
                    .get(&name)
                    .ok_or_else(|| {
                        napi::Error::new(
                            Status::InvalidArg,
                            "invalid_configuration: unknown host callback",
                        )
                    })?
                    .clone();
                requested = requested.allow_capability(&name).map_err(|_| {
                    napi::Error::new(
                        Status::InvalidArg,
                        "invalid_configuration: invalid capability",
                    )
                })?;
                grants.push(HostGrant { callback, scope });
            }
        }
        let sandbox = self
            .engine
            .create(
                plugin,
                SandboxOptions {
                    policy: requested,
                    host_grants: grants,
                    ..SandboxOptions::default()
                },
            )
            .await
            .map_err(native_error)?;
        Ok(NativeSandbox { sandbox })
    }

    #[napi]
    pub async fn shutdown(&self) -> Result<bool> {
        Ok(self.engine.shutdown().await.map_err(native_error)?.clean)
    }
}

#[napi(js_name = "NativeSandbox")]
pub struct NativeSandbox {
    sandbox: SandboxHandle,
}

#[napi]
impl NativeSandbox {
    #[napi(getter)]
    pub fn id(&self) -> String {
        self.sandbox.id().to_string()
    }

    #[napi]
    pub async fn call(
        &self,
        export: String,
        input_json: String,
        timeout_ms: Option<u32>,
    ) -> Result<String> {
        // This first binding exposes default core limits; reject before parsing.
        if input_json.len() > 1024 * 1024 {
            return Err(napi::Error::new(
                Status::InvalidArg,
                "limit_exceeded: JSON input exceeds 1 MiB",
            ));
        }
        let input = serde_json::from_str(&input_json)
            .map_err(|_| napi::Error::new(Status::InvalidArg, "invalid_input: malformed JSON"))?;
        let options = InvocationOptions {
            timeout: timeout_ms.map(|ms| Duration::from_millis(ms.into())),
            deadline: None,
        };
        let value = self
            .sandbox
            .call(&export, input, options)
            .await
            .map_err(native_error)?;
        serde_json::to_string(&value).map_err(|_| {
            napi::Error::new(
                Status::GenericFailure,
                "serialization: result cannot be encoded",
            )
        })
    }

    #[napi]
    pub fn cancel(&self) {
        self.sandbox.cancel();
    }

    #[napi]
    pub async fn close(&self) -> Result<bool> {
        Ok(self
            .sandbox
            .close()
            .await
            .map_err(native_error)?
            .closed_cleanly)
    }
}
