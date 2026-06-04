use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub hooks: Vec<PluginHook>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PluginHook {
    OnCommand,
    OnConnect,
    OnDisconnect,
    OnOutput,
    OnInit,
}

pub struct PluginHost {
    manifest: PluginManifest,
}

impl PluginHost {
    pub fn new(wasm_bytes: &[u8]) -> anyhow::Result<Self> {
        let engine = wasmtime::Engine::default();
        let _module = wasmtime::Module::new(&engine, wasm_bytes)
            .map_err(|e| anyhow::anyhow!("WASM module error: {}", e))?;

        let manifest = PluginManifest {
            name: "unknown".to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            author: String::new(),
            hooks: vec![],
        };

        Ok(Self { manifest })
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    pub fn on_command(&mut self, _command: &str) -> anyhow::Result<Option<String>> {
        if !self.manifest.hooks.contains(&PluginHook::OnCommand) {
            return Ok(None);
        }
        Ok(None)
    }
}
