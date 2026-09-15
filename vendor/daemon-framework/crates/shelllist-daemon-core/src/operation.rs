//! Bookkeeping only: callers decide cancellation effects, visibility and outcomes.
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy)]
pub struct OperationLimits {
    pub total: usize,
    pub per_owner: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationAdmissionError {
    Full,
    DuplicateId,
}
impl std::fmt::Display for OperationAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Full => "too many active operations; retry after an operation finishes",
            Self::DuplicateId => "operation ID is already active",
        })
    }
}
impl std::error::Error for OperationAdmissionError {}

pub struct OwnedOperation<T> {
    pub owner: Option<String>,
    pub value: T,
}

/// The caller synchronizes admission and insertion. Removing an entry claims its
/// terminal transition; a racing completion/cancellation gets None thereafter.
pub struct OwnedOperations<T> {
    active: HashMap<String, OwnedOperation<T>>,
    limits: OperationLimits,
}
impl<T> OwnedOperations<T> {
    pub fn new(limits: OperationLimits) -> Self {
        Self {
            active: HashMap::new(),
            limits,
        }
    }
    pub fn admit(&self, owner: Option<&str>) -> Result<(), OperationAdmissionError> {
        if self.active.len() >= self.limits.total
            || self
                .active
                .values()
                .filter(|entry| entry.owner.as_deref() == owner)
                .count()
                >= self.limits.per_owner
        {
            Err(OperationAdmissionError::Full)
        } else {
            Ok(())
        }
    }
    pub fn insert(
        &mut self,
        id: String,
        owner: Option<String>,
        value: T,
    ) -> Result<(), OperationAdmissionError> {
        if self.active.contains_key(&id) {
            return Err(OperationAdmissionError::DuplicateId);
        }
        self.admit(owner.as_deref())?;
        self.active.insert(id, OwnedOperation { owner, value });
        Ok(())
    }
    pub fn get_owned(&self, id: &str, owner: Option<&str>) -> Option<&T> {
        self.active
            .get(id)
            .filter(|entry| entry.owner.as_deref() == owner)
            .map(|entry| &entry.value)
    }
    pub fn get_mut(&mut self, id: &str) -> Option<&mut T> {
        self.active.get_mut(id).map(|entry| &mut entry.value)
    }
    pub fn claim(&mut self, id: &str) -> Option<OwnedOperation<T>> {
        self.active.remove(id)
    }
    pub fn claim_owned(&mut self, id: &str, owner: Option<&str>) -> Option<OwnedOperation<T>> {
        self.get_owned(id, owner)?;
        self.claim(id)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &OwnedOperation<T>)> {
        self.active.iter()
    }
}

struct Finished<T> {
    id: String,
    owner: Option<String>,
    value: T,
    recorded: Instant,
}

/// Bounded terminal results. TTL and aggregate visibility remain caller policy.
pub struct RecentResults<T> {
    entries: VecDeque<Finished<T>>,
    limit: usize,
    retention: Option<Duration>,
}
impl<T> RecentResults<T> {
    pub fn new(limit: usize, retention: Option<Duration>) -> Self {
        Self {
            entries: VecDeque::new(),
            limit,
            retention,
        }
    }
    pub fn record(&mut self, id: String, owner: Option<String>, value: T) {
        let now = Instant::now();
        self.prune(now);
        self.entries.retain(|entry| entry.id != id);
        self.entries.push_back(Finished {
            id,
            owner,
            value,
            recorded: now,
        });
        while self.entries.len() > self.limit {
            self.entries.pop_front();
        }
    }
    pub fn get_owned(&mut self, id: &str, owner: Option<&str>) -> Option<&T> {
        self.prune(Instant::now());
        self.entries
            .iter()
            .find(|entry| entry.id == id && entry.owner.as_deref() == owner)
            .map(|entry| &entry.value)
    }
    pub fn values(&mut self) -> impl Iterator<Item = &T> {
        self.prune(Instant::now());
        self.entries.iter().map(|entry| &entry.value)
    }
    pub fn prune(&mut self, now: Instant) {
        if let Some(retention) = self.retention {
            self.entries
                .retain(|entry| now.saturating_duration_since(entry.recorded) < retention);
        }
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_and_terminal_claim_are_owner_scoped() {
        let mut active = OwnedOperations::new(OperationLimits {
            total: 2,
            per_owner: 1,
        });
        active.insert("one".into(), Some("a".into()), 1).unwrap();
        assert_eq!(active.admit(Some("a")), Err(OperationAdmissionError::Full));
        active.insert("two".into(), Some("b".into()), 2).unwrap();
        assert_eq!(active.admit(Some("c")), Err(OperationAdmissionError::Full));
        assert!(active.claim_owned("one", Some("b")).is_none());
        assert_eq!(active.claim_owned("one", Some("a")).unwrap().value, 1);
        assert!(active.claim("one").is_none());
        assert!(active.admit(Some("a")).is_ok());
    }
    #[test]
    fn terminal_results_are_bounded_replaced_and_expire() {
        let ttl = Duration::from_secs(5);
        let mut recent = RecentResults::new(2, Some(ttl));
        for id in 0..3 {
            recent.record(id.to_string(), Some("a".into()), id);
        }
        assert!(recent.get_owned("0", Some("a")).is_none());
        assert!(recent.get_owned("1", Some("b")).is_none());
        recent.record("1".into(), Some("a".into()), 42);
        assert_eq!(recent.values().copied().collect::<Vec<_>>(), vec![2, 42]);
        recent.prune(Instant::now() + ttl);
        assert!(recent.is_empty());
    }
}
