mod cache;
mod conflict;
mod download;
mod facade;
mod gates;
mod ledger;
mod play;
mod scheduler;
mod spawner;
mod state;
mod upload;
mod view;

pub use self::{download::Download, ledger::Ledger, scheduler::UploadStatus, state::LedgerGuard};
pub(crate) use self::{gates::Frozen, play::Outcome};
pub use crate::model::{Conflict, Fate, Observation, Operation, Plan, Resolution};

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crate::path::RelPath;
use crate::port::LocalTree;
use crate::sdk::Sdk;

use self::{conflict::Conflicts, gates::Transfers, spawner::Spawner, state::State};

pub struct Engine<T: LocalTree> {
    tree: T,
    sdk: Arc<Sdk>,
    ignore: crate::config::Ignore,

    state: Mutex<State>,

    transfers: Transfers,
    frozen: Mutex<BTreeSet<RelPath>>,
    conflicts: Conflicts,

    scheduler: scheduler::Handle,
    spawner: Spawner<T>,
}

#[cfg(test)]
mod tests;
