use std::io;

use crate::path::RelPath;
use crate::port::LocalTree;
use crate::sdk::Error as SdkError;

use super::Engine;
use crate::model::{Conflict, Observation, Operation, Plan};

pub(crate) enum Outcome {
    Saved {
        obs: Option<Observation>,
        sig: Option<Vec<u8>>,
        reedited: bool,
    },
    Diverted {
        theirs: Option<Observation>,
        copy: RelPath,
        obs: Option<Observation>,
        sig: Option<Vec<u8>>,
        conflict: Conflict,
    },
    Moved,
    MoveLost {
        theirs: Option<Observation>,
        resurrect: Option<RelPath>,
        conflict: Option<Conflict>,
    },
    Removed,
    RemoveLost {
        theirs: Observation,
        conflict: Conflict,
    },
    Busy,
    Failed(io::Error),
}

impl<T: LocalTree> Engine<T> {
    pub(crate) async fn replay(&self, plan: &Plan) -> Outcome {
        match plan {
            Plan::Save {
                path,
                replaces,
                reuses,
            } => self.replay_save(path, *replaces, reuses.as_ref()).await,
            Plan::Move { from, to, moves } => self.replay_move(from, to, *moves).await,
            Plan::Remove { path, removes } => self.replay_remove(path, *removes).await,
        }
    }

    async fn replay_move(&self, from: &RelPath, to: &RelPath, moves: Observation) -> Outcome {
        if self.is_frozen(from) || self.is_frozen(to) {
            return Outcome::Busy;
        }
        match self.sdk.stat(&from.as_file()).await {
            Ok(info) if Observation::of(&info) == moves => {
                match self.sdk.mv(&from.as_file(), &to.as_file()).await {
                    Ok(()) => Outcome::Moved,
                    Err(SdkError::NotFound) => self.move_lost(from, to, moves, None),
                    Err(err) => Outcome::Failed(err.into()),
                }
            }
            Ok(info) => self.move_lost(from, to, moves, Some(Observation::of(&info))),
            Err(SdkError::NotFound) => self.move_lost(from, to, moves, None),
            Err(err) => Outcome::Failed(err.into()),
        }
    }

    fn move_lost(
        &self,
        from: &RelPath,
        to: &RelPath,
        moves: Observation,
        theirs: Option<Observation>,
    ) -> Outcome {
        let resurrect = self.tree.backing(to).is_file().then(|| to.clone());
        let lost = theirs.is_some() || resurrect.is_none();
        Outcome::MoveLost {
            theirs,
            resurrect,
            conflict: lost.then(|| {
                Conflict::new(
                    Operation::Rename(from.clone(), to.clone()),
                    theirs.map(|_| moves),
                    theirs,
                    None,
                )
            }),
        }
    }

    async fn replay_remove(&self, path: &RelPath, removes: Observation) -> Outcome {
        if self.is_frozen(path) {
            return Outcome::Busy;
        }
        match self.sdk.stat(&path.as_file()).await {
            Err(SdkError::NotFound) => Outcome::Removed,
            Ok(info) if Observation::of(&info) == removes => {
                match self.sdk.rm(&path.as_file()).await {
                    Ok(()) | Err(SdkError::NotFound) => Outcome::Removed,
                    Err(err) => Outcome::Failed(err.into()),
                }
            }
            Ok(info) => {
                let theirs = Observation::of(&info);
                Outcome::RemoveLost {
                    theirs,
                    conflict: Conflict::new(
                        Operation::Delete(path.clone()),
                        Some(removes),
                        Some(theirs),
                        None,
                    ),
                }
            }
            Err(err) => Outcome::Failed(err.into()),
        }
    }
}
