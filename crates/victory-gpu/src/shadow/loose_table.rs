//! Serialization of loose-window creation, publication and removal.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard};
use std::time::Instant;

type Key = (u64, u64);
pub(super) type Map<V> = BTreeMap<Key, Arc<V>>;

pub(super) struct Table<V> {
    map: RwLock<Map<V>>,
    writers: Mutex<()>,
    split: OnceLock<bool>,
}

impl<V> Table<V> {
    pub(super) const fn new() -> Self {
        Self { map: RwLock::new(BTreeMap::new()), writers: Mutex::new(()), split: OnceLock::new() }
    }

    fn split(&self) -> bool {
        *self.split.get_or_init(|| std::env::var_os("VICTORY_RX_WINDOW_LOCK").is_some_and(|v| v == "1"))
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, Map<V>> {
        self.map.read().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn census<R>(&self, site: &'static str, f: impl FnOnce(&Map<V>) -> R) -> R {
        if self.split() {
            owned(self.read(), site, "read", |map| f(&map))
        } else {
            owned(self.map.write().unwrap_or_else(|e| e.into_inner()), site, "write", |map| f(&map))
        }
    }

    pub(super) fn remove<R>(&self, site: &'static str, f: impl FnOnce(&mut Map<V>) -> R) -> R {
        let serial = self.split().then(|| self.writers.lock().unwrap_or_else(|e| e.into_inner()));
        let map = self.map.write().unwrap_or_else(|e| e.into_inner());
        owned((map, serial), site, "write", |(mut map, _serial)| f(&mut map))
    }

    // Select under the writer lock, including flexible asks (longest
    // holder/tail) and named asks that missed before reserving the writer.
    pub(super) fn resolve(
        &self, select: impl FnOnce(&Map<V>) -> Key, note: impl FnOnce(&V), make: impl FnOnce(Key) -> Option<Arc<V>>,
    ) -> Option<Arc<V>> {
        if !self.split() {
            let map = self.map.write().unwrap_or_else(|e| e.into_inner());
            return owned(map, "lookup", "write", |mut map| {
                let key = select(&map);
                if let Some(v) = map.get(&key) {
                    note(v);
                    return Some(v.clone());
                }
                let v = make(key)?;
                note(&v);
                map.insert(key, v.clone());
                Some(v)
            });
        }
        // Only mutators take this gate, always before the map lock. A miss
        // can't spend the budget twice, and an in-place invalidation can't
        // slip between opening title memory and publishing the window.
        let serial = self.writers.lock().unwrap_or_else(|e| e.into_inner());
        owned(serial, "lookup", "writer gate", |_serial| {
            let key = {
                // Flexible hits originally noted `used` exclusively. Keep
                // that order too: two concurrent relaxed stores could
                // otherwise leave the older moment in the window.
                let map = self.map.write().unwrap_or_else(|e| e.into_inner());
                let key = select(&map);
                if let Some(v) = map.get(&key) {
                    note(v);
                    return Some(v.clone());
                }
                key
            };
            let v = make(key)?;
            let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
            note(&v);
            map.insert(key, v.clone());
            Some(v)
        })
    }
}

// Independent A/B probe; no timer on read hits. The interval begins after
// acquisition and the guard is released before entering the tracing sink.
fn owned<G, R>(guard: G, site: &'static str, owner: &'static str, f: impl FnOnce(G) -> R) -> R {
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_RX_WINDOW_LOCK_TIMING").is_some_and(|v| v == "1")) {
        return f(guard);
    }
    let start = Instant::now();
    let result = f(guard);
    let us = start.elapsed().as_micros() as u64;
    if us >= 1_000 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SLOW: AtomicU32 = AtomicU32::new(0);
        if SLOW.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < 64).then(|| n + 1)).is_ok() {
            tracing::info!(target: "victory_gpu::heap_close", site, owner, us, tid = crate::host::thread_id(),
                frame = crate::frame::current(), "slow loose-window lock owner");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{TryLockError, mpsc};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn creation_leaves_old_windows_readable_and_invalidates_after_publication() {
        let table = Table::new();
        table.split.set(true).unwrap();
        let old = (0, 64);
        let new = (64, 64);
        let first = table.resolve(|_| old, |_| {}, |_| Some(Arc::new(0))).unwrap();
        std::thread::scope(|s| {
            let (inside, entered) = mpsc::channel();
            let (resume, paused) = mpsc::channel();
            let t = &table;
            let creator = s.spawn(move || t.resolve(|_| new, |_| {}, |_| {
                inside.send(()).unwrap();
                paused.recv().unwrap();
                Some(Arc::new(1))
            }).unwrap());
            entered.recv().unwrap();
            // No timeout or assumption about thread scheduling. Try while
            // the creator is inside the simulated D3D call.
            let readable = t.map.try_read().is_ok_and(|map| Arc::ptr_eq(&first, &map[&old]) && !map.contains_key(&new));
            let reserved = matches!(t.writers.try_lock(), Err(TryLockError::WouldBlock));
            let invalidate = s.spawn(move || t.remove("test invalidate", |map| map.remove(&new)));
            resume.send(()).unwrap();
            let published = creator.join().unwrap();
            let removed = invalidate.join().unwrap().unwrap();
            assert!(readable && reserved);
            assert!(Arc::ptr_eq(&published, &removed));
            assert!(!t.read().contains_key(&new));
        });
    }

    #[test]
    fn overlapping_miss_selects_the_published_holder_and_notes_only_that_copy() {
        // A named miss is making a large window. A flexible ask that didn't
        // see it yet must choose it after the creator, not make its fallback.
        for split in [false, true] {
            let table = Table::new();
            table.split.set(split).unwrap();
            let large = (0, 256);
            let fallback = (64, 64);
            let note = |n: &AtomicUsize| { n.fetch_add(1, Ordering::Relaxed); };
            std::thread::scope(|s| {
                let (inside, entered) = mpsc::channel();
                let (resume, paused) = mpsc::channel();
                let t = &table;
                let creator = s.spawn(move || t.resolve(|_| large, note, |_| {
                    inside.send(()).unwrap();
                    paused.recv().unwrap();
                    Some(Arc::new(AtomicUsize::new(0)))
                }).unwrap());
                entered.recv().unwrap();
                let other = s.spawn(move || t.resolve(
                    |map| super::super::holder(map.keys().copied(), 70, 80).unwrap_or(fallback),
                    note, |_| panic!("allocated a second, aliasing window"),
                ).unwrap());
                resume.send(()).unwrap();
                let a = creator.join().unwrap();
                let b = other.join().unwrap();
                assert!(Arc::ptr_eq(&a, &b));
                assert_eq!(a.load(Ordering::Relaxed), 2);
                assert_eq!(t.read().len(), 1);
            });
        }
    }
}
