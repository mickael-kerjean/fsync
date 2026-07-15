use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use crate::model::{secs, Conflict, Observation, Operation, Plan};
use crate::path::RelPath;

#[derive(Default)]
pub struct Ledger {
    pub observations: BTreeMap<RelPath, Observation>,
    pub pins: BTreeSet<RelPath>,

    pub dirty: BTreeSet<RelPath>,
    pub(crate) unreadable: bool,
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

fn row_obs(size: Option<i64>, time: Option<i64>) -> Option<Observation> {
    Some(Observation {
        size: size? as u64,
        time: time? as u64,
    })
}

impl Ledger {
    pub(crate) fn open(file: &Path) -> Result<Self, ()> {
        let load = || -> rusqlite::Result<Self> {
            let db = open_db(
                file,
                "CREATE TABLE IF NOT EXISTS observations(path TEXT PRIMARY KEY, size INTEGER NOT NULL, time INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS journal(seq INTEGER PRIMARY KEY, op TEXT NOT NULL, path TEXT NOT NULL, dest TEXT, base TEXT, size INTEGER, time INTEGER);
                 CREATE TABLE IF NOT EXISTS conflicts(seq INTEGER PRIMARY KEY, op TEXT NOT NULL, path TEXT NOT NULL, dest TEXT, expected_size INTEGER, expected_time INTEGER, found_size INTEGER, found_time INTEGER, ours TEXT, at INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS pins(path TEXT PRIMARY KEY);
                 CREATE TABLE IF NOT EXISTS signatures(path TEXT PRIMARY KEY, sig BLOB NOT NULL);",
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
                let mut stmt = db.prepare("SELECT path FROM journal WHERE op IN ('w', 's')")?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let path: String = row.get(0)?;
                    ledger.dirty.insert(RelPath::new(&path));
                }
                let mut stmt = db.prepare("SELECT path FROM pins")?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let path: String = row.get(0)?;
                    ledger.pins.insert(RelPath::new(&path));
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

    pub fn pin_set(&mut self, path: &RelPath) {
        if self.pins.insert(path.clone()) {
            self.exec("INSERT INTO pins(path) VALUES (?1)", [path.as_str()]);
        }
    }

    pub fn pin_clear(&mut self, path: &RelPath) {
        if self.pins.remove(path) {
            self.exec("DELETE FROM pins WHERE path = ?1", [path.as_str()]);
        }
    }

    pub(crate) fn mark(&self, path: &RelPath) {
        self.exec(
            "INSERT INTO journal(op, path) VALUES ('w', ?1)",
            [path.as_str()],
        );
    }

    pub(crate) fn journal_load(&self) -> (Vec<Operation>, Vec<(i64, Plan)>) {
        let Some(db) = &self.db else {
            return (Vec::new(), Vec::new());
        };
        let mut window = Vec::new();
        let mut out = Vec::new();
        let mut read = || -> rusqlite::Result<()> {
            let mut stmt = db.prepare(
                "SELECT seq, op, path, dest, base, size, time FROM journal ORDER BY seq",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let seq: i64 = row.get(0)?;
                let op: String = row.get(1)?;
                let path = RelPath::new(&row.get::<_, String>(2)?);
                let dest: Option<String> = row.get(3)?;
                let base: Option<String> = row.get(4)?;
                let lease = row_obs(row.get(5)?, row.get(6)?);
                let plan = match op.as_str() {
                    "w" => {
                        if !window.contains(&Operation::Write(path.clone())) {
                            window.push(Operation::Write(path));
                        }
                        continue;
                    }
                    "s" => Plan::Save {
                        path,
                        replaces: lease,
                        reuses: base.map(|b| RelPath::new(&b)),
                    },
                    "m" => match (dest, lease) {
                        (Some(dest), Some(moves)) => Plan::Move {
                            from: path,
                            to: RelPath::new(&dest),
                            moves,
                        },
                        _ => continue,
                    },
                    "r" => match lease {
                        Some(removes) => Plan::Remove { path, removes },
                        None => continue,
                    },
                    _ => continue,
                };
                out.push((seq, plan));
            }
            Ok(())
        };
        if let Err(err) = read() {
            log::error!("ledger journal: {err}");
        }
        (window, out)
    }

    pub(crate) fn journal_swap(
        &mut self,
        unmark: &[RelPath],
        retired: &[i64],
        plans: &[Plan],
    ) -> Vec<(i64, Plan)> {
        let Some(db) = &self.db else {
            return Self::fallback_seqs(plans);
        };
        let mut out = Vec::new();
        let mut write = || -> rusqlite::Result<()> {
            db.execute_batch("BEGIN")?;
            for path in unmark {
                db.prepare_cached("DELETE FROM journal WHERE op = 'w' AND path = ?1")?
                    .execute([path.as_str()])?;
            }
            for seq in retired {
                db.prepare_cached("DELETE FROM journal WHERE seq = ?1")?
                    .execute([seq])?;
            }
            for plan in plans {
                let (op, path, dest, base, lease) = match plan {
                    Plan::Save {
                        path,
                        replaces,
                        reuses,
                    } => ("s", path, None, reuses.as_ref(), *replaces),
                    Plan::Move { from, to, moves } => ("m", from, Some(to), None, Some(*moves)),
                    Plan::Remove { path, removes } => ("r", path, None, None, Some(*removes)),
                };
                db.prepare_cached(
                    "INSERT INTO journal(op, path, dest, base, size, time) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?
                .execute(rusqlite::params![
                    op,
                    path.as_str(),
                    dest.map(|d| d.as_str()),
                    base.map(|b| b.as_str()),
                    lease.map(|l| l.size as i64),
                    lease.map(|l| l.time as i64),
                ])?;
                out.push((db.last_insert_rowid(), plan.clone()));
            }
            db.execute_batch("COMMIT")?;
            Ok(())
        };
        if let Err(err) = write() {
            log::error!("ledger journal: {err}");
            let _ = db.execute_batch("ROLLBACK");
            return Self::fallback_seqs(plans);
        }
        out
    }

    fn fallback_seqs(plans: &[Plan]) -> Vec<(i64, Plan)> {
        use std::sync::atomic::{AtomicI64, Ordering};
        static NEXT: AtomicI64 = AtomicI64::new(1 << 40);
        plans
            .iter()
            .map(|plan| (NEXT.fetch_add(1, Ordering::Relaxed), plan.clone()))
            .collect()
    }

    pub(crate) fn journal_retire(&self, seq: i64) {
        self.exec("DELETE FROM journal WHERE seq = ?1", [seq]);
    }

    pub(crate) fn conflict_add(&self, c: &Conflict) -> i64 {
        let (op, path, dest) = c.what();
        self.exec(
            "INSERT INTO conflicts(op, path, dest, expected_size, expected_time, found_size, found_time, ours, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                op,
                path.as_str(),
                dest.map(|d| d.as_str().to_string()),
                c.expected.map(|o| o.size as i64),
                c.expected.map(|o| o.time as i64),
                c.found.map(|o| o.size as i64),
                c.found.map(|o| o.time as i64),
                c.ours.as_ref().map(|p| p.as_str().to_string()),
                secs(Some(c.at)) as i64,
            ],
        );
        self.db
            .as_ref()
            .map(|db| db.last_insert_rowid())
            .unwrap_or_default()
    }

    pub(crate) fn conflict_retire(&self, seq: i64) {
        self.exec("DELETE FROM conflicts WHERE seq = ?1", [seq]);
    }

    pub(crate) fn conflicts_load(&self) -> Vec<Conflict> {
        let Some(db) = &self.db else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut read = || -> rusqlite::Result<()> {
            let mut stmt = db.prepare(
                "SELECT seq, op, path, dest, expected_size, expected_time, found_size, found_time, ours, at FROM conflicts ORDER BY seq",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let op: String = row.get(1)?;
                let path = RelPath::new(&row.get::<_, String>(2)?);
                let dest: Option<String> = row.get(3)?;
                if let Some(c) = Conflict::from_row(
                    row.get(0)?,
                    &op,
                    path,
                    dest.map(|d| RelPath::new(&d)),
                    row_obs(row.get(4)?, row.get(5)?),
                    row_obs(row.get(6)?, row.get(7)?),
                    row.get::<_, Option<String>>(8)?.map(|p| RelPath::new(&p)),
                    row.get::<_, i64>(9)? as u64,
                ) {
                    out.push(c);
                }
            }
            Ok(())
        };
        if let Err(err) = read() {
            log::error!("ledger conflicts: {err}");
        }
        out
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
        self.exec("DELETE FROM signatures WHERE path = ?1", [path.as_str()]);
    }

    pub fn sign_set(&mut self, path: &RelPath, sig: &[u8]) {
        self.exec(
            "INSERT OR REPLACE INTO signatures(path, sig) VALUES (?1, ?2)",
            rusqlite::params![path.as_str(), sig],
        );
    }

    pub fn sign_get(&self, path: &RelPath) -> Option<Vec<u8>> {
        self.db
            .as_ref()?
            .prepare_cached("SELECT sig FROM signatures WHERE path = ?1")
            .ok()?
            .query_row([path.as_str()], |row| row.get(0))
            .ok()
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
            &format!("DELETE FROM journal WHERE op = 'w' AND ({SUBTREE})"),
            [path.as_str()],
        );
        self.exec(
            &format!("DELETE FROM signatures WHERE {SUBTREE}"),
            [path.as_str()],
        );
    }

    pub fn remap(&mut self, from: &RelPath, to: &RelPath) {
        self.exec(
            &format!("DELETE FROM signatures WHERE {SUBTREE}"),
            [to.as_str()],
        );
        self.exec(
            &format!("UPDATE OR REPLACE signatures SET path = ?2 || substr(path, length(?1) + 1) WHERE {SUBTREE}"),
            rusqlite::params![from.as_str(), to.as_str()],
        );
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
            self.exec(
                "DELETE FROM journal WHERE op = 'w' AND path = ?1",
                [p.as_str()],
            );
            let dest = rebase(&p);
            if self.dirty.insert(dest.clone()) {
                self.exec(
                    "INSERT INTO journal(op, path) VALUES ('w', ?1)",
                    [dest.as_str()],
                );
            }
        }
    }

    pub(crate) fn local_only(&self, path: &RelPath) -> bool {
        !self.observations.contains_key(path) && self.dirty.contains(path)
    }
}
