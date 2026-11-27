//! Startup stock for the smallest title shadow. Only the producer can put
//! anything in it; taking a buffer permanently transfers its resource and map.
use super::{Host, ID3D12Resource};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

pub(crate) const TITLE_UNIT: u64 = 64 << 10;

fn in_batches() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_RX_TITLE_STOCK"))
}

fn cap() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("VICTORY_TITLE_PREWARM_COUNT").ok()
            .and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(0).min(if in_batches() { 3584 } else { 512 })
    })
}

struct Fresh(ID3D12Resource, *mut u8);
// SAFETY: the resource owns the persistent map; no thread touches its
// bytes until the sole owner removes the pair from stock.
unsafe impl Send for Fresh {}

#[derive(Default)]
pub(super) struct Stock {
    started: AtomicBool,
    stopped: AtomicBool,
    buffers: OnceLock<Mutex<Vec<Fresh>>>,
    ready: AtomicUsize,
    hits: AtomicU64,
    discarded: AtomicU64,
}

fn room(memory: Option<[u64; 4]>) -> bool {
    // L0 is non-local on discrete cards. Fail closed if DXGI cannot give
    // us a budget, including on adapters with no non-local segment.
    memory.is_some_and(|[_, _, budget, used]| budget.saturating_sub(used) >= 128 << 20 && used < budget / 10 * 9)
}

impl Host {
    // All allocations precede the mapping fence. No descriptor or title list
    // can see any buffer in the batch until MakeResident, the GPU fence, and
    // every CPU map have succeeded. Each resource still has its own address.
    fn fresh_batch(&self, count: usize) -> windows::core::Result<Vec<Fresh>> {
        use windows::{core::Interface, Win32::Graphics::Direct3D12::ID3D12Pageable};
        let buffers = (0..count).map(|_| self.create_shared_buffer(TITLE_UNIT, Kind::Prewarm64))
            .collect::<windows::core::Result<Vec<_>>>()?;
        let objects = buffers.iter().map(|b| b.cast::<ID3D12Pageable>().map(Some))
            .collect::<windows::core::Result<Vec<_>>>()?;
        let began = Instant::now();
        // SAFETY: one live pageable for each freshly committed resource, all
        // on this device. No previously issued buffer is present.
        unsafe { self.device.MakeResident(&objects)?; }
        Kind::Prewarm64.resident_us(began.elapsed().as_micros() as u64);
        super::mapped(&self.device, Kind::Prewarm64)?;
        buffers.into_iter().map(|b| {
            let mut data = std::ptr::null_mut();
            // SAFETY: CPU-visible memory, and the returned owner keeps the map.
            unsafe { b.Map(0, None, Some(&mut data))?; }
            #[cfg(feature = "diagnostics")]
            crate::book::made(&b, || "a shared buffer".into());
            Ok(Fresh(b, data.cast()))
        }).collect()
    }

    pub(super) fn start_title_stock(&self) {
        let count = cap();
        if count == 0 || self.stock.started.load(Relaxed) { return; }
        // Only the process-lifetime presenting host may lend itself to the
        // worker; a separately created/test device never shares this stock.
        let Some(owner) = super::HOST.get().and_then(Option::as_ref).filter(|h| std::ptr::eq(*h, self)) else { return };
        if self.stock.started.swap(true, Relaxed) { return; }
        if !super::wait_resident() {
            tracing::info!("title prewarm skipped: mapping fence disabled");
            return;
        }
        let start = Instant::now();
        if let Err(e) = std::thread::Builder::new().name("title-prewarm".into()).spawn(move || {
            let stock = &owner.stock;
            let mut slots = Vec::new();
            if slots.try_reserve_exact(count).is_err() {
                tracing::warn!(count, "could not reserve title prewarm stock");
                return;
            }
            let buffers = stock.buffers.get_or_init(|| Mutex::new(slots));
            let mut made = 0usize;
            let reason = loop {
                if made == count { break "cap"; }
                if start.elapsed() >= Duration::from_secs(if in_batches() { 8 } else { 2 }) { break "deadline"; }
                if stock.stopped.load(Relaxed) { break "pressure"; }
                if made.is_multiple_of(8) && (!room(owner.video_memory()) || crate::residency::pressured()
                    || crate::residency::critical_commit() || super::available_physical_ram().is_none_or(|n| n < 3 << 30))
                {
                    stock.stop();
                    break "pressure";
                }
                let (one, batch) = if in_batches() {
                    match owner.fresh_batch((count - made).min(32)) {
                        Ok(fresh) => (None, fresh),
                        Err(error) => {
                            tracing::warn!(%error, "title prewarm batch failed");
                            stock.stop();
                            break "allocation failed";
                        }
                    }
                } else {
                    let Some((buffer, map)) = owner.make_shared_buffer(TITLE_UNIT, Kind::Prewarm64) else {
                        stock.stop();
                        break "allocation failed";
                    };
                    (Some(Fresh(buffer, map)), Vec::new())
                };
                // All driver calls (including the mapping fence), and the
                // Vec capacity reservation, happened before taking this lock.
                let mut slots = buffers.lock().unwrap_or_else(|e| e.into_inner());
                if stock.stopped.load(Relaxed) { break "pressure"; }
                made += usize::from(one.is_some()) + batch.len();
                slots.extend(one);
                slots.extend(batch);
                stock.ready.store(slots.len(), Relaxed);
            };
            tracing::info!(count, made, reason, elapsed_ms = start.elapsed().as_millis() as u64, "title prewarm finished");
        }) {
            tracing::warn!(%e, "could not start title prewarm");
        }
    }

    pub(crate) fn title_shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        if size != TITLE_UNIT { return self.shared_buffer(size); }
        if let Some(Fresh(buffer, map)) = self.stock.take() {
            return Some((buffer, map));
        }
        self.make_shared_buffer(size, Kind::Title64)
    }

    pub(super) fn trim_title_stock(&self, memory: Option<[u64; 4]>) {
        if self.stock.buffers.get().is_some() && !self.stock.stopped.load(Relaxed)
            && (crate::residency::pressured() || !room(memory))
        {
            self.stock.stop();
        }
    }
}

impl Stock {
    fn take(&self) -> Option<Fresh> {
        if self.ready.load(Relaxed) == 0 { return None; }
        let mut slots = self.buffers.get()?.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = slots.pop()?;
        self.ready.store(slots.len(), Relaxed);
        drop(slots);
        let issued = self.hits.fetch_add(1, Relaxed) + 1;
        if in_batches() && (issued.is_multiple_of(1024) || issued as usize == cap()) {
            tracing::info!(issued, "title stock buffers used");
        }
        Some(fresh)
    }

    fn stop(&self) {
        self.stopped.store(true, Relaxed);
        if let Some(slots) = self.buffers.get() {
            let drop_outside_lock = {
                let mut slots = slots.lock().unwrap_or_else(|e| e.into_inner());
                self.ready.store(0, Relaxed);
                std::mem::take(&mut *slots)
            };
            self.discarded.fetch_add(drop_outside_lock.len() as u64, Relaxed);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind {
    Title64, Prewarm64, Shared, Upload, Readback, MappedCard, Uav,
    TextureHeap, PlacedTexture, Texture, RenderTarget, DepthTarget, UavTexture, Reserved, DepthTwin,
}
use Kind::*;
const KINDS: [Kind; 15] = [Title64, Prewarm64, Shared, Upload, Readback, MappedCard, Uav,
    TextureHeap, PlacedTexture, Texture, RenderTarget, DepthTarget, UavTexture, Reserved, DepthTwin];

fn diagnostics() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ALLOC_TIMINGS").is_some_and(|v| v == "1"))
}

// Per kind and operation (Create, MakeResident, mapping fence): cumulative
// calls, total us, longest us. Sampling never resets a producer's counters.
static COST: [[[AtomicU64; 3]; 3]; KINDS.len()] =
    [const { [const { [const { AtomicU64::new(0) }; 3] }; 3] }; KINDS.len()];

impl Kind {
    fn add(self, op: usize, us: u64) {
        let [count, total, longest] = &COST[self as usize][op];
        count.fetch_add(1, Relaxed);
        total.fetch_add(us, Relaxed);
        longest.fetch_max(us, Relaxed);
    }

    pub(crate) fn create<T>(self, call: impl FnOnce() -> T) -> T {
        if !diagnostics() { return call(); }
        let start = Instant::now();
        let result = call();
        self.add(0, start.elapsed().as_micros() as u64);
        result
    }

    pub(super) fn resident_us(self, us: u64) { if diagnostics() { self.add(1, us); } }
    pub(super) fn mapped_us(self, us: u64) { if diagnostics() { self.add(2, us); } }
}

pub(super) fn sample() {
    if !diagnostics() { return; }
    use std::fmt::Write;
    let mut costs = String::new();
    for kind in KINDS {
        let ops = COST[kind as usize].each_ref().map(|row| row.each_ref().map(|a| a.load(Relaxed)));
        if ops[0][0] != 0 { let _ = write!(costs, " {kind:?}={ops:?}"); }
    }
    let stock = super::HOST.get().and_then(Option::as_ref).map(|h| &h.stock);
    let ready = stock.map_or(0, |s| s.ready.load(Relaxed));
    tracing::info!(frame = crate::frame::current(), ms = super::now_ms(), ready,
        hits = stock.map_or(0, |s| s.hits.load(Relaxed)), discarded = stock.map_or(0, |s| s.discarded.load(Relaxed)),
        texture_pool = ?crate::pool::allocation_sample(), %costs,
        "allocation sample: texture_pool=[grown,dropped,quarantined_bytes,largest_64k_aligned_bytes]; costs=create,resident,mapping [calls,total_us,max_us]");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speculative_stock_requires_shared_budget_and_slack() {
        assert!(!room(None));
        assert!(!room(Some([0, 0, 0, 0])));
        assert!(!room(Some([0, 0, 127 << 20, 0])));
        assert!(!room(Some([0, 0, 8 << 30, 15 << 29])));
        assert!(room(Some([0, 0, 8 << 30, 4 << 30])));
    }

    #[test]
    fn trimming_stock_does_not_touch_an_issued_buffer() {
        let Some(host) = crate::host::host() else { return };
        let fresh = || {
            let (buffer, map) = host.make_shared_buffer(TITLE_UNIT, Kind::Prewarm64).unwrap();
            Fresh(buffer, map)
        };
        let stock = Stock::default();
        let (one, two) = (fresh(), fresh());
        // SAFETY: two live committed buffers, queried without submitting work.
        assert_ne!(unsafe { one.0.GetGPUVirtualAddress() }, unsafe { two.0.GetGPUVirtualAddress() });
        assert!(stock.buffers.set(Mutex::new(vec![one, two])).is_ok());
        stock.ready.store(2, Relaxed);
        let Fresh(issued, map) = stock.take().unwrap();
        // SAFETY: this resource is mapped and has never been sent to a list.
        unsafe { map.write_volatile(0xa5) };
        stock.stop();
        assert!(stock.take().is_none());
        assert_eq!(stock.discarded.load(Relaxed), 1);
        // SAFETY: `issued` owns this mapping even after stock was trimmed.
        unsafe {
            assert_eq!(issued.GetDesc().Width, TITLE_UNIT);
            assert_eq!(map.read_volatile(), 0xa5);
        }
    }
    #[test]
    fn batch_maps_have_distinct_zeroed_storage_and_independent_lifetimes() {
        let Some(host) = crate::host::host() else { return };
        if !super::super::wait_resident() { return; }
        let mut batch = host.fresh_batch(4).unwrap();
        let mut addresses = std::collections::HashSet::new();
        for (i, Fresh(buffer, map)) in batch.iter().enumerate() {
            // SAFETY: new persistent maps, no queues access them.
            unsafe {
                assert!(addresses.insert(buffer.GetGPUVirtualAddress()));
                assert_eq!(map.read_volatile(), 0);
                map.write_volatile(i as u8 + 1);
            }
        }
        let last = batch.pop().unwrap();
        drop(batch);
        assert_eq!(unsafe { last.1.read_volatile() }, 4);
    }
}
