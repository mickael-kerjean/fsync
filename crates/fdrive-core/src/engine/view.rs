use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, UNIX_EPOCH};

use crate::path::RelPath;
use crate::port::LocalTree;
use crate::sdk::FileInfo;

use super::Engine;
use crate::model::{Fate, Observation};

impl<T: LocalTree> Engine<T> {
    pub fn fates(&self) -> BTreeMap<RelPath, Fate> {
        self.state().view().fates
    }

    pub(super) fn upstream_of(&self, path: &RelPath) -> Option<RelPath> {
        match self.fates().get(path) {
            Some(Fate::Arrived { from, .. }) => Some(from.clone()),
            _ => None,
        }
    }

    pub fn listed(&self, dir: &RelPath, entries: &[FileInfo]) {
        let fates = self.fates();
        let mut ledger = self.ledger();
        for e in entries {
            if e.kind != crate::sdk::FileType::File {
                continue;
            }
            let path = dir.join(&e.name);
            if ledger.dirty.contains(&path) || fates.contains_key(&path) {
                continue;
            }
            let obs = Observation::of(e);
            if ledger.observations.get(&path) == Some(&obs) {
                continue;
            }
            let mirrors = fs::metadata(self.tree.backing(&path))
                .is_ok_and(|md| Observation::of_local(&md) == obs);
            if mirrors {
                ledger.observe(&path, obs);
            }
        }
    }

    pub fn overlay(&self, dir: &RelPath, mut listing: Vec<FileInfo>) -> Vec<FileInfo> {
        let fates = self.fates();
        listing.retain(|e| {
            let path = dir.join(&e.name);
            !matches!(fates.get(&path), Some(Fate::Gone))
        });
        for (path, fate) in &fates {
            let Fate::Arrived { was, .. } = fate else {
                continue;
            };
            if path.parent_or_root() != *dir {
                continue;
            }
            let name = path.name();
            if listing.iter().any(|e| e.name == name) {
                continue;
            }
            let (size, mtime) = match fs::metadata(self.tree.backing(path)) {
                Ok(md) => (md.len(), md.modified().ok()),
                Err(_) => (was.size, Some(UNIX_EPOCH + Duration::from_secs(was.time))),
            };
            listing.push(FileInfo {
                name: name.to_string(),
                kind: crate::sdk::FileType::File,
                size: Some(size),
                mtime,
            });
        }
        let extras: Vec<RelPath> = {
            let ledger = self.ledger();
            ledger
                .dirty
                .iter()
                .filter(|p| ledger.local_only(p) && p.parent_or_root() == *dir)
                .cloned()
                .collect()
        };
        for path in extras {
            let name = path.name();
            if !listing.iter().any(|e| e.name == name) {
                if let Ok(md) = fs::metadata(self.tree.backing(&path)) {
                    listing.push(FileInfo {
                        name: name.to_string(),
                        kind: crate::sdk::FileType::File,
                        size: Some(md.len()),
                        mtime: md.modified().ok(),
                    });
                }
            }
        }
        listing
    }

    pub fn observed(&self, path: &RelPath) -> Option<Observation> {
        self.ledger().observations.get(path).copied()
    }

    pub fn is_dirty(&self, path: &RelPath) -> bool {
        self.ledger().dirty.contains(path)
    }

    pub fn needs_baseline(&self, path: &RelPath) -> bool {
        let ledger = self.ledger();
        !ledger.observations.contains_key(path) && !ledger.dirty.contains(path)
    }

    pub async fn overwriting(&self, path: &RelPath) {
        if self.needs_baseline(path) {
            if let Ok(info) = self.sdk.stat(&path.as_file()).await {
                self.ledger().observe(path, Observation::of(&info));
            }
        }
    }

    pub fn content_current(&self, path: &RelPath, current: Observation) -> bool {
        let (observed, dirty) = {
            let ledger = self.ledger();
            (
                ledger.observations.get(path).copied(),
                ledger.dirty.contains(path),
            )
        };
        dirty || (observed == Some(current) && self.tree.backing(path).is_file())
    }

    pub fn dirty_metadata(&self, path: &RelPath) -> Option<fs::Metadata> {
        if !self.ledger().dirty.contains(path) {
            return None;
        }
        fs::metadata(self.tree.backing(path)).ok()
    }
}
