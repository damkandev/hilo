//! Leases: "this agent session is editing this file". Advisory only; the anchor's ordering rule
//! still protects every write. The anchor owns the table and broadcasts it; every daemon answers
//! hooks from its own copy, so hooks never wait on the network.
#![forbid(unsafe_code)]
use crate::sync::Key;
use crate::wire::Lease;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const TTL: Duration = Duration::from_secs(120);
pub const MAX_PER_SESSION: usize = 256;

#[derive(Clone, Debug)]
pub struct Entry {
    pub session: String,
    pub machine: String,
    pub since: Instant,
    pub expires: Instant,
}

#[derive(Default)]
pub struct Table(pub HashMap<Key, Entry>);

impl Table {
    pub fn holder(&self, wt: &str, path: &str) -> Option<&Entry> {
        self.0.get(&(wt.to_string(), path.to_string())).filter(|e| e.expires > Instant::now())
    }

    /// Grants (or renews) unless another live session holds it.
    pub fn claim(&mut self, wt: &str, path: &str, session: &str, machine: &str) -> Option<Lease> {
        if self.holder(wt, path).is_some_and(|e| e.session != session) {
            return None;
        }
        let held = self.0.values().filter(|e| e.session == session && e.expires > Instant::now()).count();
        let k = (wt.to_string(), path.to_string());
        let now = Instant::now();
        let since = match self.0.get(&k) {
            Some(e) if e.session == session && e.expires > now => e.since,
            _ if held >= MAX_PER_SESSION => return None,
            _ => now,
        };
        self.0.insert(k, Entry { session: session.into(), machine: machine.into(), since, expires: now + TTL });
        Some(Lease {
            wt: wt.into(),
            path: path.into(),
            session: session.into(),
            machine: machine.into(),
            age_ms: (now - since).as_millis() as u64,
            ttl_ms: TTL.as_millis() as u64,
        })
    }

    /// Releases one path, or every lease of the session when `wt` is empty.
    pub fn release(&mut self, wt: &str, path: &str, session: &str) -> Vec<Lease> {
        let gone: Vec<Key> = self
            .0
            .iter()
            .filter(|(k, e)| e.session == session && (wt.is_empty() || (k.0 == wt && k.1 == path)))
            .map(|(k, _)| k.clone())
            .collect();
        gone.into_iter()
            .map(|k| {
                let e = self.0.remove(&k).unwrap();
                Lease { wt: k.0, path: k.1, session: e.session, machine: e.machine, age_ms: 0, ttl_ms: 0 }
            })
            .collect()
    }

    /// Mirror a broadcast from the anchor. Durations are relative, so clocks don't need to agree.
    pub fn apply(&mut self, l: &Lease) {
        let k = (l.wt.clone(), l.path.clone());
        if l.ttl_ms == 0 {
            if self.0.get(&k).is_some_and(|e| e.session == l.session) {
                self.0.remove(&k);
            }
            return;
        }
        let now = Instant::now();
        let since = now.checked_sub(Duration::from_millis(l.age_ms)).unwrap_or(now);
        self.0.insert(k, Entry { session: l.session.clone(), machine: l.machine.clone(), since, expires: now + Duration::from_millis(l.ttl_ms) });
    }

    pub fn snapshot(&self) -> Vec<Lease> {
        let now = Instant::now();
        self.0
            .iter()
            .filter(|(_, e)| e.expires > now)
            .map(|(k, e)| Lease {
                wt: k.0.clone(),
                path: k.1.clone(),
                session: e.session.clone(),
                machine: e.machine.clone(),
                age_ms: (now - e.since).as_millis() as u64,
                ttl_ms: (e.expires - now).as_millis() as u64,
            })
            .collect()
    }
}

/// The text a blocked agent reads.
pub fn busy_message(path: &str, e: &Entry) -> String {
    let now = Instant::now();
    format!(
        "hilo: {path} is being edited by another agent ({}, session {}, {} s ago).\nWork on another file or wait; the lease expires in {} s unless renewed.",
        e.machine,
        e.session.chars().take(4).collect::<String>(),
        (now - e.since).as_secs(),
        e.expires.saturating_duration_since(now).as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claims() {
        let mut t = Table::default();
        assert!(t.claim("w", "a", "s1", "mac").is_some());
        assert!(t.claim("w", "a", "s2", "srv").is_none());
        assert!(t.claim("w2", "a", "s2", "srv").is_some()); // other worktree: no competition
        assert!(t.claim("w", "a", "s1", "mac").is_some()); // renew
        assert_eq!(t.release("", "", "s1").len(), 1);
        assert!(t.claim("w", "a", "s2", "srv").is_some());
        let mut mirror = Table::default();
        for l in t.snapshot() {
            mirror.apply(&l);
        }
        assert_eq!(mirror.holder("w", "a").unwrap().session, "s2");
    }
}
