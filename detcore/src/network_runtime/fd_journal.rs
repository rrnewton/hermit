//! Run-owned immutable physical history. It contains no semantic FD table and
//! cannot issue current-slot, child-birth or lifetime-release authority.
#[path = "fd_journal/transport.rs"]
mod transport;
use std::collections::BTreeMap;
use std::io;

pub(super) use transport::Journal;

use super::accepted_provider::FdEvent;
use super::accepted_provider::FdStatus;

// Same maximum unresolved population as AP_FD_JOURNAL. Reclamation requires
// a future semantic publication receipt; never discard evidence to admit more.
const MAX_RETAINED: usize = 128;
#[derive(Debug, Default)]
pub(super) struct History {
    rows: BTreeMap<u64, FdEvent>,
    status: Option<FdStatus>,
    failed: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Transition {
    Install {
        begin: FdEvent,
        end: FdEvent,
    },
    Remove {
        begin: Option<FdEvent>,
        end: FdEvent,
    },
    Replace {
        begin: FdEvent,
        old: Option<FdEvent>,
        end: FdEvent,
    },
    FileRetired(FdEvent),
    Put {
        begin: FdEvent,
        retired: Option<FdEvent>,
        end: FdEvent,
    },
    Copy {
        begin: FdEvent,
        slots: Vec<FdEvent>,
        end: FdEvent,
    },
    Enrollment {
        begin: FdEvent,
        slots: Vec<FdEvent>,
        end: FdEvent,
    },
    Exec {
        begin: FdEvent,
        removed: Vec<FdEvent>,
        end: FdEvent,
    },
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}
fn require(ok: bool) -> io::Result<()> {
    if ok {
        Ok(())
    } else {
        Err(invalid("invalid physical journal relation"))
    }
}
fn same_actor(a: &FdEvent, b: &FdEvent) -> bool {
    a.task == b.task && a.task_start == b.task_start && a.sequence < b.sequence
}
fn clean(e: &FdEvent) -> bool {
    e.previous_file == 0 && e.accept_command == 0
}
impl History {
    pub(super) fn next(&self) -> io::Result<u64> {
        self.rows.last_key_value().map_or(Ok(1), |(n, _)| {
            n.checked_add(1)
                .ok_or_else(|| invalid("journal sequence exhausted"))
        })
    }
    /// The raw row and status survive validation failure. Neither C ACK nor a
    /// transport ACK means semantic publication. Same bytes may be recovered.
    pub(super) fn retain(&mut self, status: FdStatus, row: FdEvent) -> io::Result<()> {
        if let Some(error) = &self.failed {
            return Err(io::Error::other(error.clone()));
        }
        let next = self.next()?;
        if let Some(old) = self.rows.get(&row.sequence) {
            return require(old == &row && self.status.as_ref() == Some(&status));
        }
        if self.rows.len() >= MAX_RETAINED {
            return Err(invalid("unpublished journal capacity exhausted"));
        }
        let monotonic = self.status.as_ref().is_none_or(|old| {
            status.next_event >= old.next_event
                && status.next_file >= old.next_file
                && status.next_table >= old.next_table
        });
        let ordinal = row.sequence;
        self.status = Some(status.clone());
        self.rows.insert(ordinal, row);
        let result = (|| {
            require(
                monotonic && status.problem == 0 && ordinal == next && ordinal <= status.next_event,
            )?;
            let e = &self.rows[&ordinal];
            require(e.sequence != 0 && e.complete == 1 && e.task != 0 && e.task_start != 0)?;
            require(if e.kind == 21 {
                crate::fd::FdType::from_initial_profile(
                    e.mode,
                    e.status_flags,
                    e.device_major,
                    e.device_minor,
                )
                .is_some()
            } else {
                e.mode == 0 && e.status_flags == 0 && e.device_major == 0 && e.device_minor == 0
            })?;
            require(
                e.table <= status.next_table
                    && e.file <= status.next_file
                    && e.previous_file <= status.next_file,
            )?;
            self.validate(ordinal)
        })();
        if let Err(error) = &result {
            self.failed = Some(error.to_string());
        }
        result
    }
    fn row(&self, id: u64, kind: u64) -> io::Result<&FdEvent> {
        let e = self
            .rows
            .get(&id)
            .ok_or_else(|| invalid("missing journal dependency"))?;
        require(e.kind == kind)?;
        Ok(e)
    }
    fn children(&self, begin: &FdEvent, end: &FdEvent, kind: u64) -> Vec<FdEvent> {
        self.rows
            .range((begin.sequence + 1)..end.sequence)
            .filter(|&(_, e)| e.dependency == begin.sequence && e.kind == kind).map(|(_, e)| e.clone())
            .collect()
    }
    fn paired(&self, end: &FdEvent, kind: u64) -> io::Result<&FdEvent> {
        let begin = self.row(end.dependency, kind)?;
        require(same_actor(begin, end))?;
        // One invocation has one endpoint, including a failed native outcome.
        require(
            !self
                .rows
                .range((begin.sequence + 1)..end.sequence)
                .any(|(_, e)| {
                    e.dependency == begin.sequence
                        && matches!(e.kind, 2 | 3 | 6 | 11 | 13 | 16 | 19 | 22)
                }),
        )?;
        Ok(begin)
    }
    fn validate(&self, id: u64) -> io::Result<()> {
        let e = &self.rows[&id];
        match e.kind {
            1 => require(
                e.table != 0
                    && e.file != 0
                    && e.fd >= 0
                    && e.previous_file == 0
                    && e.dependency == 0
                    && e.returned == 0,
            ),
            2 => {
                let b = self.paired(e, 1)?;
                require(
                    e.table == b.table
                        && e.fd == b.fd
                        && e.file == b.file
                        && e.accept_command == b.accept_command
                        && e.previous_file == 0
                        && e.returned == 0,
                )
            }
            3 | 11 => {
                require(
                    e.table != 0
                        && clean(e)
                        && e.returned == 0
                        && ((e.kind == 3 && e.file != 0) || (e.kind == 11 && e.file == 0)),
                )?;
                if e.dependency == 0 {
                    return require(e.kind == 3);
                }
                let b = self.paired(e, 10)?;
                require(e.table == b.table && e.fd == b.fd)
            }
            4 => require(
                e.table != 0
                    && e.file != 0
                    && e.fd >= 0
                    && clean(e)
                    && e.dependency == 0
                    && e.returned == 0,
            ),
            5 => {
                let b = self.row(e.dependency, 4)?;
                require(
                    same_actor(b, e)
                        && e.table == b.table
                        && e.file == b.file
                        && e.fd == b.fd
                        && e.previous_file != 0
                        && e.accept_command == 0
                        && e.returned == 0,
                )?;
                require(
                    !self
                        .rows
                        .range((b.sequence + 1)..e.sequence)
                        .any(|(_, p)| p.dependency == b.sequence && matches!(p.kind, 5 | 6)),
                )
            }
            6 => {
                let b = self.paired(e, 4)?;
                let old = self.children(b, e, 5);
                require(
                    e.table == b.table
                        && e.fd == b.fd
                        && e.file == b.file
                        && e.accept_command == 0
                        && old.len() <= 1,
                )?;
                require(if e.returned < 0 {
                    e.returned >= -4095 && e.previous_file == 0 && old.is_empty()
                } else {
                    e.returned == e.fd
                        && old
                            .first()
                            .map_or(e.previous_file == 0, |o| o.previous_file == e.previous_file)
                })
            }
            7 => require(
                e.table == 0
                    && e.fd == -1
                    && e.file != 0
                    && clean(e)
                    && e.dependency == 0
                    && e.returned == 0,
            ),
            8 => Err(invalid("unresolved physical table mutation")),
            9 => {
                let b = self.row(e.dependency, 12)?;
                require(
                    same_actor(b, e)
                        && e.table == b.table
                        && e.fd == -1
                        && e.file == 0
                        && clean(e)
                        && e.returned == 0,
                )?;
                require(
                    !self
                        .rows
                        .range((b.sequence + 1)..e.sequence)
                        .any(|(_, p)| p.dependency == b.sequence && matches!(p.kind, 9 | 13)),
                )
            }
            10 => require(
                e.table != 0 && e.file == 0 && clean(e) && e.dependency == 0 && e.returned == 0,
            ),
            12 | 14 | 17 => require(
                e.table != 0
                    && e.fd == -1
                    && e.file == 0
                    && clean(e)
                    && e.dependency == 0
                    && e.returned == 0,
            ),
            13 => {
                require(e.table != 0 && e.fd == -1 && e.file == 0 && clean(e))?;
                let b = if e.returned == 0 {
                    let b = self.paired(e, 12)?;
                    require(
                        !self
                            .rows
                            .range((b.sequence + 1)..e.sequence)
                            .any(|(_, r)| r.kind == 9 && r.dependency == b.sequence),
                    )?;
                    b
                } else {
                    require(e.returned == 1)?;
                    let r = self.paired(e, 9)?;
                    require(r.table == e.table)?;
                    let b = self.row(r.dependency, 12)?;
                    require(same_actor(b, e))?;
                    require(
                        !self.rows.range((b.sequence + 1)..e.sequence).any(|(_, p)| {
                            p.kind == 13
                                && (p.dependency == b.sequence || p.dependency == r.sequence)
                        }),
                    )?;
                    b
                };
                require(b.table == e.table)
            }
            15 => {
                let b = self.row(e.dependency, 14)?;
                require(
                    same_actor(b, e)
                        && e.table != 0
                        && e.table != b.table
                        && e.fd >= 0
                        && e.fd < 256
                        && e.file != 0
                        && clean(e)
                        && matches!(e.returned, 0 | 1),
                )?;
                require(
                    !self.rows.range((b.sequence + 1)..e.sequence).any(|(_, p)| {
                        p.dependency == b.sequence
                            && (p.kind == 16
                                || (p.kind == 15 && (p.fd >= e.fd || p.table != e.table)))
                    }),
                )
            }
            16 => {
                let b = self.paired(e, 14)?;
                let slots = self.children(b, e, 15);
                require(e.file == 0 && clean(e))?;
                if e.returned < 0 {
                    require(e.returned >= -4095 && e.table == 0 && e.fd == -1 && slots.is_empty())
                } else {
                    require(
                        e.table != 0
                            && e.table != b.table
                            && e.fd > 0
                            && e.fd <= 256
                            && e.fd % 64 == 0
                            && e.returned as usize == slots.len()
                            && slots.iter().all(|s| s.table == e.table && s.fd < e.fd),
                    )
                }
            }
            18 => {
                let b = self.row(e.dependency, 17)?;
                require(
                    same_actor(b, e)
                        && e.table == b.table
                        && e.file != 0
                        && e.fd >= 0
                        && clean(e)
                        && e.returned == 0,
                )?;
                require(
                    !self.rows.range((b.sequence + 1)..e.sequence).any(|(_, p)| {
                        p.dependency == b.sequence
                            && (p.kind == 19 || (p.kind == 18 && p.fd >= e.fd))
                    }),
                )
            }
            19 => {
                let b = self.paired(e, 17)?;
                let removed = self.children(b, e, 18);
                require(
                    e.table == b.table
                        && e.fd == -1
                        && e.file == 0
                        && clean(e)
                        && e.returned >= 0
                        && e.returned as usize == removed.len(),
                )
            }
            20 => require(
                e.table != 0
                    && e.file == 0
                    && e.previous_file == 0
                    && e.fd == -1
                    && e.returned == 0
                    && e.dependency == 0
                    && e.accept_command != 0,
            ),
            21 => {
                let b = self.row(e.dependency, 20)?;
                require(
                    same_actor(b, e)
                        && e.table == b.table
                        && e.file != 0
                        && e.previous_file == 0
                        && e.accept_command == b.accept_command
                        && e.fd >= 0
                        && e.fd < 256
                        && matches!(e.returned, 0 | 1),
                )?;
                require(
                    !self.rows.range((b.sequence + 1)..e.sequence).any(|(_, p)| {
                        p.dependency == b.sequence
                            && (p.kind == 22 || (p.kind == 21 && p.fd >= e.fd))
                    }),
                )
            }
            22 => {
                let b = self.paired(e, 20)?;
                let slots = self.children(b, e, 21);
                require(
                    e.table == b.table
                        && e.file == 0
                        && e.previous_file == 0
                        && e.accept_command == b.accept_command
                        && e.returned >= 0
                        && e.returned as usize == slots.len()
                        && ((e.fd == -1 && slots.is_empty())
                            || (e.fd > 0 && e.fd <= 256 && slots.iter().all(|r| r.fd < e.fd))),
                )
            }
            _ => Err(invalid("unknown physical journal kind")),
        }
    }
    pub(super) fn transition(&self, id: u64) -> io::Result<Option<Transition>> {
        if let Some(error) = &self.failed {
            return Err(io::Error::other(error.clone()));
        }
        let e = self
            .rows
            .get(&id)
            .ok_or_else(|| invalid("journal endpoint not retained"))?;
        self.validate(id)?;
        Ok(match e.kind {
            2 => Some(Transition::Install {
                begin: self.row(e.dependency, 1)?.clone(),
                end: e.clone(),
            }),
            3 | 11 => Some(Transition::Remove {
                begin: if e.dependency == 0 {
                    None
                } else {
                    Some(self.row(e.dependency, 10)?.clone())
                },
                end: e.clone(),
            }),
            6 => {
                let b = self.row(e.dependency, 4)?;
                Some(Transition::Replace {
                    begin: b.clone(),
                    old: self.children(b, e, 5).into_iter().next(),
                    end: e.clone(),
                })
            }
            7 => Some(Transition::FileRetired(e.clone())),
            13 => {
                let retired = if e.returned == 1 {
                    Some(self.row(e.dependency, 9)?.clone())
                } else {
                    None
                };
                let begin = self
                    .row(retired.as_ref().map_or(e.dependency, |r| r.dependency), 12)?
                    .clone();
                Some(Transition::Put {
                    begin,
                    retired,
                    end: e.clone(),
                })
            }
            16 => {
                let b = self.row(e.dependency, 14)?;
                Some(Transition::Copy {
                    begin: b.clone(),
                    slots: self.children(b, e, 15),
                    end: e.clone(),
                })
            }
            19 => {
                let b = self.row(e.dependency, 17)?;
                Some(Transition::Exec {
                    begin: b.clone(),
                    removed: self.children(b, e, 18),
                    end: e.clone(),
                })
            }
            22 => {
                let b = self.row(e.dependency, 20)?;
                Some(Transition::Enrollment {
                    begin: b.clone(),
                    slots: self.children(b, e, 21),
                    end: e.clone(),
                })
            }
            _ => None,
        })
    }
    pub(super) fn selection_prefix(&self, through: u64) -> io::Result<()> {
        require(
            self.failed.is_none()
                && self.next()? > through
                && (through == 0
                    || self
                        .rows
                        .first_key_value()
                        .is_some_and(|(first, _)| *first == 1)),
        )
    }
    /// Proof-only origins through an authenticated post-fdget upper frontier.
    /// This is deliberately not a reconstruction of the current numeric slot:
    /// another CPU may remove/reinstall it before the post callback runs.
    pub(super) fn unique_selection_origin(
        &self,
        through: u64,
        table: u64,
        fd: i32,
        file: u64,
    ) -> io::Result<FdEvent> {
        self.selection_prefix(through)?;
        require(table != 0 && fd >= 0 && file != 0)?;
        let mut origins = self.rows.range(..=through).filter_map(|(_, row)| {
            (row.table == table
                && row.fd == fd
                && row.file == file
                && matches!(row.kind, 1 | 4 | 15 | 21))
            .then_some(row)
        });
        let origin = origins
            .next()
            .ok_or_else(|| invalid("selected native file has no retained origin"))?;
        // A begin whose mutation later fails still makes this conservative join
        // ambiguous. Never remove it to manufacture a unique generation.
        require(origins.next().is_none())?;
        self.validate(origin.sequence)?;
        Ok(origin.clone())
    }

    /// This is immutable initial provenance, not a current-slot capability.
    /// Only a completed op6 plus its full journal relation can construct it.
    pub(super) fn enrollment(
        &self,
        binding: &super::physical::CollectedEnrollment,
    ) -> io::Result<super::physical::InitialTableAssociation> {
        use super::physical::InitialTableAssociation;
        let owner = binding.owner();
        let ticket = binding.ticket();
        let observation = binding.observation();
        let r = &observation.raw.command;
        let e = &observation.raw.enrollment;
        require(observation.status.returned == 0 && observation.status.errno.is_none())?;
        require(
            ticket.command != 0
                && ticket.registration != 0
                && ticket.prepared_request != 0
                && r.command == ticket.command
                && r.operation == 6
                && r.phase == 1
                && r.identity.provider != 0
                && r.identity.object == 0
                && r.identity.namespace == 0
                && r.creation == 0
                && r.cookie == 0
                && r.reserved == 0
                && r.returned <= 0
                && r.returned >= -4095
                && r.start_boottime != 0
                // TASK_STORAGE/pidfd binds the kernel task to this local owner.
                // The scalar namespaces intentionally need not be equal.
                && r.task >> 32 != 0
                && r.task as u32 != 0,
        )?;
        require(
            e.command == r.command
                && e.registration == ticket.registration
                && e.owner_mm == owner.mm.generation()
                && e.task == r.task
                && e.task_start == r.start_boottime
                && e.table != 0
                && e.expected_table == 0
                && e.mode == 1
                && e.phases == 7
                && e.problem == 0
                && e.reserved == 0
                && e.references == 1
                && e.ptrace_return == r.returned
                && e.slots > 0
                && e.slots <= 256
                && e.files <= e.slots
                && e.begin != 0
                && e.end > e.begin,
        )?;
        let Some(Transition::Enrollment { begin, slots, end }) = self.transition(e.end)? else {
            return Err(invalid("enrollment lacks its full census"));
        };
        require(
            begin.sequence == e.begin
                && begin.task == e.task
                && begin.task_start == e.task_start
                && begin.table == e.table
                && begin.accept_command == e.command
                && end.fd == e.slots as i32
                && slots.len() == e.files as usize,
        )?;
        // Repeated slots naming the same actual struct file must carry the
        // same inode kind/status observation; aliases cannot split OFD state.
        let mut profiles = std::collections::BTreeMap::new();
        for slot in &slots {
            let profile = (
                slot.mode,
                slot.status_flags,
                slot.device_major,
                slot.device_minor,
            );
            require(
                profiles
                    .insert(slot.file, profile)
                    .is_none_or(|old| old == profile),
            )?;
        }
        Ok(InitialTableAssociation::from_checked_journal(
            binding, slots,
        ))
    }
    /// Include every same-slot/lifecycle row through a freshly requested cut.
    /// Only the original installation's two endpoints are excluded. In
    /// particular a removal exactly at `through` remains interference.
    pub(super) fn contains_command(&self, command: u64) -> io::Result<bool> {
        // Until demand-aware prefix retirement is connected, this query requires
        // the retained origin. It must not become vacuous after reclamation.
        require(
            command != 0
                && self.failed.is_none()
                && self
                    .rows
                    .first_key_value()
                    .is_none_or(|(first, _)| *first == 1),
        )?;
        Ok(self.rows.values().any(|row| row.accept_command == command))
    }

    pub(super) fn installation_interference(
        &self,
        begin: u64,
        end: u64,
        through: u64,
        table: u64,
        fd: i32,
    ) -> Vec<u64> {
        self.rows
            .values()
            .filter(|e| {
                e.sequence <= through
                    && e.sequence != begin
                    && e.sequence != end
                    && e.table == table
                    && (e.fd == fd || matches!(e.kind, 9 | 12 | 13 | 17 | 19))
                    && (e.sequence >= begin
                        || matches!(e.kind, 1 | 4 | 10 | 12 | 17)
                            && !self.rows.values().any(|r| {
                                r.dependency == e.sequence
                                    && r.sequence < begin
                                    && matches!(r.kind, 2 | 3 | 6 | 11 | 13 | 19)
                            }))
            })
            .map(|e| e.sequence)
            .collect()
    }

    /// Full raw population relevant to this one installation, including actual
    /// file retirement whose event has no table/FD. Unrelated rows remain owned
    /// by History; returning this subset does not acknowledge or reclaim them.
    pub(super) fn installation_effect_rows(
        &self,
        begin: u64,
        end: u64,
        through: u64,
        table: u64,
        fd: i32,
        file: u64,
    ) -> io::Result<Vec<FdEvent>> {
        require(
            self.failed.is_none()
                && begin != 0
                && end > begin
                && through >= end
                && self.next()? > through
                && file != 0,
        )?;
        let interfering = self.installation_interference(begin, end, through, table, fd);
        // Cross-slot/table aliases of this actual file are not unrelated:
        // until those effects are published, retiring our sole semantic binding
        // would forget a physically live alias. Retain them for refusal rather
        // than treating the absence of FileRetired as permission to drop them.
        self.rows
            .values()
            .filter(|row| {
                row.sequence <= through
                    && row.sequence != begin
                    && row.sequence != end
                    && (interfering.contains(&row.sequence)
                        || row.sequence >= begin && (row.file == file || row.previous_file == file))
            })
            .map(|row| {
                self.validate(row.sequence)?;
                Ok(row.clone())
            })
            .collect()
    }

    /// Historical same-slot interference through the retained prefix. It is
    /// deliberately not a current-slot verdict; later callbacks can still exist.
    pub(super) fn interference(&self, begin: u64, end: u64, table: u64, fd: i32) -> Vec<u64> {
        self.rows
            .values()
            .filter(|e| {
                e.sequence <= end
                    && e.sequence != begin
                    && e.sequence != end
                    && e.table == table
                    && (e.fd == fd || matches!(e.kind, 9 | 12 | 13 | 17 | 19))
                    && (e.sequence >= begin
                        || matches!(e.kind, 1 | 4 | 10 | 12 | 17)
                            && !self.rows.values().any(|r| {
                                r.dependency == e.sequence
                                    && r.sequence < begin
                                    && matches!(r.kind, 2 | 3 | 6 | 11 | 13 | 19)
                            }))
            })
            .map(|e| e.sequence)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(sequence: u64, kind: u64) -> FdEvent {
        FdEvent {
            sequence,
            kind,
            task: 10,
            task_start: 11,
            table: 1,
            file: 0,
            previous_file: 0,
            dependency: 0,
            accept_command: 0,
            fd: -1,
            returned: 0,
            complete: 1,
            mode: 0,
            status_flags: 0,
            device_major: 0,
            device_minor: 0,
        }
    }
    fn retain(h: &mut History, e: FdEvent) -> io::Result<()> {
        h.retain(
            FdStatus {
                problem: 0,
                next_table: 8,
                next_file: 9,
                next_event: e.sequence,
            },
            e,
        )
    }
    fn install(begin: u64, end: u64, file: u64) -> (FdEvent, FdEvent) {
        let mut b = event(begin, 1);
        b.fd = 3;
        b.file = file;
        b.accept_command = 7;
        let mut e = b.clone();
        e.sequence = end;
        e.kind = 2;
        e.dependency = begin;
        (b, e)
    }
    #[test]
    fn exact_installation_is_historical_and_requires_full_link() {
        let (b, e) = install(1, 2, 1);
        let mut h = History::default();
        retain(&mut h, b.clone()).unwrap();
        assert_eq!(h.transition(1).unwrap(), None);
        assert!(h.transition(2).is_err());
        retain(&mut h, e.clone()).unwrap();
        assert_eq!(
            h.transition(2).unwrap(),
            Some(Transition::Install {
                begin: b.clone(),
                end: e.clone()
            })
        );
        for field in 0..9 {
            let mut bad = e.clone();
            match field {
                0 => bad.task += 1,
                1 => bad.task_start += 1,
                2 => bad.table += 1,
                3 => bad.file += 1,
                4 => bad.fd += 1,
                5 => bad.accept_command += 1,
                6 => bad.dependency = 0,
                7 => bad.previous_file = 1,
                _ => bad.complete = 2,
            };
            let mut h = History::default();
            retain(&mut h, b.clone()).unwrap();
            assert!(retain(&mut h, bad).is_err(), "field {field}");
            assert!(h.transition(1).is_err());
        }
    }
    #[test]
    fn fdupfd_install_interval_retains_concurrent_remove_and_reuse() {
        let mut h = History::default();
        let (mut original, mut completed) = install(1, 5, 1);
        original.accept_command = 0;
        completed.accept_command = 0;
        let mut removed = event(2, 3);
        removed.task = 20;
        removed.task_start = 21;
        removed.fd = 3;
        removed.file = 1;
        let (mut replacement, mut replaced) = install(3, 4, 2);
        for row in [&mut replacement, &mut replaced] {
            row.task = 20;
            row.task_start = 21;
            row.accept_command = 0;
        }
        for row in [&original, &removed, &replacement, &replaced, &completed] {
            retain(&mut h, row.clone()).unwrap();
        }
        assert_eq!(
            h.transition(5).unwrap(),
            Some(Transition::Install {
                begin: original,
                end: completed
            })
        );
        assert_eq!(
            h.transition(2).unwrap(),
            Some(Transition::Remove {
                begin: None,
                end: removed
            })
        );
        assert_eq!(
            h.transition(4).unwrap(),
            Some(Transition::Install {
                begin: replacement,
                end: replaced
            })
        );
        assert_eq!(h.interference(1, 5, 1, 3), vec![2, 3, 4]);
        assert_eq!(h.interference(3, 4, 1, 3), vec![1]);
        // Late original END cannot assert current slot ownership of file1.
        assert_eq!(h.rows[&4].file, 2);
        assert_eq!(h.rows[&5].file, 1);
    }

    #[test]
    fn fdupfd_install_interval_retains_linked_close_and_reuse() {
        let mut h = History::default();
        let (mut original, mut completed) = install(1, 6, 1);
        original.accept_command = 0;
        completed.accept_command = 0;
        let mut remove_begin = event(2, 10);
        remove_begin.task = 20;
        remove_begin.task_start = 21;
        remove_begin.fd = 3;
        let mut removed = remove_begin.clone();
        removed.sequence = 3;
        removed.kind = 3;
        removed.file = 1;
        removed.dependency = remove_begin.sequence;
        let (mut replacement, mut replaced) = install(4, 5, 2);
        for row in [&mut replacement, &mut replaced] {
            row.task = 20;
            row.task_start = 21;
            row.accept_command = 0;
        }
        for row in [&original, &remove_begin, &removed, &replacement, &replaced] {
            retain(&mut h, row.clone()).unwrap();
        }
        assert_eq!(h.transition(1).unwrap(), None);
        assert_eq!(h.transition(2).unwrap(), None);
        assert_eq!(
            h.transition(3).unwrap(),
            Some(Transition::Remove {
                begin: Some(remove_begin.clone()),
                end: removed.clone(),
            })
        );
        assert_eq!(
            h.transition(5).unwrap(),
            Some(Transition::Install {
                begin: replacement,
                end: replaced,
            })
        );
        retain(&mut h, completed.clone()).unwrap();
        assert_eq!(
            h.transition(6).unwrap(),
            Some(Transition::Install {
                begin: original.clone(),
                end: completed,
            })
        );
        assert_eq!(h.interference(1, 6, 1, 3), vec![2, 3, 4, 5]);
        assert_eq!(h.interference(2, 3, 1, 3), vec![1]);
        assert_eq!(h.interference(4, 5, 1, 3), vec![1]);
        // The closed file and late alias END remain historical file1 facts;
        // neither may rewrite the intervening replacement's file2 identity.
        assert_eq!(h.rows[&3].file, 1);
        assert_eq!(h.rows[&4].file, 2);
        assert_eq!(h.rows[&5].file, 2);
        assert_eq!(h.rows[&6].file, 1);
        for field in 0..5 {
            let mut bad = removed.clone();
            match field {
                0 => bad.task += 1,
                1 => bad.task_start += 1,
                2 => bad.dependency = original.sequence,
                3 => bad.table += 1,
                _ => bad.fd += 1,
            }
            let mut h = History::default();
            retain(&mut h, original.clone()).unwrap();
            retain(&mut h, remove_begin.clone()).unwrap();
            assert!(retain(&mut h, bad).is_err(), "linked close field {field}");
            assert!(h.transition(3).is_err());
        }
    }

    #[test]
    fn post_unlock_remove_preserves_old_file_when_reinstall_finishes_first() {
        let mut h = History::default();
        let mut b = event(1, 10);
        b.fd = 3;
        retain(&mut h, b.clone()).unwrap();
        let (i, j) = install(2, 3, 2);
        retain(&mut h, i).unwrap();
        retain(&mut h, j).unwrap();
        let mut end = event(4, 3);
        end.fd = 3;
        end.file = 1;
        end.dependency = 1;
        retain(&mut h, end.clone()).unwrap();
        assert_eq!(
            h.transition(4).unwrap(),
            Some(Transition::Remove {
                begin: Some(b),
                end
            })
        );
        assert_eq!(h.interference(2, 3, 1, 3), vec![1]);
        // No numeric-slot map exists here: the late END cannot erase file2.
    }
    #[test]
    fn no_file_is_not_an_errno_and_requires_the_same_invocation() {
        let mut b = event(1, 10);
        b.fd = -1;
        let mut e = event(2, 11);
        e.fd = -1;
        e.dependency = 1;
        let mut h = History::default();
        retain(&mut h, b.clone()).unwrap();
        retain(&mut h, e.clone()).unwrap();
        assert!(matches!(
            h.transition(2).unwrap(),
            Some(Transition::Remove { .. })
        ));
        for n in 0..3 {
            let mut h = History::default();
            retain(&mut h, b.clone()).unwrap();
            let mut e = e.clone();
            match n {
                0 => e.file = 1,
                1 => e.returned = -9,
                _ => e.dependency = 0,
            };
            assert!(retain(&mut h, e).is_err());
        }
    }
    #[test]
    fn replacement_retains_actual_old_file_and_rejects_old_file_on_failure() {
        let mut b = event(1, 4);
        b.fd = 3;
        b.file = 2;
        let mut old = b.clone();
        old.sequence = 2;
        old.kind = 5;
        old.previous_file = 1;
        old.dependency = 1;
        let mut end = old.clone();
        end.sequence = 3;
        end.kind = 6;
        end.returned = 3;
        let mut h = History::default();
        for e in [b.clone(), old.clone(), end.clone()] {
            retain(&mut h, e).unwrap();
        }
        assert!(matches!(
            h.transition(3).unwrap(),
            Some(Transition::Replace { old: Some(_), .. })
        ));
        for outcome in [-9, 4] {
            let mut h = History::default();
            retain(&mut h, b.clone()).unwrap();
            retain(&mut h, old.clone()).unwrap();
            let mut e = end.clone();
            e.returned = outcome;
            assert!(retain(&mut h, e).is_err());
        }
    }
    #[test]
    fn final_and_nonfinal_put_use_captured_table_generation() {
        let b = event(1, 12);
        let mut r = event(2, 9);
        r.dependency = 1;
        let mut end = event(3, 13);
        end.dependency = 2;
        end.returned = 1;
        let mut h = History::default();
        for e in [b.clone(), r.clone(), end.clone()] {
            retain(&mut h, e).unwrap();
        }
        assert!(matches!(
            h.transition(3).unwrap(),
            Some(Transition::Put {
                retired: Some(_),
                ..
            })
        ));
        let mut h = History::default();
        retain(&mut h, b.clone()).unwrap();
        let mut other = event(2, 12);
        other.task = 12;
        retain(&mut h, other.clone()).unwrap();
        let mut retired = r;
        retired.sequence = 3;
        retired.task = 12;
        retired.dependency = 2;
        retain(&mut h, retired).unwrap();
        let mut nonfinal = event(4, 13);
        nonfinal.dependency = 1;
        retain(&mut h, nonfinal).unwrap();
        assert!(matches!(
            h.transition(4).unwrap(),
            Some(Transition::Put { retired: None, .. })
        ));
        // Actual another-task retirement does not invalidate this captured return.
        for n in 0..3 {
            let mut h = History::default();
            retain(&mut h, b.clone()).unwrap();
            let mut e = end.clone();
            e.sequence = 2;
            match n {
                0 => e.dependency = 1,
                1 => e.returned = 2,
                _ => e.table = 2,
            };
            assert!(retain(&mut h, e).is_err());
        }
    }
    #[test]
    fn fork_snapshot_requires_complete_census_and_distinct_actual_table() {
        let b = event(1, 14);
        let mut slot = event(2, 15);
        slot.table = 2;
        slot.fd = 3;
        slot.file = 1;
        slot.dependency = 1;
        slot.returned = 1;
        let mut end = event(3, 16);
        end.table = 2;
        end.fd = 128;
        end.returned = 1;
        end.dependency = 1;
        let mut h = History::default();
        for e in [b.clone(), slot.clone(), end.clone()] {
            retain(&mut h, e).unwrap();
        }
        assert!(
            matches!(h.transition(3).unwrap(),Some(Transition::Copy{slots,..}) if slots.len()==1)
        );
        for n in 0..6 {
            let mut h = History::default();
            retain(&mut h, b.clone()).unwrap();
            retain(&mut h, slot.clone()).unwrap();
            let mut e = end.clone();
            match n {
                0 => e.returned = 0,
                1 => e.table = 1,
                2 => e.fd = 127,
                3 => e.fd = 512,
                4 => e.task_start += 1,
                _ => e.returned = -12,
            };
            assert!(retain(&mut h, e).is_err());
        }
        let mut h = History::default();
        retain(&mut h, b).unwrap();
        retain(&mut h, slot.clone()).unwrap();
        slot.sequence = 3;
        assert!(retain(&mut h, slot).is_err());
    }
    #[test]
    fn exec_distinguishes_two_alias_slots_of_one_file() {
        let b = event(1, 17);
        let mut a = event(2, 18);
        a.fd = 3;
        a.file = 1;
        a.dependency = 1;
        let mut c = a.clone();
        c.sequence = 3;
        c.fd = 4;
        let mut end = event(4, 19);
        end.dependency = 1;
        end.returned = 2;
        let mut h = History::default();
        for e in [b.clone(), a.clone(), c.clone(), end.clone()] {
            retain(&mut h, e).unwrap();
        }
        assert!(
            matches!(h.transition(4).unwrap(),Some(Transition::Exec{removed,..}) if removed.iter().map(|e|e.fd).collect::<Vec<_>>()==vec![3,4])
        );
        for n in 0..3 {
            let mut h = History::default();
            retain(&mut h, b.clone()).unwrap();
            retain(&mut h, a.clone()).unwrap();
            let mut e = c.clone();
            match n {
                0 => e.fd = 3,
                1 => e.table = 2,
                _ => e.returned = 1,
            };
            assert!(retain(&mut h, e).is_err());
        }
        let mut h = History::default();
        for e in [b, a, c] {
            retain(&mut h, e).unwrap();
        }
        end.returned = 1;
        assert!(retain(&mut h, end).is_err());
    }
    #[test]
    fn malformed_status_and_event_leave_sticky_raw_failure() {
        for n in 0..7 {
            let mut h = History::default();
            let mut e = event(1, 7);
            e.table = 0;
            e.file = 1;
            let mut s = FdStatus {
                problem: 0,
                next_table: 1,
                next_file: 1,
                next_event: 1,
            };
            match n {
                0 => s.problem = 32,
                1 => e.kind = 8,
                2 => e.kind = 20,
                3 => e.sequence = 2,
                4 => e.task = 0,
                5 => s.next_file = 0,
                _ => e.complete = 2,
            };
            assert!(h.retain(s, e).is_err());
            assert_eq!(h.rows.len(), 1);
            assert!(h.failed.is_some());
            assert!(retain(&mut h, event(2, 12)).is_err());
        }
    }
    #[test]
    fn fixed_unpublished_capacity_never_drops_old_evidence() {
        let mut h = History::default();
        for id in 1..=128 {
            let mut e = event(id, 7);
            e.table = 0;
            e.file = 1;
            retain(&mut h, e).unwrap();
        }
        let mut e = event(129, 7);
        e.table = 0;
        e.file = 1;
        assert!(retain(&mut h, e).is_err());
        assert_eq!(h.rows.len(), 128);
        assert!(h.transition(1).is_ok());
    }
}

#[cfg(test)]
mod enrollment_tests {
    use super::super::accepted_provider::CallStatus;
    use super::super::accepted_provider::Observation;
    use super::super::accepted_provider::TableEnrollmentEffect;
    use super::super::accepted_provider_ffi as ffi;
    use super::super::physical::CustodyTasks;
    use super::super::physical::InitialTableTicket;
    use super::*;
    use crate::network_replay::NetworkStreamOwner;
    use crate::types::DetTid;
    use crate::types::MmId;
    fn owner() -> NetworkStreamOwner {
        let thread = DetTid::from_raw(71);
        NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        }
    }
    fn row(n: u64, kind: u64, fd: i32, file: u64, dependency: u64) -> FdEvent {
        ffi::FdEvent {
            sequence: n,
            kind,
            task: (70071 << 32) | 70072,
            task_start: 23,
            table: 1,
            file,
            fd,
            dependency,
            accept_command: 41,
            complete: 1,
            mode: if kind == 21 { 0o010600 } else { 0 },
            status_flags: if kind == 21 { libc::O_WRONLY as u32 } else { 0 },
            ..Default::default()
        }
        .into()
    }
    fn populated(
        empty: bool,
        native: i32,
    ) -> (
        History,
        InitialTableTicket,
        Observation<TableEnrollmentEffect>,
    ) {
        let mut h = History::default();
        let ticket = InitialTableTicket {
            registration: 1,
            prepared_request: 1,
            command: 41,
        };
        let last = if empty { 2 } else { 4 };
        let status = ffi::FdStatus {
            next_table: 1,
            next_file: 1,
            next_event: last,
            problem: 0,
        };
        h.retain(status.into(), row(1, 20, -1, 0, 0)).unwrap();
        if !empty {
            h.retain(status.into(), row(2, 21, 0, 1, 1)).unwrap();
            let mut alias = row(3, 21, 7, 1, 1);
            alias.returned = 1;
            h.retain(status.into(), alias).unwrap();
        }
        let mut end = row(last, 22, 64, 0, 1);
        end.returned = if empty { 0 } else { 2 };
        h.retain(status.into(), end).unwrap();
        let raw = ffi::TableEnrollmentEffect {
            command: ffi::CommandResult {
                command: 41,
                operation: 6,
                task: (70071 << 32) | 70072,
                start_boottime: 23,
                identity: ffi::Identity {
                    provider: 3,
                    ..Default::default()
                },
                returned: native,
                phase: 1,
                ..Default::default()
            },
            enrollment: ffi::FdEnrollment {
                command: 41,
                registration: 1,
                owner_mm: owner().mm.generation(),
                task: (70071 << 32) | 70072,
                task_start: 23,
                table: 1,
                begin: 1,
                end: last,
                phases: 7,
                slots: 64,
                files: if empty { 0 } else { 2 },
                references: 1,
                mode: 1,
                ptrace_return: native,
                ..Default::default()
            },
        };
        (
            h,
            ticket,
            Observation {
                status: CallStatus {
                    operation: "collect".into(),
                    returned: 0,
                    errno: None,
                },
                raw: raw.into(),
            },
        )
    }
    fn custody(
        out: &Observation<TableEnrollmentEffect>,
        native_ok: bool,
    ) -> (CustodyTasks<i32>, InitialTableTicket) {
        let mut tasks = CustodyTasks::default();
        tasks.register(owner(), 71, 71, || Ok(5)).unwrap();
        assert_eq!(tasks.begin_initial(owner()).unwrap(), 1);
        tasks.retain_preparation(owner(), Ok(1)).unwrap();
        let ticket = tasks.prepared(owner(), 1, 41).unwrap();
        tasks.native_read(owner(), ticket, native_ok).unwrap();
        tasks.retain_collection(owner(), Ok(2)).unwrap();
        tasks.retain_raw(owner(), Ok(out.clone())).unwrap();
        (tasks, ticket)
    }
    fn enroll(
        h: &History,
        ticket: InitialTableTicket,
        out: &Observation<TableEnrollmentEffect>,
    ) -> io::Result<super::super::physical::InitialTableAssociation> {
        let (tasks, _) = custody(out, true);
        h.enrollment(&tasks.collected_binding(owner(), ticket, 2)?)
    }
    fn semantic_claim(
        a: &super::super::physical::InitialTableAssociation,
    ) -> super::super::physical::InitialTableClaim {
        use crate::types::FdSlot;
        use crate::types::FdSlotBinding;
        use crate::types::FilesId;
        use crate::types::NetworkFdSlot;
        use crate::types::OpenFileId;
        let view = a.view();
        let slots = view
            .descriptors
            .iter()
            .enumerate()
            .map(|(at, row)| NetworkFdSlot {
                binding: FdSlotBinding {
                    slot: FdSlot {
                        files: FilesId::initial(owner().thread),
                        fd: row.fd,
                    },
                    generation: 4 + at as u64,
                    open_file: OpenFileId::new(owner().thread, 3),
                },
                cloexec: row.cloexec,
            })
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        let metadata = view
            .descriptors
            .iter()
            .filter(|row| seen.insert(row.physical_file))
            .map(|row| super::super::physical::InitialFileStat {
                fd: row.fd,
                physical_file: row.physical_file,
                stat: crate::stat::DetStat {
                    mode: row.mode,
                    rdev: libc::makedev(row.device_major, row.device_minor),
                    inode: 101,
                    ..crate::stat::DetStat::default()
                },
            })
            .collect();
        super::super::physical::InitialTableClaim {
            metadata,
            through_generation: 3 + view.descriptors.len() as u64,
            view,
            slots,
            base_generation: 3,
            base_regular_sequence: 3,
            base_socket_sequence: 0,
        }
    }
    #[test]
    fn private_census_admits_exact_alias_claim_once_after_lost_reply() {
        use std::cell::Cell;
        let (h, ticket, out) = populated(false, 0);
        let a = enroll(&h, ticket, &out).unwrap();
        assert_ne!(
            a.view().owner.thread.as_raw() as u64,
            out.raw.enrollment.task & 0xffff_ffff
        );
        let (mut tasks, _) = custody(&out, true);
        tasks.complete(owner(), a.clone()).unwrap();
        let claim = semantic_claim(&a);
        let calls = Cell::new(0);
        let validations = Cell::new(0);
        for _ in 0..2 {
            tasks
                .admit_semantics(owner(), claim.clone(), |actual, proposed, _, previous| {
                    assert_eq!(actual, &a);
                    assert_eq!(proposed, &claim);
                    validations.set(validations.get() + 1);
                    if let Some(previous) = previous {
                        assert_eq!(previous, &Ok(()));
                        assert_eq!(calls.get(), 1);
                        return previous.clone().map_err(io::Error::other);
                    }
                    assert_eq!(calls.get(), 0);
                    calls.set(calls.get() + 1);
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(calls.get(), 1);
        assert_eq!(validations.get(), 2);
        let mut changed_stat = claim.clone();
        changed_stat.metadata[0].stat.inode += 1;
        assert!(
            tasks
                .admit_semantics(owner(), changed_stat, |_, _, _, _| panic!(
                    "changed native observation executed"
                ))
                .is_err()
        );
        // A plausible higher generation is not the exact retained claim.
        let mut changed = claim;
        changed.base_generation += 1;
        changed.through_generation += 1;
        for slot in &mut changed.slots {
            slot.binding.generation += 1;
        }
        assert!(
            tasks
                .admit_semantics(owner(), changed, |_, _, _, _| panic!(
                    "changed claim executed"
                ))
                .is_err()
        );
    }
    #[test]
    fn initial_semantic_claim_cannot_omit_rebind_split_alias_or_change_profile() {
        use crate::types::DetTid;
        use crate::types::FilesId;
        use crate::types::OpenFileId;
        let (h, ticket, out) = populated(false, 0);
        let a = enroll(&h, ticket, &out).unwrap();
        let good = semantic_claim(&a);
        a.check_claim(&good).unwrap();
        for case in 0..12 {
            let mut bad = good.clone();
            match case {
                0 => {
                    bad.slots.pop();
                }
                1 => bad.view.registration += 1,
                2 => bad.view.table += 1,
                3 => bad.view.descriptors[0].physical_file += 1,
                4 => bad.view.descriptors[0].status_flags ^= libc::O_NONBLOCK as u32,
                5 => bad.view.descriptors[0].mode = 0o100600,
                6 => bad.slots[0].binding.slot.files = FilesId::initial(DetTid::from_raw(999)),
                7 => bad.slots[1].binding.open_file = OpenFileId::new(owner().thread, 4),
                8 => bad.slots[0].binding.open_file = OpenFileId::new(DetTid::from_raw(999), 3),
                9 => bad.slots[1].cloexec = false,
                10 => bad.slots[1].binding.generation += 1,
                11 => bad.through_generation += 1,
                _ => unreachable!(),
            }
            assert!(a.check_claim(&bad).is_err(), "case {case}");
        }
    }
    #[test]
    fn anonymous_and_device_profiles_pass_the_actual_retained_journal_path() {
        for (mode, major, minor) in [(0, 0, 0), (0o600, 0, 0), (0o020600, 1, 8), (0o020600, 1, 9)] {
            let (old, ticket, out) = populated(false, 0);
            let mut history = History::default();
            for mut row in old.rows.into_values() {
                if row.kind == 21 {
                    row.mode = mode;
                    row.device_major = major;
                    row.device_minor = minor;
                }
                history.retain(old.status.clone().unwrap(), row).unwrap();
            }
            let association = enroll(&history, ticket, &out).unwrap();
            let view = association.view();
            assert_eq!(view.descriptors.len(), 2);
            for slot in view.descriptors {
                assert_eq!(
                    (slot.mode, slot.device_major, slot.device_minor),
                    (mode, major, minor)
                );
            }
            if major != 0 {
                let mut altered = history;
                altered.rows.get_mut(&3).unwrap().device_minor ^= 1;
                assert!(enroll(&altered, ticket, &out).is_err());
            }
        }
        for (mode, major, minor) in [
            (0o030600, 0, 0),
            (0o200600, 0, 0),
            (0o600, 1, 8),
            (0o020600, 0x1000, 8),
            (0o020600, 1, 0x100000),
        ] {
            let (old, _, _) = populated(false, 0);
            let mut history = History::default();
            history
                .retain(old.status.clone().unwrap(), old.rows[&1].clone())
                .unwrap();
            let mut bad = old.rows[&2].clone();
            bad.mode = mode;
            bad.device_major = major;
            bad.device_minor = minor;
            assert!(history.retain(old.status.unwrap(), bad.clone()).is_err());
            assert_eq!(history.rows[&2], bad);
            assert!(history.failed.is_some());
        }
    }

    #[test]
    fn profile_alias_mismatch_and_non_slot_profile_remain_raw_failures() {
        let (mut h, t, out) = populated(false, 0);
        h.rows.get_mut(&3).unwrap().status_flags ^= libc::O_NONBLOCK as u32;
        assert!(enroll(&h, t, &out).is_err());
        let status = ffi::FdStatus {
            next_event: 1,
            next_table: 1,
            ..Default::default()
        };
        let mut h = History::default();
        let mut bad = row(1, 20, -1, 0, 0);
        bad.mode = 0o100600;
        assert!(h.retain(status.into(), bad.clone()).is_err());
        assert_eq!(h.rows[&1], bad);
        assert!(h.failed.is_some());
    }
    #[test]
    fn exact_nonempty_empty_and_native_error_census_are_distinct() {
        for empty in [false, true] {
            let (h, ticket, out) = populated(empty, 0);
            let a = enroll(&h, ticket, &out).unwrap();
            assert_eq!(a.owner(), owner());
            assert_eq!(a.table(), 1);
            assert_eq!(a.native_return(), 0);
            let Some(Transition::Enrollment { slots, .. }) =
                h.transition(out.raw.enrollment.end).unwrap()
            else {
                panic!()
            };
            assert_eq!(slots.len(), if empty { 0 } else { 2 });
            if !empty {
                assert_eq!(slots[0].file, slots[1].file);
                assert_eq!(slots[1].returned, 1)
            }
        }
        let (h, t, out) = populated(true, -libc::EFAULT);
        assert_eq!(enroll(&h, t, &out).unwrap().native_return(), -libc::EFAULT);
    }
    #[test]
    fn census_rejects_each_unproven_identity_completion_and_count() {
        let (h, t, out) = populated(false, 0);
        for which in 0..20 {
            let mut bad = out.clone();
            match which {
                0 => bad.status.returned = -1,
                1 => bad.raw.command.command += 1,
                2 => bad.raw.command.operation = 4,
                3 => bad.raw.command.phase = 3,
                4 => bad.raw.command.task += 1,
                5 => bad.raw.command.start_boottime = 0,
                6 => bad.raw.command.identity.object = 1,
                7 => bad.raw.enrollment.registration += 1,
                8 => bad.raw.enrollment.owner_mm += 1,
                9 => bad.raw.enrollment.task_start += 1,
                10 => bad.raw.enrollment.table += 1,
                11 => bad.raw.enrollment.begin += 1,
                12 => bad.raw.enrollment.end += 1,
                13 => bad.raw.enrollment.expected_table = 1,
                14 => bad.raw.enrollment.mode = 2,
                15 => bad.raw.enrollment.references = 2,
                16 => bad.raw.enrollment.files = 1,
                17 => bad.raw.enrollment.slots = 63,
                18 => bad.raw.enrollment.ptrace_return = -libc::EFAULT,
                19 => bad.raw.enrollment.problem = 1,
                _ => unreachable!(),
            }
            assert!(enroll(&h, t, &bad).is_err(), "case {which}");
        }
        let mut wrong = t;
        wrong.prepared_request = 0;
        assert!(enroll(&h, wrong, &out).is_err());
    }
    #[test]
    fn different_namespace_ids_bind_only_through_retained_task_command() {
        let (h, ticket, out) = populated(false, 0);
        let (mut tasks, real_ticket) = custody(&out, true);
        assert_eq!(ticket, real_ticket);
        assert_ne!(out.raw.command.task >> 32, 71);
        assert_ne!(out.raw.command.task as u32, owner().thread.as_raw() as u32);
        let binding = tasks.collected_binding(owner(), ticket, 2).unwrap();
        assert_eq!(binding.process(), 71);
        let a = h.enrollment(&binding).unwrap();
        tasks.complete(owner(), a).unwrap();
        assert_eq!(tasks.initial_association(owner()).unwrap().table(), 1);

        let other_thread = DetTid::from_raw(72);
        let other = NetworkStreamOwner {
            thread: other_thread,
            mm: MmId::initial(other_thread),
        };
        tasks.register(other, 72, 72, || Ok(6)).unwrap();
        tasks.begin_initial(other).unwrap();
        assert!(tasks.collected_binding(other, ticket, 2).is_err());
        let changed_mm = NetworkStreamOwner {
            mm: owner().mm.for_exec(owner().thread),
            ..owner()
        };
        assert!(tasks.collected_binding(changed_mm, ticket, 2).is_err());
        for which in 0..3 {
            let mut wrong = ticket;
            match which {
                0 => wrong.registration += 1,
                1 => wrong.prepared_request += 1,
                2 => wrong.command += 1,
                _ => unreachable!(),
            }
            assert!(tasks.collected_binding(owner(), wrong, 2).is_err());
        }
        assert!(tasks.collected_binding(owner(), ticket, 3).is_err());
    }
    #[test]
    fn original_register_failure_cannot_be_replaced_by_collector_success() {
        let (h, _, out) = populated(true, 0);
        let mut tasks = CustodyTasks::default();
        tasks.register(owner(), 71, 71, || Ok(5)).unwrap();
        let id = tasks.begin_initial(owner()).unwrap();
        assert_eq!(id, 1);
        tasks.retain_preparation(owner(), Ok(1)).unwrap();
        let t = tasks.prepared(owner(), 1, 41).unwrap();
        let mut out = out;
        out.raw.enrollment.registration = id;
        tasks.native_read(owner(), t, false).unwrap();
        tasks.retain_collection(owner(), Ok(2)).unwrap();
        tasks.retain_raw(owner(), Ok(out.clone())).unwrap();
        let binding = tasks.collected_binding(owner(), t, 2).unwrap();
        let a = h.enrollment(&binding).unwrap();
        assert!(tasks.complete(owner(), a).is_err());
        assert!(tasks.initial_association(owner()).is_err());
        assert!(tasks.enrollments_settled());
        assert!(tasks.native_read(owner(), t, true).is_err());
    }
    #[test]
    fn canceled_initial_command_survives_forget_without_new_identity() {
        let mut tasks = CustodyTasks::default();
        tasks.register(owner(), 71, 71, || Ok(5)).unwrap();
        let id = tasks.begin_initial(owner()).unwrap();
        assert_eq!(tasks.begin_initial(owner()).unwrap(), id);
        tasks.retain_preparation(owner(), Ok(7)).unwrap();
        assert!(!tasks.enrollments_settled());
        tasks.forget(owner());
        assert!(tasks.get(owner()).is_err());
        assert!(!tasks.enrollments_settled());
        assert!(
            tasks
                .register(owner(), 71, 71, || panic!("must not reopen"))
                .is_err()
        );
        let changed = NetworkStreamOwner {
            mm: owner().mm.for_exec(owner().thread),
            ..owner()
        };
        assert!(
            tasks
                .register(changed, 71, 71, || panic!(
                    "must not replace pending enrollment"
                ))
                .is_err()
        );
    }
}

#[cfg(test)]
mod epoll_history_tests {
    use super::*;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    fn retain(history: &mut History, sequence: u64, kind: u64, file: u64, dependency: u64) {
        history
            .retain(
                ffi::FdStatus {
                    next_event: 6,
                    next_file: 37,
                    next_table: 5,
                    ..Default::default()
                }
                .into(),
                ffi::FdEvent {
                    sequence,
                    kind,
                    task: 61,
                    task_start: 99,
                    table: 5,
                    file,
                    fd: 7,
                    dependency,
                    complete: 1,
                    ..Default::default()
                }
                .into(),
            )
            .unwrap();
    }
    #[test]
    fn original_epoll_history_requires_complete_origin_prefix_and_keeps_removed_files() {
        let mut h = History::default();
        assert!(h.unique_selection_origin(1, 5, 7, 31).is_err());
        retain(&mut h, 1, 1, 31, 0);
        assert!(h.unique_selection_origin(2, 5, 7, 31).is_err());
        retain(&mut h, 2, 2, 31, 1);
        retain(&mut h, 3, 3, 31, 0);
        assert_eq!(h.unique_selection_origin(3, 5, 7, 31).unwrap().sequence, 1);
        for (table, fd, file) in [(6, 7, 31), (5, 8, 31), (5, 7, 37)] {
            assert!(h.unique_selection_origin(3, table, fd, file).is_err());
        }
        retain(&mut h, 4, 1, 37, 0);
        retain(&mut h, 5, 2, 37, 4);
        retain(&mut h, 6, 3, 37, 0);
        assert_eq!(h.unique_selection_origin(6, 5, 7, 31).unwrap().sequence, 1);
        assert_eq!(h.unique_selection_origin(6, 5, 7, 37).unwrap().sequence, 4);
        assert!(h.unique_selection_origin(3, 5, 7, 37).is_err());
    }
    #[test]
    fn original_epoll_history_refuses_same_file_reinstallation_even_if_later_removed() {
        let mut h = History::default();
        retain(&mut h, 1, 1, 31, 0);
        retain(&mut h, 2, 2, 31, 1);
        retain(&mut h, 3, 3, 31, 0);
        retain(&mut h, 4, 1, 31, 0);
        retain(&mut h, 5, 2, 31, 4);
        retain(&mut h, 6, 3, 31, 0);
        assert_eq!(h.unique_selection_origin(3, 5, 7, 31).unwrap().sequence, 1);
        assert!(h.unique_selection_origin(6, 5, 7, 31).is_err());
        // The query never prunes an inconvenient origin or current-slot effect.
        assert_eq!(h.next().unwrap(), 7);
        assert!(matches!(
            h.transition(6).unwrap(),
            Some(Transition::Remove { .. })
        ));
    }
}
