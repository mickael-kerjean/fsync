use std::fmt;

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct RelPath(String);

impl RelPath {
    pub fn new(path: &str) -> Self {
        Self(
            path.split('/')
                .filter(|s| !s.is_empty() && *s != "." && *s != "..")
                .collect::<Vec<_>>()
                .join("/"),
        )
    }

    pub fn root() -> Self {
        Self(String::new())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or("")
    }

    pub fn parent(&self) -> Option<RelPath> {
        self.0.rsplit_once('/').map(|(p, _)| RelPath(p.to_string()))
    }

    pub fn parent_or_root(&self) -> RelPath {
        self.parent().unwrap_or_else(RelPath::root)
    }

    pub fn join(&self, name: &str) -> RelPath {
        if self.0.is_empty() {
            RelPath::new(name)
        } else {
            RelPath::new(&format!("{}/{name}", self.0))
        }
    }

    pub fn is_descendant_of(&self, other: &RelPath) -> bool {
        self.0.len() > other.0.len()
            && self.0.starts_with(other.0.as_str())
            && self.0.as_bytes()[other.0.len()] == b'/'
    }

    pub fn as_file(&self) -> String {
        format!("/{}", self.0)
    }

    pub fn as_dir(&self) -> String {
        if self.is_root() {
            "/".to_string()
        } else {
            format!("/{}/", self.0)
        }
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[path = "path_test.rs"]
mod tests;
