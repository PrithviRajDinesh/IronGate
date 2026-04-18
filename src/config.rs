use arc_swap::ArcSwap;
use serde::Deserialize;
use std::sync::Arc;
use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::mpsc::channel;
use std::time::Duration;

#[derive(Debug, Deserialize, Clone)]
pub struct BackendConfig {
    pub address: String,
    pub weight: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server_port: u16,
    pub timeout_ms: u64,
    pub max_connections: usize,
    pub feature_flags: FeatureFlags,
    pub backends: Vec<BackendConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FeatureFlags {
    pub enable_caching: bool,
    pub enable_rate_limiting: bool,
    pub rate_limit_per_second: u32,
}

pub struct ConfigManager {
    config: ArcSwap<Config>,
    path: String,
}

impl Config {
    //Validate Configuration before accepting it
    pub fn validate(&self) -> Result<(), String> {
        if self.server_port == 0{
            return Err("server_port must be non-zero".into());
        }

        if self.timeout_ms == 0 {
            return Err("timeout_ms must be non-zero".into());
        }

        if self.max_connections == 0{
            return Err("max_connections must be non-zero".into());
        }

        if self.feature_flags.enable_rate_limiting && self.feature_flags.rate_limit_per_second == 0 {
            return Err("rate_limit_per_second must be set when enable_rate_limiting is enabled".into());
        }

        Ok(())
    }

}

impl ConfigManager {
    pub fn new(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let config = Self::load_from_file(path)?;

        if let Err(e) = config.validate(){
            return Err(format!("Valid startup configuration: {}", e).into());
        }

        Ok(Self {
            config: ArcSwap::from_pointee(config),
            path: path.to_string(),
        })
    }

    fn load_from_file(path: &str) -> Result<Config, Box<dyn std::error::Error>>{
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    pub fn reload_with_validation(&self) -> Result<(), Box<dyn std::error::Error>> {
        let new_config = Self::load_from_file(&self.path)?;

        //Validate before swapping
        if let Err(e) = new_config.validate(){
            return Err(e.into());
        }

        self.config.store(Arc::new(new_config));
        Ok(())
    }

    pub fn get(&self) -> arc_swap::Guard<Arc<Config>> {
        self.config.load()
    }

    pub fn reload(&self) -> Result<(), Box<dyn std::error::Error>> {
        let new_config = Self::load_from_file(&self.path)?;
        self.config.store(Arc::new(new_config));
        println!("Configuration reloaded from {}", self.path);
        Ok(())
    }

    //Spawns a background thread that watches for file changes and auto reloads Configuration
    pub fn watch_for_changes(
        manager: Arc<ConfigManager>,
    ) -> Result <(), Box<dyn std::error::Error>> {
        let (tx, rx) = channel();

        let mut watcher = RecommendedWatcher::new(
            move |res| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            },
            NotifyConfig::default().with_poll_interval(Duration::from_secs(2)),
        )?;

        let path_to_watch = manager.path.clone();
        watcher.watch(Path::new(&path_to_watch), RecursiveMode::NonRecursive)?;

        std::thread::spawn(move || {
            let _watcher = watcher;

            for event in rx {
                if event.kind.is_modify() {
                    println!("Config file changed, reloading...");

                    std::thread::sleep(Duration::from_millis(150));

                    match manager.reload_with_validation() {
                        Ok(()) => println!("Configuration reloaded successfully"),
                        Err(e) => eprintln!("Failed to reload config: {}", e),
                    }
                }
            }
        });
        Ok(())
    }
}
