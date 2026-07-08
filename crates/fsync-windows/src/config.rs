use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub windows: WindowsConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsConfig {
    #[serde(default = "default_provider_name")]
    pub provider_name: String,
    #[serde(default = "default_true")]
    pub allow_pinning: bool,
    #[serde(default = "default_refresh")]
    pub refresh_secs: u64,
    #[serde(default)]
    pub icon: Option<String>,
}

impl Default for WindowsConfig {
    fn default() -> Self {
        Self {
            provider_name: default_provider_name(),
            allow_pinning: true,
            refresh_secs: default_refresh(),
            icon: None,
        }
    }
}

fn default_provider_name() -> String {
    "Filestash".to_string()
}

fn default_true() -> bool {
    true
}

fn default_refresh() -> u64 {
    10
}

impl AppConfig {
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents).map_err(io::Error::other),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AppConfig;

    #[test]
    fn empty_toml_is_a_valid_config() {
        let config = toml::from_str::<AppConfig>("").unwrap();
        assert_eq!(config.windows.provider_name, "Filestash");
    }

    #[test]
    fn provider_name_can_be_set() {
        let config = toml::from_str::<AppConfig>("[windows]\nprovider_name = \"Custom\"").unwrap();
        assert_eq!(config.windows.provider_name, "Custom");
    }
}
