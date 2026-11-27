//! Record one prelude per title recording in its already allocated command
//! memory. D3D12 allows several lists in an allocator, one *open* at a time.
//! At submission the title list is closed; its normal retirement/reset owns
//! the lifetime of both recordings. Never reset the title allocator here.

use super::*;
use windows::core::Result;

pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RX_PRELUDE").is_some_and(|v| v == "1"))
}

struct Native {
    list: ID3D12GraphicsCommandList,
    submit: ID3D12CommandList,
}

impl Native {
    fn open(host: &Host, allocator: &ID3D12CommandAllocator) -> Result<Self> {
        let _create = crate::timeline::span("rx prelude: create open list");
        // SAFETY: allocator on this device, no other open list using it.
        let list: ID3D12GraphicsCommandList = unsafe {
            host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, allocator, None)?
        };
        LISTS[1].fetch_add(1, Ordering::Relaxed);
        Ok(Self { submit: list.cast()?, list })
    }
}

enum LastUse {
    /// Still being prepared or already submitted under this submission lock.
    Preparing(u64),
    /// An upper bound on the fence signalled behind the last execution.
    Fence(u64),
}

impl LastUse {
    fn retired(&mut self, submission: u64, last_sent: u64, completed: u64) -> bool {
        if let Self::Preparing(previous) = *self {
            if submission == previous {
                return false;
            }
            // A subsequent submit has acquired the same lock. last_sent
            // covers *all* groups of the previous one, including GPU timing
            // mode. The allocator stays unavailable if that signal fails.
            *self = Self::Fence(last_sent);
        }
        matches!(*self, Self::Fence(f) if completed != u64::MAX && completed >= f)
    }
}

#[derive(Default)]
pub(super) struct Recording {
    shared: Option<(u64, Native)>,
    // Uncommon: resubmitting a closed title recording. A private allocator
    // prevents growth of its title allocator until reset; distinct pending
    // occurrences cannot overwrite one another before ExecuteCommandLists.
    extra: Vec<(LastUse, Native, ID3D12CommandAllocator)>,
}

impl Recording {
    pub(super) fn record(
        &mut self, host: &Host, owner: &HostList, submission: u64,
        commands: impl FnOnce(&ID3D12GraphicsCommandList),
    ) -> Result<ID3D12CommandList> {
        let epoch = owner.epoch();
        // Recording changes the epoch, and reset always changes it. It is
        // stable from Close through all submissions of that same recording.
        let native = match self.shared.as_ref().map(|(prior, _)| *prior) {
            None => &self.shared.insert((epoch, Native::open(host, &owner.allocator)?)).1,
            Some(prior) if prior != epoch => {
                let (prior, list) = self.shared.as_mut().unwrap();
                let _reset = crate::timeline::span("rx prelude: reset list on title allocator");
                // SAFETY: the previous recording has been submitted. Reset
                // opens a new list; it does not recycle the allocator memory.
                unsafe { list.list.Reset(&owner.allocator, None)? };
                *prior = epoch;
                &*list
            }
            Some(_) => {
                // card_sent signals this fence *after* every Execute batch.
                // Looking at the most recent marker is conservative: later
                // unrelated work may delay reuse. No queue signal or wait is
                // needed here. Don't ask at all for an as-yet unused slot.
                let slot = if self.extra.is_empty() {
                    None
                } else {
                    let clock = host.card_clock().ok_or_else(windows::core::Error::empty)?;
                    let last_sent = clock.state.lock().unwrap_or_else(|e| e.into_inner()).0;
                    // SAFETY: a plain fence query. UINT64_MAX means device loss.
                    let completed = unsafe { clock.fence.GetCompletedValue() };
                    if completed == u64::MAX {
                        fail(format_args!("host device removed before recording a repeated prelude"));
                    }
                    self.extra.iter_mut().position(|(used, _, _)| used.retired(submission, last_sent, completed))
                };
                if let Some(i) = slot {
                    let (used, list, allocator) = &mut self.extra[i];
                    let _reset = crate::timeline::span("rx prelude: reset private allocator and list");
                    // SAFETY: completed fence behind its previous submission.
                    unsafe {
                        allocator.Reset()?;
                        list.list.Reset(&*allocator, None)?;
                    }
                    *used = LastUse::Preparing(submission);
                    &*list
                } else {
                    let allocator = {
                        let _create = crate::timeline::span("rx prelude: create private allocator");
                        // SAFETY: plain creation on the title device.
                        unsafe { host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)? }
                    };
                    let list = Native::open(host, &allocator)?;
                    self.extra.push((LastUse::Preparing(submission), list, allocator));
                    &self.extra.last().unwrap().1
                }
            }
        };
        let mut stage = crate::timeline::span("rx prelude: barriers and copies");
        commands(&native.list);
        stage.next("rx prelude: close");
        // SAFETY: the selected list is open and the complete plan recorded.
        unsafe { native.list.Close()? };
        Ok(native.submit.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prepared_occurrence_is_exclusive_until_its_submission_has_run() {
        let mut first = LastUse::Preparing(19);
        assert!(!first.retired(19, 100, 100)); // not sent yet, even on an idle queue
        assert!(!first.retired(20, 106, 101)); // timing groups advanced the fence
        assert!(!first.retired(21, 110, u64::MAX)); // device removal
        assert!(!first.retired(21, 110, 105));
        assert!(first.retired(21, 110, 106)); // later flights can still be in the air
    }

    #[test]
    fn prelude_and_title_share_memory_without_overwriting_closed_recordings() {
        let host = host().expect("D3D12 is needed for the prelude recording test");
        let owner = host.new_list().unwrap();
        let mut recording = Recording::default();
        let (source, mapped) = host.upload_buffer(4).unwrap();
        let dest = host.read_back_buffer(4).unwrap();
        // SAFETY: the four upload bytes are mapped and not yet submitted.
        unsafe { mapped.copy_from_nonoverlapping([11, 22, 33, 44].as_ptr(), 4) };
        let copy = |list: &ID3D12GraphicsCommandList, offset: u64| {
            // SAFETY: open list, live upload/readback ranges, legal copy states.
            unsafe { list.CopyBufferRegion(&dest, offset, &source, offset, 1) };
        };
        // SAFETY: a private queue and fence used only by this test.
        let (queue, fence): (ID3D12CommandQueue, ID3D12Fence) = unsafe {
            (host.device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default()
            }).unwrap(), host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap())
        };
        let send = |lists: &[Option<ID3D12CommandList>], value| {
            // SAFETY: all lists are closed and owned until after this wait.
            // A null event makes SetEventOnCompletion wait synchronously.
            unsafe {
                queue.ExecuteCommandLists(lists);
                queue.Signal(&fence, value).unwrap();
                fence.SetEventOnCompletion(value, HANDLE::default()).unwrap();
                assert_eq!(fence.GetCompletedValue(), value);
            }
        };
        copy(&owner.list, 1);
        owner.close();
        let a = recording.record(host, &owner, 1, |l| copy(l, 0)).unwrap();
        let b = recording.record(host, &owner, 1, |l| copy(l, 2)).unwrap();
        assert_ne!(a.as_raw(), b.as_raw());
        send(&[Some(a.clone()), Some(owner.list.cast().unwrap()), Some(b)], 1);
        owner.reset();
        owner.close();
        let c = recording.record(host, &owner, 2, |l| copy(l, 3)).unwrap();
        assert_eq!(a.as_raw(), c.as_raw()); // same list, fresh title allocation epoch
        send(&[Some(c), Some(owner.list.cast().unwrap())], 2);

        let mut data = std::ptr::null_mut();
        // SAFETY: the final fence completed; all four bytes have been copied.
        let result: [u8; 4] = unsafe {
            dest.Map(0, None, Some(&mut data)).unwrap();
            let result = data.cast::<[u8; 4]>().read();
            dest.Unmap(0, None);
            result
        };
        assert_eq!(result, [11, 22, 33, 44]);
    }
}
