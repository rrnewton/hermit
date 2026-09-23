// Bounded model used by the actual fork broker, with no child-side allocation.
const MAX_RELEASE_SLOTS: usize = 64;
const DEFAULT_RELEASE_SLOTS: usize = 32;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotPhase {
    Free,
    Active,
    Ready,
    Sent,
    Acked,
}
#[derive(Clone, Copy, Debug)]
struct BrokerSlot {
    phase: SlotPhase,
    id: u64,
    pid: i32,
    pidfd: i32,
    channel: i32,
    start_ticks: u64,
    sequence: u64,
    status: i32,
    no_child: bool,
}
impl BrokerSlot {
    const EMPTY: Self = Self {
        phase: SlotPhase::Free,
        id: 0,
        pid: -1,
        pidfd: -1,
        channel: -1,
        start_ticks: 0,
        sequence: 0,
        status: 0,
        no_child: false,
    };
}
struct BrokerCredits {
    slots: [BrokerSlot; MAX_RELEASE_SLOTS],
    capacity: usize,
    last_job: u64,
    last_completion: u64,
    last_ack: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreditError {
    InvalidCapacity,
    Full,
    Identity,
    Sequence,
    NotReaped,
    NotSent,
}
impl BrokerCredits {
    fn new(capacity: usize) -> Result<Self, CreditError> {
        if capacity == 0 || capacity > MAX_RELEASE_SLOTS {
            return Err(CreditError::InvalidCapacity);
        }
        Ok(Self {
            slots: [BrokerSlot::EMPTY; MAX_RELEASE_SLOTS],
            capacity,
            last_job: 0,
            last_completion: 0,
            last_ack: 0,
        })
    }
    fn reserve(&mut self, id: u64) -> Result<usize, CreditError> {
        if id == 0 || id <= self.last_job {
            return Err(CreditError::Identity);
        }
        let index = self.slots[..self.capacity]
            .iter()
            .position(|s| s.phase == SlotPhase::Free)
            .ok_or(CreditError::Full)?;
        self.last_job = id;
        self.slots[index] = BrokerSlot {
            id,
            phase: SlotPhase::Active,
            ..BrokerSlot::EMPTY
        };
        Ok(index)
    }
    fn terminal(&mut self, index: usize, status: i32, no_child: bool) -> Result<u64, CreditError> {
        let slot = self.slots.get_mut(index).ok_or(CreditError::Identity)?;
        if slot.phase != SlotPhase::Active {
            return Err(CreditError::NotReaped);
        }
        let sequence = self
            .last_completion
            .checked_add(1)
            .ok_or(CreditError::Sequence)?;
        self.last_completion = sequence;
        slot.sequence = sequence;
        slot.status = status;
        slot.no_child = no_child;
        slot.phase = SlotPhase::Ready;
        Ok(sequence)
    }
    fn next_unsent(&self) -> Option<usize> {
        self.slots[..self.capacity]
            .iter()
            .enumerate()
            .filter(|(_, s)| s.phase == SlotPhase::Ready)
            .min_by_key(|(_, s)| s.sequence)
            .map(|(i, _)| i)
    }
    fn sent(&mut self, index: usize) -> Result<(), CreditError> {
        if self.slots[index].phase != SlotPhase::Ready {
            return Err(CreditError::NotReaped);
        }
        self.slots[index].phase = SlotPhase::Sent;
        Ok(())
    }
    fn acknowledge(&mut self, prefix: u64) -> Result<u64, CreditError> {
        if prefix > self.last_completion {
            return Err(CreditError::Sequence);
        }
        if prefix <= self.last_ack {
            return Ok(0);
        }
        for sequence in self.last_ack + 1..=prefix {
            if !self.slots[..self.capacity]
                .iter()
                .any(|s| s.sequence == sequence && s.phase == SlotPhase::Sent)
            {
                return Err(CreditError::NotSent);
            }
        }
        let mut mask = 0u64;
        for (index, slot) in self.slots[..self.capacity].iter_mut().enumerate() {
            if slot.phase == SlotPhase::Sent && slot.sequence <= prefix {
                slot.phase = SlotPhase::Acked;
                mask |= 1 << index;
            }
        }
        self.last_ack = prefix;
        Ok(mask)
    }
    fn release_acked(&mut self, index: usize) -> Result<(), CreditError> {
        if self.slots[index].phase != SlotPhase::Acked {
            return Err(CreditError::NotSent);
        }
        self.slots[index] = BrokerSlot::EMPTY;
        Ok(())
    }
    fn occupied(&self) -> usize {
        self.slots[..self.capacity]
            .iter()
            .filter(|s| s.phase != SlotPhase::Free)
            .count()
    }
    fn active(&self) -> bool {
        self.slots[..self.capacity]
            .iter()
            .any(|s| s.phase == SlotPhase::Active)
    }
}
#[cfg(test)]
mod credit_model_tests {
    use super::*;
    #[test]
    fn actual_broker_model_retains_eagain_and_credits_until_exact_ack() {
        let mut state = BrokerCredits::new(2).unwrap();
        let a = state.reserve(1).unwrap();
        let b = state.reserve(2).unwrap();
        assert_eq!(state.reserve(3), Err(CreditError::Full));
        assert_eq!(state.terminal(b, 9, false).unwrap(), 1);
        assert_eq!(state.terminal(a, 0, false).unwrap(), 2);
        let first = state.next_unsent().unwrap();
        assert_eq!(first, b);
        // Real EAGAIN path makes no state mutation; repeated selection is exact.
        assert_eq!(state.next_unsent(), Some(first));
        assert_eq!(state.occupied(), 2);
        assert_eq!(state.acknowledge(1), Err(CreditError::NotSent));
        state.sent(b).unwrap();
        let mask = state.acknowledge(1).unwrap();
        assert_eq!(mask, 1 << b);
        assert_eq!(state.reserve(3), Err(CreditError::Full));
        state.release_acked(b).unwrap();
        assert_eq!(state.reserve(3).unwrap(), b);
        assert_eq!(state.slots[a].status, 0);
        assert_eq!(state.slots[a].phase, SlotPhase::Ready);
    }
    #[test]
    fn actual_broker_model_rejects_future_ack_without_mutation() {
        let mut state = BrokerCredits::new(2).unwrap();
        let a = state.reserve(7).unwrap();
        state.terminal(a, 125 << 8, false).unwrap();
        state.sent(a).unwrap();
        assert_eq!(state.acknowledge(2), Err(CreditError::Sequence));
        assert_eq!(state.last_ack, 0);
        assert_eq!(state.slots[a].phase, SlotPhase::Sent);
        assert_eq!(state.slots[a].status, 125 << 8);
        assert_eq!(state.acknowledge(1), Ok(1 << a));
        assert_eq!(state.acknowledge(1), Ok(0));
    }
    #[test]
    fn actual_broker_model_slot_bound_survives_many_completion_orders() {
        let mut state = BrokerCredits::new(4).unwrap();
        for round in 0..128u64 {
            let ids = [round * 4 + 1, round * 4 + 2, round * 4 + 3, round * 4 + 4];
            let mut slots = [0; 4];
            for i in 0..4 {
                slots[i] = state.reserve(ids[i]).unwrap();
                assert!(state.occupied() <= 4);
            }
            for i in [2, 0, 3, 1] {
                state.terminal(slots[i], i as i32, false).unwrap();
            }
            for _ in 0..4 {
                let index = state.next_unsent().unwrap();
                state.sent(index).unwrap();
            }
            let mask = state.acknowledge(state.last_completion).unwrap();
            for i in 0..4 {
                assert!(mask & (1 << i) != 0);
                state.release_acked(i).unwrap();
            }
            assert_eq!(state.occupied(), 0);
        }
        assert_eq!(state.last_ack, 512);
    }
    #[test]
    fn actual_broker_model_rejected_fork_also_requires_terminal_ack() {
        let mut state = BrokerCredits::new(1).unwrap();
        let a = state.reserve(1).unwrap();
        state.terminal(a, libc::EAGAIN, true).unwrap();
        assert_eq!(state.reserve(2), Err(CreditError::Full));
        assert!(state.slots[a].no_child);
        assert_eq!(state.slots[a].status, libc::EAGAIN);
        state.sent(a).unwrap();
        assert_eq!(state.acknowledge(1), Ok(1));
        state.release_acked(a).unwrap();
        assert_eq!(state.reserve(2), Ok(a));
    }
}
