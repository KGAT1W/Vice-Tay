//! Queue membership is still serialized with the calendar. For a bounded
//! descriptor heap use its slot number as an index into packed live records.
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Store {
    other: BTreeMap<u32, super::Waiting>,
    positions: Vec<u32>, // 0 absent, otherwise 1 + row index
    rows: Vec<(u32, super::Waiting)>,
}

impl Store {
    pub(super) fn indexed(count: u32) -> Self {
        Self { positions: vec![0; count as usize], ..Default::default() }
    }

    pub(super) fn get(&self, i: &u32) -> Option<&super::Waiting> {
        match self.positions.get(*i as usize) {
            Some(&p) => p.checked_sub(1).map(|p| &self.rows[p as usize].1),
            None => self.other.get(i),
        }
    }
    pub(super) fn get_mut(&mut self, i: &u32) -> Option<&mut super::Waiting> {
        match self.positions.get(*i as usize) {
            Some(&p) => p.checked_sub(1).map(|p| &mut self.rows[p as usize].1),
            None => self.other.get_mut(i),
        }
    }
    pub(super) fn remove(&mut self, i: &u32) -> Option<super::Waiting> {
        let Some(p) = self.positions.get_mut(*i as usize) else { return self.other.remove(i) };
        let at = std::mem::take(p).checked_sub(1)? as usize;
        let (_, old) = self.rows.swap_remove(at);
        if let Some((moved, _)) = self.rows.get(at) { self.positions[*moved as usize] = at as u32 + 1; }
        Some(old)
    }
    /// Apply `change` once. Returns true only if a new member was inserted.
    pub(super) fn change(&mut self, i: u32, change: impl FnOnce(Option<&super::Waiting>) -> Option<super::Waiting>) -> bool {
        let Some(p) = self.positions.get_mut(i as usize) else {
            use std::collections::btree_map::Entry;
            return match self.other.entry(i) {
                Entry::Occupied(mut e) => { if let Some(v) = change(Some(e.get())) { e.insert(v); } false }
                Entry::Vacant(e) => { if let Some(v) = change(None) { e.insert(v); true } else { false } }
            };
        };
        if *p == 0 {
            if let Some(v) = change(None) {
                self.rows.push((i, v));
                *p = self.rows.len() as u32;
                return true;
            }
        } else {
            let row = &mut self.rows[(*p - 1) as usize].1;
            if let Some(v) = change(Some(row)) { *row = v; }
        }
        false
    }
    pub(super) fn len(&self) -> usize { self.rows.len() + self.other.len() }
    pub(super) fn iter(&self) -> impl Iterator<Item=(&u32, &super::Waiting)> {
        self.rows.iter().map(|(i, v)| (i,v)).chain(self.other.iter())
    }
    #[cfg(test)]
    pub(super) fn contains_key(&self, i: &u32) -> bool { self.get(i).is_some() }
}
