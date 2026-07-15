use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::path::RelPath;
use crate::port::LocalTree;

use super::Engine;
use crate::model::Observation;

impl<T: LocalTree> Engine<T> {
    pub fn pin(&self, path: &RelPath) {
        self.ledger().pin_set(path);
        log::info!("pinned {path}");
        self.pin_sweep();
    }

    pub fn unpin(&self, path: &RelPath) {
        self.ledger().pin_clear(path);
        log::info!("unpinned {path}");
    }

    pub fn pinned(&self, path: &RelPath) -> bool {
        self.ledger()
            .pins
            .iter()
            .any(|p| path == p || path.is_descendant_of(p))
    }

    pub(super) fn pin_sweep(&self) {
        self.spawner.spawn(|engine| async move {
            let roots: Vec<RelPath> = engine.ledger().pins.iter().cloned().collect();
            for root in roots {
                engine.hydrate_subtree(&root).await;
            }
        });
    }

    pub(super) async fn hydrate_subtree(&self, root: &RelPath) {
        let mut dirs = vec![root.clone()];
        while let Some(dir) = dirs.pop() {
            let listing = match self.sdk.ls(&dir.as_dir()).await {
                Ok(listing) => listing,
                Err(_) if dir == *root => {
                    if let Err(err) = self.hydrate(root, None).await {
                        log::debug!("pin {root}: {err}");
                    }
                    return;
                }
                Err(err) => {
                    log::debug!("pin {dir}: {err}");
                    continue;
                }
            };
            self.listed(&dir, &listing);
            for entry in listing {
                let child = dir.join(&entry.name);
                if child.parent_or_root() != dir {
                    continue;
                }
                match entry.kind {
                    crate::sdk::FileType::Directory => dirs.push(child),
                    crate::sdk::FileType::File => {
                        let hint = Observation::of(&entry);
                        if self.content_current(&child, hint) {
                            continue;
                        }
                        if let Err(err) = self.hydrate(&child, Some(hint)).await {
                            log::debug!("pin {child}: {err}");
                        }
                    }
                }
            }
        }
    }

    pub fn prune(&self, cache_root: &Path) -> io::Result<()> {
        if std::mem::take(&mut self.ledger().unreadable) {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let aside = cache_root.with_file_name(format!(
                "{}.unreadable-{stamp}",
                cache_root.file_name().unwrap_or_default().to_string_lossy()
            ));
            log::error!(
                "the ledger was unreadable; moving the cache to {} instead of pruning it",
                aside.display()
            );
            fs::rename(cache_root, &aside)?;
            fs::create_dir_all(cache_root)?;
            return Ok(());
        }
        let owed: BTreeSet<RelPath> = self.state().owed();
        let mut ledger = self.ledger();
        let pins = ledger.pins.clone();
        let pinned = |p: &RelPath| pins.iter().any(|r| p == r || p.is_descendant_of(r));
        let gone: Vec<RelPath> = ledger
            .observations
            .keys()
            .filter(|p| !ledger.dirty.contains(p) && !owed.contains(*p) && !pinned(p))
            .cloned()
            .collect();
        for path in &gone {
            ledger.unobserve(path);
        }
        let keep: Vec<PathBuf> = ledger
            .dirty
            .iter()
            .chain(owed.iter())
            .chain(pins.iter())
            .map(|p| self.tree.backing(p))
            .collect();
        drop(ledger);
        prune_dir(cache_root, &keep)?;
        Ok(())
    }
}

fn prune_dir(dir: &Path, keep: &[PathBuf]) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if keep.iter().any(|k| path.starts_with(k)) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            if keep.iter().any(|k| k.starts_with(&path)) {
                prune_dir(&path, keep)?;
            } else {
                fs::remove_dir_all(&path)?;
            }
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}
