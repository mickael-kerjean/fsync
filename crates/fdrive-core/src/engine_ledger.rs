use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::path::RelPath;
use crate::sdk::FileInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub size: u64,
    pub time: u64,
}

impl Observation {
    pub fn new(size: u64, mtime: Option<SystemTime>) -> Self {
        Self {
            size,
            time: secs(mtime),
        }
    }

    pub fn of(info: &FileInfo) -> Self {
        Self::new(info.size.unwrap_or(0), info.mtime)
    }

    pub fn of_local(md: &fs::Metadata) -> Self {
        Self::new(md.len(), md.modified().ok())
    }
}

fn secs(t: Option<SystemTime>) -> u64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Default)]
pub struct Ledger {
    pub observations: BTreeMap<RelPath, Observation>,
    pub dirty: BTreeSet<RelPath>,
    db: Option<rusqlite::Connection>,
}

const SUBTREE: &str = "path = ?1 OR (path >= ?1 || '/' AND path < ?1 || '0')";

fn open_db(file: &Path, schema: &str) -> rusqlite::Result<rusqlite::Connection> {
    let db = rusqlite::Connection::open(file)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "synchronous", "OFF")?;
    let _: String = db.query_row("PRAGMA journal_mode=MEMORY", [], |row| row.get(0))?;
    db.execute_batch(schema)?;
    Ok(db)
}

impl Ledger {
    pub(crate) fn open(file: &Path) -> Result<Self, ()> {
        let load = || -> rusqlite::Result<Self> {
            let db = open_db(
                file,
                "CREATE TABLE IF NOT EXISTS observations(path TEXT PRIMARY KEY, size INTEGER NOT NULL, time INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS dirty(path TEXT PRIMARY KEY);",
            )?;
            let mut ledger = Ledger::default();
            {
                let mut stmt = db.prepare("SELECT path, size, time FROM observations")?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let path: String = row.get(0)?;
                    let (size, time): (i64, i64) = (row.get(1)?, row.get(2)?);
                    ledger.observations.insert(
                        RelPath::new(&path),
                        Observation {
                            size: size as u64,
                            time: time as u64,
                        },
                    );
                }
                let mut stmt = db.prepare("SELECT path FROM dirty")?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let path: String = row.get(0)?;
                    ledger.dirty.insert(RelPath::new(&path));
                }
            }
            ledger.db = Some(db);
            Ok(ledger)
        };
        load().map_err(|err| log::error!("{} is unreadable: {err}", file.display()))
    }

    fn exec(&self, sql: &str, params: impl rusqlite::Params) {
        if let Some(db) = &self.db {
            match db
                .prepare_cached(sql)
                .and_then(|mut stmt| stmt.execute(params))
            {
                Ok(_) => {}
                Err(err) => log::error!("ledger: {err}"),
            }
        }
    }

    pub fn dirty_set(&mut self, path: &RelPath) -> bool {
        let inserted = self.dirty.insert(path.clone());
        if inserted {
            self.exec(
                "INSERT OR IGNORE INTO dirty(path) VALUES (?1)",
                [path.as_str()],
            );
        }
        inserted
    }

    pub fn dirty_clear(&mut self, path: &RelPath) {
        if self.dirty.remove(path) {
            self.exec("DELETE FROM dirty WHERE path = ?1", [path.as_str()]);
        }
    }

    pub fn observe(&mut self, path: &RelPath, obs: Observation) {
        self.observations.insert(path.clone(), obs);
        self.exec(
            "INSERT OR REPLACE INTO observations(path, size, time) VALUES (?1, ?2, ?3)",
            rusqlite::params![path.as_str(), obs.size as i64, obs.time as i64],
        );
    }

    pub fn unobserve(&mut self, path: &RelPath) {
        if self.observations.remove(path).is_some() {
            self.exec("DELETE FROM observations WHERE path = ?1", [path.as_str()]);
        }
    }

    pub fn forget(&mut self, path: &RelPath) {
        self.observations
            .retain(|p, _| p != path && !p.is_descendant_of(path));
        self.dirty
            .retain(|p| p != path && !p.is_descendant_of(path));
        self.exec(
            &format!("DELETE FROM observations WHERE {SUBTREE}"),
            [path.as_str()],
        );
        self.exec(
            &format!("DELETE FROM dirty WHERE {SUBTREE}"),
            [path.as_str()],
        );
    }

    pub fn remap(&mut self, from: &RelPath, to: &RelPath) {
        let rebase =
            |p: &RelPath| RelPath::new(&p.as_str().replacen(from.as_str(), to.as_str(), 1));
        let moved: Vec<RelPath> = self
            .observations
            .keys()
            .filter(|p| *p == from || p.is_descendant_of(from))
            .cloned()
            .collect();
        for p in moved {
            let record = self.observations.remove(&p).unwrap();
            self.exec("DELETE FROM observations WHERE path = ?1", [p.as_str()]);
            let dest = rebase(&p);
            self.exec(
                "INSERT OR REPLACE INTO observations(path, size, time) VALUES (?1, ?2, ?3)",
                rusqlite::params![dest.as_str(), record.size as i64, record.time as i64],
            );
            self.observations.insert(dest, record);
        }
        let moved: Vec<RelPath> = self
            .dirty
            .iter()
            .filter(|p| *p == from || p.is_descendant_of(from))
            .cloned()
            .collect();
        for p in moved {
            self.dirty.remove(&p);
            self.exec("DELETE FROM dirty WHERE path = ?1", [p.as_str()]);
            let dest = rebase(&p);
            self.exec(
                "INSERT OR REPLACE INTO dirty(path) VALUES (?1)",
                [dest.as_str()],
            );
            self.dirty.insert(dest);
        }
    }

    pub(crate) fn local_only(&self, path: &RelPath) -> bool {
        !self.observations.contains_key(path) && self.dirty.contains(path)
    }
}
