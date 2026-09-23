//! [`SessionHub`]: the one place actions enter. Each [`dispatch`](SessionHub::dispatch) reduces
//! the action into the `Session`, publishes the new state on the `watch` channel the UI mirrors,
//! and fans the action out on a `broadcast` channel (the store writer and anything else that
//! wants the log).

use std::sync::{Mutex, PoisonError};

use tokio::sync::{broadcast, watch};

use crate::action::Action;
use crate::state::{Session, reduce};

/// Actions a slow subscriber may fall behind by before it loses some (it then gets
/// `RecvError::Lagged` with the count). A replay of a 12 minute fixture dispatches about 400; a
/// live hour is a few thousand spread over the hour.
pub const DEFAULT_ACTION_CAPACITY: usize = 16_384;

pub struct SessionHub {
    /// The reducer's copy. The lock is held across reduce + publish + broadcast so the order of
    /// the action log always matches the order the state saw.
    state: Mutex<Session>,
    watch_tx: watch::Sender<Session>,
    actions_tx: broadcast::Sender<Action>,
}

impl SessionHub {
    pub fn new(capacity: usize) -> Self {
        let (watch_tx, _) = watch::channel(Session::default());
        let (actions_tx, _) = broadcast::channel(capacity.max(1));
        Self {
            state: Mutex::new(Session::default()),
            watch_tx,
            actions_tx,
        }
    }

    /// Reduce `action`, publish the new state, broadcast the action.
    pub fn dispatch(&self, action: Action) {
        // A panic while holding the lock can only come from a bug in `reduce`; the state it left
        // is still the best there is, so carry on with it rather than poisoning every caller.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        reduce(&mut state, &action);
        self.watch_tx.send_replace(state.clone());
        // No subscribers is fine: a session with nothing persisting it is still a session.
        let _ = self.actions_tx.send(action);
    }

    /// The `watch` channel the UI mirrors.
    pub fn watch(&self) -> watch::Receiver<Session> {
        self.watch_tx.subscribe()
    }

    /// Every action from now on, in dispatch order.
    pub fn subscribe(&self) -> broadcast::Receiver<Action> {
        self.actions_tx.subscribe()
    }

    /// A copy of the current state.
    pub fn snapshot(&self) -> Session {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Default for SessionHub {
    fn default() -> Self {
        Self::new(DEFAULT_ACTION_CAPACITY)
    }
}
