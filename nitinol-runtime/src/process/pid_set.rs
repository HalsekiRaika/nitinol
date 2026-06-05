//! Pid set modelled after ProtoActor.go's `PIDSet`.
//!
//! A doubly-linked list threaded through a `HashMap<Pid, Node>` maintains
//! insertion order with O(1) add, O(1) remove, O(1) contains, and storage
//! strictly bounded by the number of live entries. No tombstones accumulate,
//! so a long-lived parent with high child churn does not leak memory.

use std::collections::HashMap;

use crate::ident::Pid;

#[derive(Debug, Clone)]
struct Node {
    prev: Option<Pid>,
    next: Option<Pid>,
}

/// Ordered set of `Pid`s backed by a doubly-linked list threaded through a
/// `HashMap<Pid, Node>`.
///
/// Public API is read-only (`len` / `is_empty` / `contains` / `iter`);
/// mutation is restricted to the runtime crate so user code cannot corrupt the
/// parent/child bookkeeping maintained by the lifecycle loop.
///
/// All operations are O(1) and internal storage is bounded by the number of
/// live entries — no tombstones accumulate when children are removed.
pub struct PidSet {
    nodes: HashMap<Pid, Node>,
    head: Option<Pid>,
    tail: Option<Pid>,
}

struct PidSetIter<'a> {
    nodes: &'a HashMap<Pid, Node>,
    current: Option<Pid>,
}

impl<'a> Iterator for PidSetIter<'a> {
    type Item = &'a Pid;

    fn next(&mut self) -> Option<Self::Item> {
        let pid = self.current?;
        let (key, node) = self.nodes.get_key_value(&pid)?;
        self.current = node.next;
        Some(key)
    }
}

struct PidSetRevIter<'a> {
    nodes: &'a HashMap<Pid, Node>,
    current: Option<Pid>,
}

impl<'a> Iterator for PidSetRevIter<'a> {
    type Item = &'a Pid;

    fn next(&mut self) -> Option<Self::Item> {
        let pid = self.current?;
        let (key, node) = self.nodes.get_key_value(&pid)?;
        self.current = node.prev;
        Some(key)
    }
}

impl PidSet {
    pub(crate) fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            head: None,
            tail: None,
        }
    }

    pub(crate) fn add(&mut self, pid: Pid) -> bool {
        if self.nodes.contains_key(&pid) {
            return false;
        }
        let node = Node {
            prev: self.tail,
            next: None,
        };
        // Update old tail to point forward to the new entry.
        if let Some(tail_pid) = self.tail {
            self.nodes.get_mut(&tail_pid).unwrap().next = Some(pid);
        }
        self.nodes.insert(pid, node);
        self.tail = Some(pid);
        if self.head.is_none() {
            self.head = Some(pid);
        }
        true
    }

    pub(crate) fn remove(&mut self, pid: &Pid) -> bool {
        let Some(node) = self.nodes.remove(pid) else {
            return false;
        };
        // Stitch prev and next together, bypassing the removed entry.
        match node.prev {
            Some(prev_pid) => self.nodes.get_mut(&prev_pid).unwrap().next = node.next,
            None => self.head = node.next,
        }
        match node.next {
            Some(next_pid) => self.nodes.get_mut(&next_pid).unwrap().prev = node.prev,
            None => self.tail = node.prev,
        }
        true
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn contains(&self, pid: &Pid) -> bool {
        self.nodes.contains_key(pid)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Pid> {
        PidSetIter {
            nodes: &self.nodes,
            current: self.head,
        }
    }

    pub(crate) fn iter_rev(&self) -> impl Iterator<Item = &Pid> {
        PidSetRevIter {
            nodes: &self.nodes,
            current: self.tail,
        }
    }

    /// Returns the number of entries in the internal backing store.
    ///
    /// In this linked-list implementation `internal_len() == len()` always
    /// holds because removed entries are fully dropped from `nodes`. The method
    /// exists so tests can assert that internal storage is bounded by the live
    /// count — not by cumulative spawn count as a tombstone implementation
    /// would be.
    #[cfg(test)]
    fn internal_len(&self) -> usize {
        self.nodes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_preserves_insertion_order_and_dedupes() {
        let mut set = PidSet::new();
        let p1 = Pid::next();
        let p2 = Pid::next();
        let p3 = Pid::next();
        assert!(set.add(p1));
        assert!(set.add(p2));
        assert!(set.add(p3));
        assert!(!set.add(p2));

        let observed: Vec<Pid> = set.iter().copied().collect();
        assert_eq!(observed, vec![p1, p2, p3]);
        assert_eq!(set.len(), 3);
        assert!(!set.is_empty());
        assert!(set.contains(&p1));
    }

    /// Spec compliance (order.md): PidSet must preserve insertion order after
    /// mid-set removal. Linked-list deletion keeps the live elements in their
    /// original positions.
    ///
    /// Regression for family_tag `spec-noncompliance`
    /// (ARCH-REVIEW-013 / VAL-NEW-pidset-order-regression-L44):
    /// After removing p2 from [p1, p2, p3, p4], the remaining order must be
    /// [p1, p3, p4] — NOT [p1, p4, p3] as swap-remove would produce.
    #[test]
    fn remove_preserves_insertion_order() {
        let mut set = PidSet::new();
        let p1 = Pid::next();
        let p2 = Pid::next();
        let p3 = Pid::next();
        let p4 = Pid::next();
        set.add(p1);
        set.add(p2);
        set.add(p3);
        set.add(p4);

        // Removing p2 (second element): p3 and p4 stay in place.
        // Resulting live order: [p1, p3, p4]
        assert!(set.remove(&p2));
        assert_eq!(set.len(), 3);
        assert!(!set.contains(&p2));

        let order: Vec<Pid> = set.iter().copied().collect();
        assert_eq!(order, vec![p1, p3, p4]);

        // iter_rev reflects the original insertion order reversed for live PIDs.
        let rev: Vec<Pid> = set.iter_rev().copied().collect();
        assert_eq!(rev, vec![p4, p3, p1]);
    }

    #[test]
    fn remove_last_element_does_not_affect_others() {
        let mut set = PidSet::new();
        let p1 = Pid::next();
        let p2 = Pid::next();
        set.add(p1);
        set.add(p2);

        assert!(set.remove(&p2));
        assert_eq!(set.len(), 1);
        assert!(set.contains(&p1));
        assert!(!set.contains(&p2));

        let order: Vec<Pid> = set.iter().copied().collect();
        assert_eq!(order, vec![p1]);
    }

    #[test]
    fn remove_absent_returns_false() {
        let mut set = PidSet::new();
        let p = Pid::next();
        assert!(!set.remove(&p));
    }

    /// Regression for family_tag `spec-noncompliance`
    /// (VAL-NEW-reverse-stop-after-removal-L312):
    /// `iter_rev` after removal of a non-last element must yield live PIDs in
    /// reverse insertion order.
    #[test]
    fn iter_rev_reflects_insertion_order_after_removal() {
        let mut set = PidSet::new();
        let p1 = Pid::next();
        let p2 = Pid::next();
        let p3 = Pid::next();
        set.add(p1);
        set.add(p2);
        set.add(p3);

        // Without any removal: iter_rev is [p3, p2, p1]
        let rev: Vec<Pid> = set.iter_rev().copied().collect();
        assert_eq!(rev, vec![p3, p2, p1]);

        // Remove p1 (head): live order [p2, p3]; iter_rev must be [p3, p2].
        set.remove(&p1);
        let rev: Vec<Pid> = set.iter_rev().copied().collect();
        assert_eq!(rev, vec![p3, p2]);
    }

    #[test]
    fn remove_all_elements_empties_set() {
        let mut set = PidSet::new();
        let pids: Vec<Pid> = (0..5).map(|_| Pid::next()).collect();
        for &p in &pids {
            set.add(p);
        }
        for &p in &pids {
            assert!(set.remove(&p), "expected remove to succeed for live pid");
        }
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        for &p in &pids {
            assert!(!set.contains(&p));
        }
        assert_eq!(set.iter().count(), 0);
    }

    /// Regression for family_tag `spec-noncompliance` (ARCH-REVIEW-022):
    /// repeated mid-set removals and re-insertions must keep `contains` and
    /// `len` consistent with the live set — no stale index entries.
    #[test]
    fn remove_o1_and_contains_consistent_after_churn() {
        let mut set = PidSet::new();
        let pids: Vec<Pid> = (0..5).map(|_| Pid::next()).collect();
        for &p in &pids {
            set.add(p);
        }

        // Remove pids[1] and pids[3]. Remaining live: pids[0], pids[2], pids[4].
        assert!(set.remove(&pids[1]));
        assert!(set.remove(&pids[3]));
        assert_eq!(set.len(), 3);

        // All removed pids are absent; remaining pids are present.
        assert!(!set.contains(&pids[1]));
        assert!(!set.contains(&pids[3]));
        assert!(set.contains(&pids[0]));
        assert!(set.contains(&pids[2]));
        assert!(set.contains(&pids[4]));

        // Re-insert a new pid — must be present and counted.
        let p_new = Pid::next();
        assert!(set.add(p_new));
        assert_eq!(set.len(), 4);
        assert!(set.contains(&p_new));

        let rev: Vec<Pid> = set.iter_rev().copied().collect();
        assert_eq!(rev.len(), 4);
        // p_new was inserted last — it appears first in reversed order.
        assert_eq!(rev[0], p_new);
    }

    /// Regression for family_tag `scalability-violation` (ARCH-REVIEW-024):
    /// after churn, internal storage must not grow proportional to the
    /// cumulative spawn count — only proportional to the max concurrent count.
    ///
    /// With the doubly-linked list implementation `internal_len() == len()`
    /// always, so both are checked here.
    #[test]
    fn storage_is_bounded_by_max_concurrent_not_cumulative() {
        let mut set = PidSet::new();
        let max_concurrent = 3;
        let mut active: Vec<Pid> = Vec::new();

        // Simulate 100 child spawns, keeping at most `max_concurrent` alive.
        for _ in 0..100 {
            let p = Pid::next();
            set.add(p);
            active.push(p);
            if active.len() > max_concurrent {
                let oldest = active.remove(0);
                set.remove(&oldest);
            }
        }

        // len() == number of live children, never the cumulative spawn count.
        assert_eq!(set.len(), active.len());
        assert!(
            set.len() <= max_concurrent + 1,
            "expected at most {} live entries, got {}",
            max_concurrent + 1,
            set.len()
        );

        // Internal storage equals live count — no tombstones, no growth.
        assert_eq!(
            set.internal_len(),
            set.len(),
            "internal storage must match live count (no hidden tombstones)"
        );

        // Verify storage does not retain entries proportional to cumulative spawns.
        // 100 spawns with max 3 concurrent → internal storage must be ≤ max_concurrent+1.
        assert!(
            set.internal_len() <= max_concurrent + 1,
            "internal storage grew to {} after 100 cumulative spawns; must be ≤ {}",
            set.internal_len(),
            max_concurrent + 1
        );

        // After removing all, set is fully empty.
        for p in active.clone() {
            set.remove(&p);
        }
        assert_eq!(set.len(), 0);
        assert!(set.is_empty());
        assert_eq!(set.internal_len(), 0);
    }
}
