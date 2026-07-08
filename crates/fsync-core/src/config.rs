use std::collections::BTreeSet;
use std::path::Path;

use crate::path::RelPath;

const FILE: &str = "fsync.toml";

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Ignore(BTreeSet<String>);

impl Default for Ignore {
    fn default() -> Self {
        Self(
            ["node_modules", ".DS_Store", "Thumbs.db", "desktop.ini"]
                .map(String::from)
                .into(),
        )
    }
}

impl Ignore {
    pub fn matches(&self, path: &RelPath) -> bool {
        path.as_str().split('/').any(|name| self.0.contains(name))
    }
}

pub fn ignore(data: &Path) -> Ignore {
    #[derive(Default, serde::Deserialize)]
    struct File {
        #[serde(default)]
        sync: Sync,
    }
    #[derive(Default, serde::Deserialize)]
    struct Sync {
        ignore: Option<Ignore>,
    }
    load::<File>(&data.join(FILE))
        .unwrap_or_default()
        .sync
        .ignore
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub url: String,
    pub token: String,
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ConfigFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<Session>,
    #[serde(flatten)]
    rest: toml::Table,
}

pub fn recall(data: &Path) -> Option<Session> {
    let session = load::<ConfigFile>(&data.join(FILE))?.session?;
    (!session.url.is_empty() && !session.token.is_empty()).then_some(session)
}

pub fn remember(data: &Path, url: &str, token: &str, insecure: bool) {
    if url.is_empty() || token.is_empty() {
        return;
    }
    update(
        data,
        Some(Session {
            url: url.to_owned(),
            token: token.to_owned(),
            insecure,
        }),
    );
}

pub fn forget(data: &Path) {
    update(data, None);
}

fn update(data: &Path, session: Option<Session>) {
    let path = data.join(FILE);
    let mut config = load::<ConfigFile>(&path).unwrap_or_default();
    if config.session == session {
        return;
    }
    config.session = session;
    if config.session.is_none() && config.rest.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else if let Ok(text) = toml::to_string_pretty(&config) {
        let _ = crate::write_atomic(&path, text.as_bytes());
    }
}

fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
#[path = "config_test.rs"]
mod tests;
