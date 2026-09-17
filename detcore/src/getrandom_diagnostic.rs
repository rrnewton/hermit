//! Temporary, bounded getrandom observations for the installed SaBRe diagnostic.
//! Recording must remain allocation-, syscall-, RPC-, and PRNG-draw-free.
//! Collection/decoding happens in the coordinator after the guest's existing stop.

use std::fmt;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Maximum number of append-only observations in one image.
pub const CAPACITY: usize = 1024;
/// Fixed word width including the commit marker.
pub const RECORD_WORDS: usize = 32;
/// Fixed image header width in words.
pub const HEADER_WORDS: usize = 6;
/// Image identity marker.
pub const MAGIC: u64 = 0x3152474e524d5248;
/// Nonzero image footer and file-backed extent marker.
pub const TRAILER: u64 = 0x454e4f44474e5248;
/// Diagnostic wire-layout version.
pub const VERSION: u64 = 1;
/// Complete fixed image extent in words.
pub const IMAGE_WORDS: usize = HEADER_WORDS + CAPACITY * RECORD_WORDS + 1;
/// Complete fixed image extent in bytes.
pub const IMAGE_BYTES: usize = IMAGE_WORDS * 8;

/// Thread state after its original initialization.
pub const INIT: u64 = 1;
/// Original AT_RANDOM draw, or explicit absence of an auxv pointer.
pub const POST_EXEC: u64 = 2;
/// Shared handler entry before flags are validated.
pub const GETRANDOM_ENTRY: u64 = 3;
/// Original bounded random fill with before/after PRNG state.
pub const RANDOM_FILL: u64 = 4;
/// Shared handler result; payload word24 marks success.
pub const GETRANDOM_RESULT: u64 = 5;
/// SaBRe route entry; payload word24 identifies the route.
pub const DETOUR_ENTRY: u64 = 6;
/// SaBRe public detour return.
pub const DETOUR_RESULT: u64 = 7;
/// Plugin post-load callback boundary.
pub const POST_LOAD: u64 = 8;
/// Original one-shot startup guard decision.
pub const BOOTSTRAP_DECISION: u64 = 9;

/// Record includes seed, PRNG state and pedigree.
pub const HAS_THREAD_STATE: u64 = 1;
/// Fixed pedigree capacity was exceeded; decoding refuses.
pub const PEDIGREE_TRUNCATED: u64 = 2;
/// The copied bytes are only a bounded prefix of the observed fill.
pub const BYTES_ARE_PREFIX: u64 = 4;
/// Existing recursive detour branch called the captured original.
pub const REENTRANT_ORIGINAL: u64 = 8;

/// All words are scalar observations, never pointers to guest memory.
/// Payload: kind, validity, DetTid, flags, length, seed, state low/high,
/// result, observed-byte length, copied-byte length, pedigree length,
/// four byte words, eight pedigree words, then seven event-specific words.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record(pub [u64; RECORD_WORDS - 1]);

impl Record {
    /// Construct a zeroed scalar observation for one event.
    pub fn new(kind: u64) -> Self {
        let mut words = [0; RECORD_WORDS - 1];
        words[0] = kind;
        Self(words)
    }

    /// Record supplied state and format the borrowed pedigree into fixed storage.
    pub fn thread(mut self, tid: i32, seed: u64, state: u128, pedigree: impl fmt::Display) -> Self {
        self.0[1] |= HAS_THREAD_STATE;
        self.0[2] = tid as i64 as u64;
        self.0[5] = seed;
        self.0[6] = state as u64;
        self.0[7] = (state >> 64) as u64;
        let mut text = FixedText::new();
        if fmt::write(&mut text, format_args!("{pedigree}")).is_err() {
            self.0[1] |= PEDIGREE_TRUNCATED;
        }
        self.0[11] = text.used as u64;
        for (word, chunk) in self.0[16..24].iter_mut().zip(text.bytes.as_chunks::<8>().0) {
            let mut bytes = [0; 8];
            bytes.copy_from_slice(chunk);
            *word = u64::from_le_bytes(bytes);
        }
        self
    }

    /// Attach the unchanged request flags and length.
    pub fn request(mut self, flags: u64, length: usize) -> Self {
        self.0[3] = flags;
        self.0[4] = length as u64;
        self
    }

    /// Attach the observed signed result without changing it.
    pub fn result(mut self, result: i64) -> Self {
        self.0[8] = result as u64;
        self
    }

    /// Copy only safe, already-existing tool-local bytes. Never read the
    /// original syscall pointer to produce a diagnostic observation.
    pub fn bytes(mut self, observed: &[u8]) -> Self {
        self.0[9] = observed.len() as u64;
        let copied = observed.len().min(32);
        self.0[10] = copied as u64;
        if copied != observed.len() {
            self.0[1] |= BYTES_ARE_PREFIX;
        }
        let mut bytes = [0; 32];
        bytes[..copied].copy_from_slice(&observed[..copied]);
        for (word, chunk) in self.0[12..16].iter_mut().zip(bytes.as_chunks::<8>().0) {
            let mut value = [0; 8];
            value.copy_from_slice(chunk);
            *word = u64::from_le_bytes(value);
        }
        self
    }
}

struct FixedText {
    bytes: [u8; 64],
    used: usize,
}

impl FixedText {
    const fn new() -> Self {
        Self {
            bytes: [0; 64],
            used: 0,
        }
    }
}

impl fmt::Write for FixedText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let copied = text.len().min(self.bytes.len() - self.used);
        self.bytes[self.used..self.used + copied].copy_from_slice(&text.as_bytes()[..copied]);
        self.used += copied;
        if copied == text.len() {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}

#[repr(C)]
struct Slot {
    committed: AtomicU64,
    payload: [AtomicU64; RECORD_WORDS - 1],
}

impl Slot {
    const fn new() -> Self {
        Self {
            committed: AtomicU64::new(0),
            payload: [const { AtomicU64::new(0) }; RECORD_WORDS - 1],
        }
    }
}

/// Fixed append-only atomic image, shared only within its owning address space.
#[repr(C)]
pub struct Buffer {
    magic: u64,
    version: u64,
    capacity: u64,
    record_words: u64,
    next: AtomicU64,
    dropped: AtomicU64,
    slots: [Slot; CAPACITY],
    // Nonzero static data at both ends also avoids an anonymous BSS tail.
    trailer: u64,
}

const _: () = assert!(std::mem::size_of::<Slot>() == RECORD_WORDS * 8);
const _: () = assert!(std::mem::size_of::<Buffer>() == IMAGE_BYTES);
const _: () = assert!(std::mem::align_of::<Buffer>() == 8);

impl Buffer {
    /// Construct a statically initialized empty image.
    pub const fn new() -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            capacity: CAPACITY as u64,
            record_words: RECORD_WORDS as u64,
            next: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            slots: [const { Slot::new() }; CAPACITY],
            trailer: TRAILER,
        }
    }

    /// Append only: a record's payload is never overwritten after publication.
    pub fn record(&self, record: Record) -> Option<u64> {
        let index = match self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                (next < CAPACITY as u64).then_some(next + 1)
            }) {
            Ok(index) => index,
            Err(_) => {
                let _ = self
                    .dropped
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |dropped| {
                        Some(dropped.saturating_add(1))
                    });
                return None;
            }
        };
        let slot = &self.slots[index as usize];
        for (destination, value) in slot.payload.iter().zip(record.0) {
            destination.store(value, Ordering::Relaxed);
        }
        slot.committed.store(index + 1, Ordering::Release);
        Some(index)
    }

    /// Coordinator-only, after writers finish. No allocation is used by record().
    /// Keep the complete fixed image even when decode() will reject its contents.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut image = Vec::with_capacity(IMAGE_BYTES);
        for word in [
            self.magic,
            self.version,
            self.capacity,
            self.record_words,
            self.next.load(Ordering::Acquire),
            self.dropped.load(Ordering::Acquire),
        ] {
            image.extend_from_slice(&word.to_le_bytes());
        }
        for slot in &self.slots {
            image.extend_from_slice(&slot.committed.load(Ordering::Acquire).to_le_bytes());
            for word in &slot.payload {
                image.extend_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
            }
        }
        image.extend_from_slice(&self.trailer.to_le_bytes());
        image
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

/// This loaded Detcore instance's diagnostic image.
pub static BUFFER: Buffer = Buffer::new();

/// Coordinator-side read protocol. Call only after establishing the separate
/// address-space lifetime condition. A successful copy does not prove that
/// another writer cannot append after this function returns.
///
/// Confirm every claimed publication BEFORE copying payload. Observing a
/// committed value only after a payload read must never validate that payload.
/// The callback must perform a complete read or return an error, not zero-fill
/// an unavailable or short read. At most CAPACITY + 3 reads are requested.
pub fn capture(
    mut read_exact: impl FnMut(usize, &mut [u8]) -> Result<(), String>,
) -> Result<Vec<u8>, String> {
    let mut first_header = [0_u8; HEADER_WORDS * 8];
    read_exact(0, &mut first_header)?;
    let header_word = |index: usize| {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&first_header[index * 8..(index + 1) * 8]);
        u64::from_le_bytes(bytes)
    };
    if [
        header_word(0),
        header_word(1),
        header_word(2),
        header_word(3),
    ] != [MAGIC, VERSION, CAPACITY as u64, RECORD_WORDS as u64]
    {
        return Err("diagnostic header identity/layout mismatch".into());
    }
    let count = header_word(4);
    if count > CAPACITY as u64 || header_word(5) != 0 {
        return Err("diagnostic buffer overflow".into());
    }
    for index in 0..count as usize {
        let mut published = [0; 8];
        read_exact((HEADER_WORDS + index * RECORD_WORDS) * 8, &mut published)?;
        if u64::from_le_bytes(published) != index as u64 + 1 {
            return Err("record not committed before payload capture".into());
        }
    }
    let mut image = vec![0; IMAGE_BYTES];
    read_exact(0, &mut image)?;
    let mut final_header = [0; HEADER_WORDS * 8];
    read_exact(0, &mut final_header)?;
    if final_header != first_header || image[..HEADER_WORDS * 8] != first_header {
        return Err("diagnostic header changed during capture".into());
    }
    // decode also rechecks each publication value against the exact sequence
    // confirmed above, and rejects publication outside the observed count.
    decode(&image).map_err(str::to_owned)?;
    Ok(image)
}

/// The real decoder for both local and stopped-tracee captures. Loss is never
/// converted to a valid empty sequence. The raw image is retained separately.
pub fn decode(image: &[u8]) -> Result<Vec<Record>, &'static str> {
    if image.len() != IMAGE_BYTES {
        return Err("short or oversized diagnostic image");
    }
    let word = |i: usize| {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&image[i * 8..(i + 1) * 8]);
        u64::from_le_bytes(bytes)
    };
    if [word(0), word(1), word(2), word(3)]
        != [MAGIC, VERSION, CAPACITY as u64, RECORD_WORDS as u64]
        || word(IMAGE_WORDS - 1) != TRAILER
    {
        return Err("diagnostic image identity/layout mismatch");
    }
    let count = word(4);
    if count > CAPACITY as u64 || word(5) != 0 {
        return Err("diagnostic buffer overflow");
    }
    let mut records = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let start = HEADER_WORDS + index * RECORD_WORDS;
        if word(start) != index as u64 + 1 {
            return Err("incomplete diagnostic record");
        }
        let mut payload = [0; RECORD_WORDS - 1];
        for (offset, value) in payload.iter_mut().enumerate() {
            *value = word(start + offset + 1);
        }
        if !(INIT..=BOOTSTRAP_DECISION).contains(&payload[0])
            || payload[1]
                & !(HAS_THREAD_STATE | PEDIGREE_TRUNCATED | BYTES_ARE_PREFIX | REENTRANT_ORIGINAL)
                != 0
        {
            return Err("unknown diagnostic record kind/validity");
        }
        if payload[11] > 64 || payload[10] > 32 || payload[10] > payload[9] {
            return Err("invalid diagnostic payload bounds");
        }
        if payload[1] & PEDIGREE_TRUNCATED != 0 {
            return Err("truncated diagnostic pedigree");
        }
        records.push(Record(payload));
    }
    for index in count as usize..CAPACITY {
        if word(HEADER_WORDS + index * RECORD_WORDS) != 0 {
            return Err("diagnostic count/publication mismatch");
        }
    }
    Ok(records)
}
