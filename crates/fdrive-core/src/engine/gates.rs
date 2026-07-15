use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use crate::path::RelPath;

use super::Download;

#[derive(Default)]
pub(super) struct Transfers {
    pub(super) hydrating: Mutex<HashMap<RelPath, Arc<tokio::sync::Mutex<()>>>>,
    pub(super) downloads: Mutex<HashMap<RelPath, Arc<Download>>>,
    pub(super) uploading: Mutex<HashMap<RelPath, Arc<tokio::sync::Mutex<()>>>>,
}

impl Transfers {
    pub(super) fn upload_gate(&self, path: &RelPath) -> Arc<tokio::sync::Mutex<()>> {
        gate(&self.uploading, path)
    }

    pub(super) fn hydrate_gate(&self, path: &RelPath) -> Arc<tokio::sync::Mutex<()>> {
        gate(&self.hydrating, path)
    }
}

fn gate(
    gates: &Mutex<HashMap<RelPath, Arc<tokio::sync::Mutex<()>>>>,
    path: &RelPath,
) -> Arc<tokio::sync::Mutex<()>> {
    let mut gates = gates.lock().unwrap();
    gates.retain(|_, gate| Arc::strong_count(gate) > 1);
    gates.entry(path.clone()).or_default().clone()
}

pub(crate) struct Frozen<'a> {
    pub(super) set: &'a Mutex<BTreeSet<RelPath>>,
    pub(super) paths: Vec<RelPath>,
}

impl Drop for Frozen<'_> {
    fn drop(&mut self) {
        let mut set = self.set.lock().unwrap();
        for path in &self.paths {
            set.remove(path);
        }
    }
}
