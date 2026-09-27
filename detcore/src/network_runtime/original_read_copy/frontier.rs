//! Version5 current-attempt geometry and byte coordinates. These observations
//! do not issue immutable unseen bytes, a replay layout, or a release cut.
use super::*;

pub(super) const BEGIN: u32 = 4;
pub(super) const FINISH: u32 = 5;
pub(super) const VERSION: u64 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Begin {
    pub file: u64,
    pub before: u64,
    pub start: u64,
    pub order: u64,
    pub offset: u64,
    pub requested: u64,
    pub available: u64,
    pub source_offset: u64,
    pub skb_length: u64,
    pub nonlinear: u64,
    pub position: u64,
    pub transport: u64,
    pub disposition: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttemptObservation {
    /// Index of the exact Begin in this Call's canonical raw receipt store.
    pub begin_record: usize,
    pub begin: Begin,
    pub after: u64,
}
impl Begin {
    pub(super) fn from_record(record: &Record) -> io::Result<Self> {
        if record.kind != BEGIN || record.length != 104 || record.bytes.len() != RECORD_BYTES {
            return Err(invalid("copy5 Begin wire shape"));
        }
        let mut f = [0u64; 13];
        for (field, bytes) in f.iter_mut().zip(record.bytes[..104].chunks_exact(8)) {
            *field = u64::from_le_bytes(bytes.try_into().unwrap());
        }
        Ok(Self {
            file: f[0],
            before: f[1],
            start: f[2],
            order: f[3],
            offset: f[4],
            requested: f[5],
            available: f[6],
            source_offset: f[7],
            skb_length: f[8],
            nonlinear: f[9],
            position: f[10],
            transport: f[11],
            disposition: f[12],
        })
    }
    pub(super) fn validate(
        self,
        file: u64,
        maximum: u64,
        cursor: u64,
        authority: CopyAuthority,
    ) -> io::Result<()> {
        if self.file == 0
            || self.file != file
            || self.requested == 0
            || self.offset != cursor
            || cursor > maximum
            || self.requested > maximum - cursor
            || self.available == 0
            || self.requested > self.available
            || self.source_offset > self.skb_length
            || self.available != self.skb_length - self.source_offset
            || self.skb_length > u64::from(u32::MAX)
            || self.nonlinear > self.skb_length
            || self.position > u64::from(u32::MAX)
            || !matches!(self.transport, 1 | 2)
            || self.transport == 2 && (self.nonlinear != 0 || self.position != self.source_offset)
            || self.nonlinear != 0 && self.source_offset < self.skb_length - self.nonlinear
            || self.disposition != authority.disposition
        {
            return Err(invalid(
                "copy5 Begin changed actual file, request or full available extent",
            ));
        }
        let traversal = if self.disposition == OBSERVE {
            cursor
        } else {
            0
        };
        if self.before.checked_add(traversal) != Some(self.start)
            || self.start.checked_add(self.available).is_none()
        {
            return Err(invalid(
                "copy5 Begin byte coordinates overflow or include the Consume local cursor",
            ));
        }
        Ok(())
    }
    pub(super) fn validate_end(self, end: Finish, copied: u64) -> io::Result<()> {
        let unit = end.unit;
        if unit.file != self.file
            || unit.offset != self.offset
            || unit.requested != self.requested
            || unit.position != self.position
            || unit.transport != self.transport
            || unit.disposition != self.disposition
            || unit.copied != copied
            || copied > self.requested
            || end.before != self.before
            || !matches!(unit.returned, 0 | -14)
            || unit.returned == 0 && copied != self.requested
            || unit.returned != 0 && copied == self.requested
        {
            return Err(invalid(
                "copy5 End changed its actual Begin or copy outcome",
            ));
        }
        let consume = unit.returned == 0 && unit.disposition == CONSUME;
        if self.order.checked_add(u64::from(consume)) != Some(unit.order)
            || self.before.checked_add(if consume { copied } else { 0 }) != Some(end.after)
        {
            return Err(invalid(
                "copy5 End changed the checked per-file byte/order frontier",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug)]
pub(super) struct Finish {
    pub unit: Unit,
    pub before: u64,
    pub after: u64,
}
impl Finish {
    pub(super) fn from_record(record: &Record) -> io::Result<Self> {
        if record.kind != FINISH || record.length != 88 || record.bytes.len() != RECORD_BYTES {
            return Err(invalid("copy5 End wire shape"));
        }
        let mut f = [0u64; 11];
        for (field, bytes) in f.iter_mut().zip(record.bytes[..88].chunks_exact(8)) {
            *field = u64::from_le_bytes(bytes.try_into().unwrap());
        }
        Ok(Self {
            unit: Unit {
                file: f[0],
                order: f[1],
                offset: f[2],
                requested: f[3],
                copied: f[4],
                returned: f[5] as i64,
                position: f[6],
                transport: f[7],
                disposition: f[8],
            },
            before: f[9],
            after: f[10],
        })
    }
}
