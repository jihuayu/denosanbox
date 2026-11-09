//! Trusted N-API addon. Guest code runs only through the Rust facade.
use denosanbox::{
    Engine, EngineConfig, InvocationOptions, PluginSource, SandboxHandle, SandboxOptions,
};
use napi::bindgen_prelude::*;
use napi_derive::napi;
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
}

#[napi]
impl NativeEngine {
    #[napi(constructor)]
    pub fn new() -> Result<Self> {
        Ok(Self {
            engine: Engine::new(EngineConfig::default()).map_err(native_error)?,
        })
    }

    #[napi]
    pub fn prepare(&self, source: String, entrypoint: String) -> Result<()> {
        self.engine
            .prepare(PluginSource::single(entrypoint, source))
            .map_err(native_error)?;
        Ok(())
    }

    #[napi]
    pub async fn create(&self, source: String, entrypoint: String) -> Result<NativeSandbox> {
        let plugin = self
            .engine
            .prepare(PluginSource::single(entrypoint, source))
            .map_err(native_error)?;
        let sandbox = self
            .engine
            .create(plugin, SandboxOptions::default())
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
