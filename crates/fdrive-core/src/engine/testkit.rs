use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use httpmock::MockServer;

use crate::engine::{Engine, Observation};
use crate::path::RelPath;
use crate::port::LocalTree;
use crate::sdk::Sdk;

pub(super) struct TempTree {
    pub(super) dir: PathBuf,
    pub(super) state: PathBuf,
    pub(super) settled: Mutex<Vec<RelPath>>,
}

impl TempTree {
    pub(super) fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fdrive-engine-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self {
            state: dir.with_extension("ledger.json"),
            dir,
            settled: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn write(&self, path: &str, content: &[u8]) {
        let abs = self.dir.join(path);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, content).unwrap();
    }

    pub(super) fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.dir.join(path)).ok()
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
        let _ = fs::remove_file(&self.state);
    }
}

impl LocalTree for TempTree {
    fn backing(&self, path: &RelPath) -> PathBuf {
        self.dir.join(path.as_str())
    }

    fn relocate(&self, from: &RelPath, to: &RelPath) -> std::io::Result<()> {
        fs::rename(self.backing(from), self.backing(to))
    }

    fn settled(&self, target: &RelPath, _mtime: Option<SystemTime>) {
        self.settled.lock().unwrap().push(target.clone());
    }

    fn ledger(&self) -> PathBuf {
        self.state.clone()
    }
}

pub(super) fn engine(server: &MockServer) -> Arc<Engine<TempTree>> {
    engine_with(server, TempTree::new())
}

pub(super) fn engine_with(server: &MockServer, tree: TempTree) -> Arc<Engine<TempTree>> {
    let mut sdk = Sdk::new(&server.base_url()).unwrap();
    sdk.set_token("TOKEN".into());
    Engine::start(Arc::new(sdk), tokio::runtime::Handle::current(), tree)
}

pub(super) async fn settle(engine: &Engine<TempTree>) {
    engine.flush(Duration::from_secs(10)).await;
}

pub(super) const MTIME: &str = "Wed, 21 Oct 2015 07:28:00 GMT";

pub(super) fn observed(size: u64) -> Observation {
    Observation::new(size, Some(httpdate::parse_http_date(MTIME).unwrap()))
}
