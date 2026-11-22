//! Connection admission control.
//!
//! Each listener shares a [`ConnLimiter`] that bounds the number of
//! simultaneously open connections, both globally and per remote IP. This keeps
//! a single host from exhausting sockets, worker tasks or memory by opening many
//! connections and holding them until the idle timeout fires.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

/// A permit that releases its slots when dropped.
pub struct ConnGuard {
    limiter: Arc<Inner>,
    ip: IpAddr,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut state = self.limiter.state.lock().expect("conn limiter poisoned");
        state.total = state.total.saturating_sub(1);
        if let Some(n) = state.per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                state.per_ip.remove(&self.ip);
            }
        }
    }
}

struct State {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

struct Inner {
    max_total: usize,
    max_per_ip: usize,
    state: Mutex<State>,
}

/// Admits or rejects new connections for one listener.
#[derive(Clone)]
pub struct ConnLimiter(Arc<Inner>);

impl ConnLimiter {
    pub fn new(max_total: usize, max_per_ip: usize) -> Self {
        ConnLimiter(Arc::new(Inner {
            max_total: max_total.max(1),
            max_per_ip: max_per_ip.max(1),
            state: Mutex::new(State { total: 0, per_ip: HashMap::new() }),
        }))
    }

    /// Try to admit a connection from `ip`. Returns a guard that must be held for
    /// the lifetime of the connection, or `None` when a limit is reached.
    pub fn admit(&self, ip: IpAddr) -> Option<ConnGuard> {
        let mut state = self.0.state.lock().expect("conn limiter poisoned");
        if state.total >= self.0.max_total {
            return None;
        }
        let entry = state.per_ip.entry(ip).or_insert(0);
        if *entry >= self.0.max_per_ip {
            return None;
        }
        *entry += 1;
        state.total += 1;
        Some(ConnGuard { limiter: self.0.clone(), ip })
    }
}
