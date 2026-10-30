/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;

use detcore::preemptions::PreemptionReader;
use detcore_model::collections::ReplayCursor;
use detcore_model::schedule::Op;
use detcore_model::schedule::SchedEvent;
use detcore_model::time::LogicalTime;

fn split_one(event: SchedEvent) -> Vec<SchedEvent> {
    let first_count = event.count / 2;
    let second_count = event.count - first_count;

    let first_end_time = event.end_time.map(|t| {
        LogicalTime::from_nanos((t.as_nanos() as i64 - 10i64 * second_count as i64) as u64)
    });

    vec![
        SchedEvent {
            count: first_count,
            op: Op::Branch,
            end_time: first_end_time,
            start_rip: None,
            end_rip: None,
            ..event
        },
        SchedEvent {
            count: second_count,
            op: Op::Branch,
            start_rip: None,
            end_rip: None,
            ..event
        },
    ]
}

fn split_branches(
    event: SchedEvent,
    cursor: &ReplayCursor<SchedEvent>,
) -> impl IntoIterator<Item = SchedEvent> + use<> {
    match (&event, cursor.peek_nth(0), cursor.peek_nth(1)) {
        (
            SchedEvent {
                op: Op::Branch,
                count,
                ..
            },
            Some(SchedEvent {
                op: Op::OtherInstructions,
                ..
            }),
            Some(SchedEvent { op: Op::Branch, .. }),
        )
        | (
            SchedEvent {
                op: Op::Branch,
                count,
                ..
            },
            Some(SchedEvent { op: Op::Branch, .. }),
            _,
        ) if *count > 1 => split_one(event),
        _ => vec![event],
    }
}

pub fn split_branches_in_file(from_file: impl AsRef<Path>, backup: bool) -> anyhow::Result<()> {
    let mut preemptions = PreemptionReader::new(from_file.as_ref()).into_inner();
    preemptions.split_map(split_branches);

    if backup {
        let mut new_name = from_file.as_ref().to_path_buf();
        new_name.set_extension("bak");
        std::fs::rename(&from_file, &new_name)?;
    }

    preemptions
        .write_to_disk(from_file.as_ref())
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use detcore::types::DetPid;
    use detcore::types::LogicalTime;
    use detcore::types::Op;
    use detcore::types::SchedEvent;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    pub fn test_split_one() {
        let event = SchedEvent {
            count: 3,
            dettid: DetPid::from_raw(3),
            end_time: Some(LogicalTime::from_nanos(10000)),
            op: Op::Branch,
            start_rip: None,
            end_rip: None,
            strex_fails: Default::default(),
        };

        let counts: Vec<_> = split_one(event)
            .into_iter()
            .map(|x| (x.op, x.count, x.end_time.unwrap().as_nanos()))
            .collect();

        assert_eq!(counts, vec![(Op::Branch, 1, 9980), (Op::Branch, 2, 10000)]);
    }

    /// Splitting a branch segment cannot tell which half failed its
    /// store-exclusives, so the split record keeps neither the count nor the
    /// flag that every segment was counted.
    #[test]
    fn splitting_branches_keeps_no_store_exclusive_count() {
        let tid = DetPid::from_raw(3);
        let mut failing = SchedEvent::branches(tid, 4);
        failing.strex_fails = detcore_model::schedule::HostObserved(2);
        let mut writer = detcore::preemptions::PreemptionWriter::new(None);
        writer.insert_schedevent(failing);
        writer.insert_schedevent(SchedEvent::branches(tid, 6));
        writer.record_segment_store_exclusives(true);
        let mut record: detcore::preemptions::PreemptionRecord =
            serde_json::from_str(&writer.into_string()).unwrap();
        assert!(record.segment_store_exclusives());
        record.split_map(split_branches);
        let counts: Vec<_> = record
            .schedevents()
            .iter()
            .map(|event| (event.count, event.strex_fails.0))
            .collect();
        assert_eq!(counts, vec![(2, 0), (2, 0), (6, 0)]);
        assert!(!record.segment_store_exclusives());
    }
}
