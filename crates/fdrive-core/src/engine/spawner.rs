use std::sync::{Arc, Weak};

use crate::port::LocalTree;

use super::Engine;

pub(super) struct Spawner<T: LocalTree> {
    pub(super) rt: tokio::runtime::Handle,
    pub(super) weak: Weak<Engine<T>>,
}

impl<T: LocalTree> Spawner<T> {
    pub(super) fn spawn<F, Fut>(&self, f: F)
    where
        F: FnOnce(Arc<Engine<T>>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let Some(engine) = self.weak.upgrade() else {
            return;
        };
        self.rt.spawn(f(engine));
    }
}
