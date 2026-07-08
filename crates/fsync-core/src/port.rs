//! The port: what the core asks of a platform. Owned by the core and
//! grown by extraction — an operation lands here when a second adapter
//! demonstrably duplicates the first, never on speculation.

use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::path::RelPath;

pub trait LocalTree: Send + Sync + 'static {
    fn backing(&self, path: &RelPath) -> PathBuf;

    fn relocate(&self, from: &RelPath, to: &RelPath) -> io::Result<()>;

    fn settled(&self, target: &RelPath, mtime: Option<SystemTime>);

    fn ledger(&self) -> PathBuf;
}
