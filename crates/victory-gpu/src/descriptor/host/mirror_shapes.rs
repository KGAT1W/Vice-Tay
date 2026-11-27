//! Inline placed records refer to append-only entries in `Views::shapes`.
//! Mirroring a run of views with the same shape but different addresses
//! needs the shared views lock only for the first. Ordinary table entries
//! can be freed and reused; they must still check their generation.

use std::cell::RefCell;
use std::sync::OnceLock;

use super::super::{KIND_SHIFT, PLACED, PLACED_ADDRESS_BITS, View, payload, view_at};

const SLOTS: usize = 32;
type Cache = [Option<(u32, View)>; SLOTS];

pub(super) fn lookup(i: u32, generation: u32) -> Option<View> {
    static ON: OnceLock<bool> = OnceLock::new();
    if generation >> KIND_SHIFT != PLACED || !*ON.get_or_init(|| crate::switch::on("VICTORY_RX_MIRROR_SHAPES")) {
        return view_at(i, generation);
    }
    thread_local! {
        static CACHE: RefCell<Box<Cache>> = RefCell::new(Box::new([const { None }; SLOTS]));
    }
    CACHE.with(|cache| placed(&mut cache.borrow_mut(), i, generation, view_at))
}

fn placed(cache: &mut Cache, i: u32, generation: u32, resolve: impl FnOnce(u32, u32) -> Option<View>) -> Option<View> {
    let bits = payload(i, generation);
    let shape = (bits >> PLACED_ADDRESS_BITS) as u32;
    let entry = &mut cache[shape as usize % SLOTS];
    if let Some((saved, View::Placed(unordered, resource, desc))) = entry.as_ref()
        && *saved == shape
    {
        let address = bits & ((1 << PLACED_ADDRESS_BITS) - 1);
        let mut desc = *desc;
        (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
        return Some(View::Placed(*unordered, *resource, desc));
    }
    let view = resolve(i, generation)?;
    *entry = Some((shape, view.clone()));
    Some(view)
}

#[cfg(test)]
mod tests {
    use super::super::super::{Views, placed_words};
    use super::*;

    #[test]
    fn mirror_shapes_follow_the_record_address_and_shape() {
        let mut views = Views::default();
        let mut cache = [const { None }; SLOTS];
        let mut misses = 0;
        let mut check = |views: &Views, shape, address| {
            let [i, generation] = placed_words(shape, address);
            let expected = views.placed_of(i, generation);
            assert_eq!(placed(&mut cache, i, generation, |i, generation| {
                misses += 1;
                views.placed_of(i, generation)
            }), expected);
        };
        // A not-yet-published shape is retried; the cache holds no negative entry.
        check(&views, 0, 7);
        for n in 0..=SLOTS as u32 {
            assert_eq!(views.shape((n != 0, [n; 14], [100 + n; 14])), Some(n));
        }
        check(&views, 0, 7);
        check(&views, 0, (1 << PLACED_ADDRESS_BITS) - 1);
        check(&views, SLOTS as u32, 1 << 33);
        check(&views, SLOTS as u32, 0);
        check(&views, 0, 7);
        assert_eq!(misses, 4);
    }
}
