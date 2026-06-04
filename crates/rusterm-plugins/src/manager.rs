use std::path::{Path, PathBuf};

use tracing;

use crate::host::{PluginHost, PluginManifest};

pub struct PluginManager {
    plugins_dir: PathBuf,
    plugins: Vec<PluginHost>,
}

impl PluginManager {
    pub fn new(plugins_dir: Option<PathBuf>) -> Self {
        let plugins_dir = plugins_dir.unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("rusterm")
                .join("plugins")
        });

        Self {
            plugins_dir,
            plugins: Vec::new(),
        }
    }

    pub fn load_all(&mut self) -> anyhow::Result<()> {
        if !self.plugins_dir.exists() {
            std::fs::create_dir_all(&self.plugins_dir)?;
            return Ok(());
        }

        for entry in std::fs::read_dir(&self.plugins_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "wasm").unwrap_or(false) {
                match self.load_plugin(&path) {
                    Ok(()) => tracing::info!("Loaded plugin: {}", path.display()),
                    Err(e) => tracing::warn!("Failed to load plugin {}: {}", path.display(), e),
                }
            }
        }

        Ok(())
    }

    fn load_plugin(&mut self, path: &Path) -> anyhow::Result<()> {
        let bytes = std::fs::read(path)?;
        let host = PluginHost::new(&bytes)?;
        self.plugins.push(host);
        Ok(())
    }

    pub fn list(&self) -> Vec<&PluginManifest> {
        self.plugins.iter().map(|p| p.manifest()).collect()
    }

    pub fn on_command(&mut self, command: &str) -> Vec<String> {
        self.plugins
            .iter_mut()
            .filter_map(|p| p.on_command(command).ok().flatten())
            .collect()
    }
}
