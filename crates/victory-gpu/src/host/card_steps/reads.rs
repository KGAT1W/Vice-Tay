//! Read commands for the card-steps list. Keep the original scratch placement:
//! even the rounded-up ends of CBVs and their raw twins can be observed.

use super::{Op, Own, To};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::core::Interface;

pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RX_CBV_READS").is_some_and(|v| v == "1"))
}

// Every nonempty (shadow, address, length, level) already has its own
// aligned allocation, never reused until the submission has landed.
// Its first 256-byte block is therefore an injective key. At most 2 KiB
// for the 4 MiB scratch buffer; no four-field hash per emitted/duplicate op.
struct Seen(Vec<u64>);

impl Seen {
    fn new(end: u64) -> Self {
        Self(vec![0; end.div_ceil(64 * 256) as usize])
    }

    fn first(&mut self, at: u64) -> bool {
        debug_assert_eq!(at % 256, 0);
        let i = (at / 256) as usize;
        let word = &mut self.0[i / 64];
        let bit = 1u64 << (i % 64);
        let first = *word & bit == 0;
        *word |= bit;
        first
    }
}

// True if adjacent source data could not be joined because of its scratch
// placement. Only join adjacent bytes at BOTH ends in this level. No extra
// bytes fetched or written, even in the rounded tail of a CBV.
fn append<R>(list: &mut Vec<(R, u64, u64, u64)>, read: (R, u64, u64, u64), same: impl Fn(&R, &R) -> bool) -> bool {
    let mut apart = false;
    if let Some((resource, src, at, bytes)) = list.last_mut()
        && *src + *bytes == read.1
        && same(resource, &read.0)
    {
        if *at + *bytes == read.2 {
            *bytes += read.3;
            return false;
        }
        apart = true;
    }
    list.push(read);
    apart
}

// Logical reads, distinct places, recorded reads, read bytes, adjacent
// sources separated in scratch, recorded second-leg writes. Once per
// submission; the FPS logger drains these on the opt-in path.
static COUNTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

pub(super) fn record(own: &Own, ops: &[Op], end: u64) {
    let mut seen = Seen::new(end);
    let mut counts = [0u64; 6];
    let levels = ops.iter().map(|op| op.level).max().unwrap_or(0);
    for level in 0..=levels {
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        for op in ops.iter().filter(|op| op.level == level) {
            let len = op.len as u64;
            let shadow = &op.from.0;
            if seen.first(op.at) {
                counts[1] += 1;
                counts[3] += len;
                let read = (&shadow.buffer, shadow.offset(op.from.1 as u64), op.at, len);
                counts[4] += u64::from(append(&mut reads, read, |a, b| a.as_raw() == b.as_raw()));
            }
            match &op.to {
                To::Copy(to, dst) => writes.push((&to.buffer, to.offset(*dst as u64), op.at, len)),
                To::Buffer(buffer) => writes.push((buffer, 0, op.at, len)),
                To::Slot(..) => {}
            }
        }
        counts[2] += reads.len() as u64;
        counts[5] += writes.len() as u64;
        super::record(level == 0, &own.list.list, &own.between, &reads, &writes);
    }
    counts[0] = ops.len() as u64;
    for (counter, n) in COUNTS.iter().zip(counts) {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

pub(super) fn log(frames: f64) {
    if !enabled() {
        return;
    }
    let [ops, unique, reads, bytes, apart, writes] = COUNTS.each_ref().map(|c| c.swap(0, Ordering::Relaxed) as f64 / frames);
    tracing::info!(
        "card-steps reads per frame (VICTORY_RX_CBV_READS=1): {ops:.1} ops, {unique:.1} unique places, \
         {reads:.1} read commands ({:.1} KiB), {apart:.1} consecutive source neighbors separated in scratch, \
         {writes:.1} second-leg write commands",
        bytes / 1024.0
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_and_exact_commands_preserve_every_scratch_byte_across_levels() {
        // (level, resource, source, scratch, len). Includes the same source
        // after a write, exact duplicates, different resources, a source gap,
        // padding after a 480-byte read, and scratch slots in another bitset word.
        let ops = [
            (0, 0, 0, 0, 256),
            (1, 0, 0, 16384, 256),
            (0, 0, 256, 256, 256),
            (0, 0, 0, 0, 256),
            (0, 0, 512, 512, 480),
            (0, 0, 992, 1024, 256),
            (0, 0, 1249, 1280, 256),
            (0, 1, 1505, 1536, 256),
            (0, 1, 1506, 1792, 256),
            (1, 0, 0, 16384, 256),
        ];
        let end = 16640;
        let sources = |level: usize, r: usize, src: u64| ((src + (level * 17 + r * 31) as u64) % 251) as u8;
        let mut seen = Seen::new(end);
        let mut actual = vec![0xfd; end as usize];
        let mut expected = actual.clone();
        for level in [0, 1] {
            let mut reads = Vec::new();
            let mut apart = 0;
            for &(_, r, src, at, len) in ops.iter().filter(|op| op.0 == level) {
                for i in 0..len {
                    expected[(at + i) as usize] = sources(level, r, src + i);
                }
                if seen.first(at) {
                    apart += usize::from(append(&mut reads, (r, src, at, len), |a, b| a == b));
                }
            }
            assert_eq!((reads.len(), apart), if level == 0 { (5, 1) } else { (1, 0) });
            for (r, src, at, len) in reads {
                for i in 0..len {
                    actual[(at + i) as usize] = sources(level, r, src + i);
                }
            }
            assert_eq!(actual, expected);
        }
    }
}
