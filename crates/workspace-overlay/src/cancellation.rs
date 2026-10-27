//! Synchronous cancellation with wakeable, bounded waits.
use parking_lot::{Condvar, Mutex};
use std::{
    sync::{Arc, Weak},
    time::Duration,
};
#[derive(Default)]
struct State {
    cancelled: Mutex<bool>,
    wake: Condvar,
    children: Mutex<Vec<Weak<State>>>,
}
/// A clonable cancellation signal shared by a session and its subprocesses.
#[derive(Clone, Default)]
pub struct Cancellation {
    state: Arc<State>,
}
impl Cancellation {
    pub fn cancel(&self) {
        {
            let mut cancelled = self.state.cancelled.lock();
            *cancelled = true;
            self.state.wake.notify_all();
        }
        let children = self
            .state
            .children
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        for state in children {
            Self { state }.cancel();
        }
    }
    /// A child observes this signal; cancelling the child leaves its parent and siblings running.
    pub fn child(&self) -> Self {
        let child = Self::default();
        let mut children = self.state.children.lock();
        children.retain(|child| child.strong_count() > 0);
        children.push(Arc::downgrade(&child.state));
        drop(children);
        // Either this check or a concurrent parent's traversal observes the child.
        if self.is_cancelled() {
            child.cancel();
        }
        child
    }
    pub fn is_cancelled(&self) -> bool {
        *self.state.cancelled.lock()
    }
    /// Wait until cancellation or the timeout; true means cancelled.
    pub fn wait(&self, duration: Duration) -> bool {
        let mut cancelled = self.state.cancelled.lock();
        self.state
            .wake
            .wait_while_for(&mut cancelled, |cancelled| !*cancelled, duration);
        *cancelled
    }
    pub(crate) fn check(&self) -> crate::Result<()> {
        if self.is_cancelled() {
            Err(crate::Error::Cancelled)
        } else {
            Ok(())
        }
    }
}
