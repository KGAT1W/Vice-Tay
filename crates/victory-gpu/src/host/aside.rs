//! Work for a list that a thread of ours does while the title records on:
//! the pass over the heap's buffer views (`crate::draw`), which took 12 to 17
//! ms of one recording thread at the frame's first list that needs it, with
//! the submission of that list, and of those behind it, waiting.
//! `VICTORY_HEAP_PASS_ASIDE=0`: done where it is needed, as before.
//!
//! What it finds are refreshes of host copies, which are steps of the list
//! for its submission, and what it makes are descriptors, which the card
//! reads when the list runs: both are there when the list is closed
//! (`HostList::close` waits), and the refreshes stand at the place in the
//! list's steps where the pass was begun.

use super::*;

/// A host copy, and the title memory of it (from, to).
pub(crate) type Refresh = (std::sync::Arc<crate::shadow::Shadow>, u64, u64);

/// What is being found, until it is.
pub(crate) struct Coming(Mutex<Option<Vec<Refresh>>>, std::sync::Condvar,
    #[cfg(feature = "diagnostics")] Option<Box<measure::Job>>);

#[cfg(feature = "diagnostics")]
mod measure {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use windows_sys::Win32::{Foundation::FILETIME, System::Threading::{GetCurrentThread, GetCurrentThreadId, GetThreadTimes}};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryThreadCycleTime(thread: *mut std::ffi::c_void, cycles: *mut u64) -> i32;
    }
    pub(super) use crate::timeline::ticks;
    pub(super) struct Job {
        queued: u64,
        start: AtomicU64,
        pub(super) ready: AtomicU64,
        producer: AtomicU64,
        cpu_100ns: AtomicU64,
        cycles: AtomicU64,
    }
    pub(super) fn counters() -> (u64, u64) {
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let val = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        let mut cycles = 0;
        // SAFETY: pseudo-handle for the calling thread and valid outputs.
        unsafe {
            let h = GetCurrentThread();
            let cpu = if GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 { val(k) + val(u) } else { 0 };
            QueryThreadCycleTime(h, &mut cycles);
            (cpu, cycles)
        }
    }
    impl Job {
        pub(super) fn new() -> Option<Box<Self>> {
            static ON: OnceLock<bool> = OnceLock::new();
            // Not tied to the timeline's few frames: a system trace
            // (scripts/trace-close-wait.ps1) is taken whenever the scene is
            // on screen, and its events are matched by these QPC values.
            if !*ON.get_or_init(|| crate::switch::on("VICTORY_HEAP_CLOSE_TIMING")) { return None; }
            Some(Box::new(Self { queued: ticks(), start: 0.into(), ready: 0.into(), producer: 0.into(), cpu_100ns: 0.into(), cycles: 0.into() }))
        }
        pub(super) fn start(&self) -> (u64, u64) {
            self.producer.store(u64::from(unsafe { GetCurrentThreadId() }), Ordering::Relaxed);
            self.start.store(ticks(), Ordering::Relaxed);
            counters()
        }
        pub(super) fn finish(&self, before: (u64, u64)) {
            let after = counters();
            self.cpu_100ns.store(after.0.saturating_sub(before.0), Ordering::Relaxed);
            self.cycles.store(after.1.saturating_sub(before.1), Ordering::Relaxed);
        }
        pub(super) fn report(&self, wait: u64, resumed: u64) {
            tracing::info!(target: "victory_gpu::heap_close", queued=self.queued, start=self.start.load(Ordering::Relaxed),
                ready=self.ready.load(Ordering::Relaxed), wait, resumed, producer=self.producer.load(Ordering::Relaxed),
                consumer=unsafe { GetCurrentThreadId() }, cpu_100ns=self.cpu_100ns.load(Ordering::Relaxed),
                cycles=self.cycles.load(Ordering::Relaxed), freq=crate::timeline::frequency(), pid=std::process::id(),
                "heap pass QPC");
        }
    }
}

/// Since the last frame line: microseconds lists waited at their close.
pub(crate) static WAITED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Coming {
    /// What was found.
    pub(crate) fn wait(&self) -> Vec<Refresh> {
        let began = std::time::Instant::now();
        let _waiting = crate::timeline::span("a list's close waits for the heap's views");
        #[cfg(feature = "diagnostics")]
        let wait = self.2.as_ref().map(|_| measure::ticks());
        let mut found = self.0.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(found) = found.take() {
                WAITED.fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                if let (Some(job), Some(wait)) = (&self.2, wait) { job.report(wait, measure::ticks()); }
                return found;
            }
            found = self.1.wait(found).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn found(&self, what: Vec<Refresh>) {
        {
            let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(what);
            #[cfg(feature = "diagnostics")]
            if let Some(job) = &self.2 { job.ready.store(measure::ticks(), Ordering::Relaxed); }
        }
        self.1.notify_all();
    }
}

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_PASS_ASIDE").is_none_or(|v| v != "0"))
}

type Job = Box<dyn FnOnce() + Send>;

/// `work` begins on the thread for it, one after another; here and now if
/// there is no such thread.
pub(crate) fn begin(work: impl FnOnce() -> Vec<Refresh> + Send + 'static) -> std::sync::Arc<Coming> {
    static THREAD: OnceLock<Option<Mutex<std::sync::mpsc::Sender<Job>>>> = OnceLock::new();
    let coming = std::sync::Arc::new(Coming(Mutex::new(None), std::sync::Condvar::new(),
        #[cfg(feature = "diagnostics")] measure::Job::new()));
    let told = coming.clone();
    let job: Job = Box::new(move || {
        // (However it ends, the list that waits goes on.)
        let _aside = crate::timeline::span("beside: the heap's views");
        #[cfg(feature = "diagnostics")]
        let before = told.2.as_ref().map(|job| job.start());
        let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_default();
        #[cfg(feature = "diagnostics")]
        if let (Some(job), Some(before)) = (&told.2, before) { job.finish(before); }
        told.found(found);
    });
    let thread = THREAD.get_or_init(|| {
        let (send, jobs) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("victory-aside".into())
            .spawn(move || {
                crate::cores::keep_on_fast_cores("victory-aside");
                jobs.into_iter().for_each(|job| job())
            })
            .ok()
            .map(|_| Mutex::new(send))
    });
    match thread {
        Some(send) => {
            if let Err(job) = send.lock().unwrap_or_else(|e| e.into_inner()).send(job) {
                (job.0)();
            }
        }
        None => job(),
    }
    coming
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_found_is_waited_for_and_a_panic_lets_the_list_go_on() {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let coming = begin(move || {
            let _ = wait.recv();
            Vec::new()
        });
        assert!(coming.0.lock().unwrap().is_none());
        go.send(()).unwrap();
        assert!(coming.wait().is_empty());
        assert!(begin(|| panic!("a panic of the work")).wait().is_empty());
        assert!(begin(Vec::new).wait().is_empty());
    }
}
