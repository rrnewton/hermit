//! One native brk cursor and one contiguous, proven private-anonymous interval.
//! Ordinary `MemoryMetadata::observe_brk` and serialized ranges are not inputs.
use super::*;

#[derive(Debug)]
struct Heap {
    root: Arc<ForegroundRoot>,
    cursor: u64,
    range: Option<(u64, u64)>,
}

#[derive(Debug)]
struct PendingHeap {
    root: Arc<ForegroundRoot>,
    nr: Sysno,
    arguments: [usize; 6],
    entered: bool,
    prior: Option<Heap>,
}

#[derive(Debug, Default)]
pub(super) struct State {
    current: Option<Heap>,
    pending: Option<PendingHeap>,
}

fn page_end(address: u64) -> Option<u64> {
    address
        .checked_add(PAGE_SIZE as u64 - 1)
        .map(|end| end & !(PAGE_SIZE as u64 - 1))
}

fn disjoint(start: u64, length: u64, range: (u64, u64)) -> bool {
    if length == 0 {
        return true;
    }
    let start_page = start & !(PAGE_SIZE as u64 - 1);
    start
        .checked_add(length)
        .and_then(page_end)
        .is_some_and(|end| end <= range.0 || start_page >= range.1)
}

impl State {
    pub(super) fn span(
        &self,
        owner: NetworkStreamOwner,
        generation: u64,
        address: u64,
        length: u64,
    ) -> Option<OriginalArena> {
        let heap = self.current.as_ref()?;
        let (start, end) = heap.range?;
        if self.pending.is_some()
            || !heap.root.is_sole_initial_root(owner)
            || length == 0
            || address < start
            || !address
                .checked_add(length)
                .is_some_and(|limit| limit <= end)
        {
            return None;
        }
        Some(OriginalArena {
            root: heap.root.clone(),
            generation,
            start,
            end,
        })
    }

    pub(super) fn prepare(
        &mut self,
        root: &Arc<ForegroundRoot>,
        nr: Sysno,
        raw: [usize; 6],
        terminal_query: bool,
    ) {
        // No permission is available during any in-flight VM operation. A
        // replacement preparation cannot borrow the interrupted operation's
        // retained geometry, even if its numeric break happens to agree.
        let mut prior = self.current.take().filter(|heap| {
            Arc::ptr_eq(&heap.root, root) && root.is_sole_initial_root(root.owner())
        });
        self.pending = None;
        if !root.is_sole_initial_root(root.owner()) {
            return;
        }
        let preserve = match nr {
            Sysno::brk => true,
            Sysno::mmap => {
                // A nonfixed mmap cannot replace the heap. Still check its
                // actual returned interval before restoring any permission.
                let fixed = raw[3] & (libc::MAP_FIXED | libc::MAP_FIXED_NOREPLACE) as usize != 0;
                if fixed
                    && let Some(heap) = &mut prior
                    && heap
                        .range
                        .is_some_and(|range| !disjoint(raw[0] as u64, raw[1] as u64, range))
                {
                    heap.range = None;
                }
                true
            }
            Sysno::munmap | Sysno::mprotect | Sysno::pkey_mprotect | Sysno::madvise => {
                if let Some(heap) = &mut prior
                    && heap
                        .range
                        .is_some_and(|range| !disjoint(raw[0] as u64, raw[1] as u64, range))
                {
                    heap.range = None;
                }
                true
            }
            Sysno::ioctl => terminal_query,
            // In particular, no old range survives mremap, an unknown ioctl,
            // a new MM, shared mapping operations or external-writer admission.
            _ => false,
        };
        if preserve && (prior.is_some() || nr == Sysno::brk) {
            self.pending = Some(PendingHeap {
                root: root.clone(),
                nr,
                arguments: raw,
                entered: false,
                prior,
            });
        }
    }

    pub(super) fn complete(
        &mut self,
        root: &Arc<ForegroundRoot>,
        nr: Sysno,
        raw: [usize; 6],
        event: Event,
    ) -> Result<(), &'static str> {
        let Some(pending) = self.pending.as_mut() else {
            return Ok(());
        };
        if pending.nr != nr
            || pending.arguments != raw
            || !Arc::ptr_eq(&pending.root, root)
            || !root.is_sole_initial_root(root.owner())
        {
            return Err("native heap observation changed exact root/MM/syscall/arguments");
        }
        match event {
            Event::Entered if !pending.entered => pending.entered = true,
            Event::InterruptedBeforeEntry if !pending.entered => {
                self.pending = None;
            }
            Event::Returned(result) => {
                let pending = self.pending.take().unwrap();
                self.current = if nr == Sysno::brk {
                    Self::returned_brk(pending, result)?
                } else {
                    if nr == Sysno::mmap
                        && result >= 0
                        && pending
                            .prior
                            .as_ref()
                            .and_then(|heap| heap.range)
                            .is_some_and(|range| {
                                raw[1] == 0 || !disjoint(result as u64, raw[1] as u64, range)
                            })
                    {
                        return Err("native mmap result overlaps retained heap provenance");
                    }
                    pending.prior
                };
            }
            _ => return Err("native heap changed preparation/entry/return progression"),
        }
        Ok(())
    }

    fn returned_brk(pending: PendingHeap, result: i64) -> Result<Option<Heap>, &'static str> {
        if result <= 0 {
            return Ok(None);
        }
        let actual = result as u64;
        let requested = pending.arguments[0] as u64;
        let Some(mut heap) = pending.prior else {
            // A real query/first result establishes a cursor, not permission
            // for preexisting pages, BSS, or the cursor's partial first page.
            return Ok(Some(Heap {
                root: pending.root,
                cursor: actual,
                range: None,
            }));
        };
        let old = heap.cursor;
        if actual != old && (requested == 0 || actual != requested) {
            return Err("native brk result changed without the matching requested transition");
        }
        if actual == old {
            // Linux returns the old break on ordinary failure, not -ENOMEM.
            // A failed shrink is not used to prove a wholly unchanged VMA.
            if requested != 0 && requested < old {
                heap.range = None;
            }
            return Ok(Some(heap));
        }
        let old_page = page_end(old).ok_or("native old break page overflows")?;
        let new_page = page_end(actual).ok_or("native new break page overflows")?;
        if new_page > old_page {
            // Linux do_brk_flags creates private anonymous writable pages only
            // in this new suffix. Never bridge a lost/unproven interior hole.
            let start = heap
                .range
                .filter(|(_, end)| *end == old_page)
                .map_or(old_page, |(start, _)| start);
            heap.range = Some((start, new_page));
        } else if let Some((start, end)) = heap.range {
            let end = end.min(new_page);
            heap.range = (start < end).then_some((start, end));
        }
        heap.cursor = actual;
        Ok(Some(heap))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod native;
