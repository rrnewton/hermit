/* SPDX-License-Identifier: MIT */
//! Actual C producer -> actual copy5 Rust decoder. Kernel callbacks, selection,
//! saved frame, source memory and EXIT are host premises, not native evidence.
use std::fs::File;
use std::fs::{self};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use super::super::driver_ftrace_inputs;
use super::super::driver_ftrace_process;
use super::super::process_group;
use super::*;

const EXTRA: &[(&str, &[u8])] = &[
    ("stream-copy-record-export.c", include_bytes!("stream-copy-record-export.c")),
    ("stream-copy-fault-producer-test.c", include_bytes!("stream-copy-fault-producer-test.c")),
    ("stream-frontier.h", include_bytes!("stream-frontier.h")),
    ("stream-copy-custody.inc", include_bytes!("stream-copy-custody.inc")),
    ("stream-copy-problem.inc", include_bytes!("stream-copy-problem.inc")),
    ("stream-copy-unit-enter.inc", include_bytes!("stream-copy-unit-enter.inc")),
    ("stream-copy-emit.inc", include_bytes!("stream-copy-emit.inc")),
    ("stream-copy-grouped-source.inc", include_bytes!("stream-copy-grouped-source.inc")),
    ("stream-copy-fault.inc", include_bytes!("stream-copy-fault.inc")),
    ("stream-copy-unit-exit.inc", include_bytes!("stream-copy-unit-exit.inc")),
];
fn stage(command: &mut Command, start: Instant, out: &Path, err: &Path) {
    let receipt=driver_ftrace_process::execute_stage(command,start,out,err);
    eprintln!("actual producer bridge: {receipt}");
    assert_eq!(receipt["passed"],true, "{}", String::from_utf8_lossy(&fs::read(err).unwrap()));
}
fn compile(inputs: &Path, binary: &Path, start: Instant, out: &Path, err: &Path) {
    stage(Command::new("clang").args(["-std=gnu11","-O2","-Wall","-Wextra","-Werror",
        "-UNDEBUG"]).arg("-I").arg(inputs).arg(inputs.join("stream-copy-record-export.c"))
        .arg("-o").arg(binary),start,out,err);
}
fn decode_export(bytes: &[u8]) -> Capture {
    assert_eq!(std::mem::size_of::<RawRecord>(),584);
    assert_eq!(bytes.len()%584,0);
    assert!(bytes.len()<=64*584);
    let mut records: Vec<Record> = bytes
        .as_chunks::<584>()
        .0
        .iter()
        .map(|b| {
        // repr(C) fixed integer/byte fields, no invalid bit patterns.
        let raw=unsafe { std::ptr::read_unaligned(b.as_ptr().cast::<RawRecord>()) };
        Record::from(raw)
    })
        .collect();
    let commit=records.pop().unwrap();
    assert_eq!(commit.kind,2);
    assert_eq!(commit.length,64);
    let summary=unsafe {std::ptr::read_unaligned(commit.bytes.as_ptr().cast::<Summary>())};
    let selected=OriginalSelection {provider:commit.provider,command:commit.command,
        call:commit.call,task:commit.task,task_start:commit.task_start,
        file:23,table:19,ready:1,fdput_flags:0,original_count:summary.initial_count,
        owner_mm:0,user_address:0,requested_fd:0,address_length:0};
    let returned=commit.offset as i64;
    let mut raw=crate::network_runtime::accepted_provider_ffi::OriginalEffect::default();
    raw.command.operation=11;
    raw.command.command=selected.command;
    raw.command.returned=returned.try_into().unwrap();
    raw.command.phase=1;
    raw.command.identity.provider=selected.provider;
    raw.command.task=selected.task;
    raw.command.start_boottime=selected.task_start;
    raw.command.original_count=selected.original_count;
    raw.original.returned=returned.try_into().unwrap();
    raw.original.complete=1;
    let mut effect:OriginalEffect=raw.into();
    effect.original.selection=selected.clone();
    effect.read_copy=Some(Manifest {provider:commit.provider,command:commit.command,
        call:commit.call,task:commit.task,task_start:commit.task_start,present:1,returned,summary});
    let thread=crate::types::DetTid::from_raw(13);
    let owner=crate::network_replay::NetworkStreamOwner {
        thread,mm:crate::types::MmId::initial(thread)};
    let wire=crate::network_runtime::copy_wire_authority::controlled_copy_authority(
        crate::network_runtime::ProviderWireFormat::Abi8Copy5,owner,11,selected.clone()).unwrap();
    let mut decoder=StreamingPrefix::for_authority(wire).unwrap();
    decoder.advance(&selected,&records,Some(End::OriginalExit {protocol:true})).unwrap();
    decoder.collect(&effect,records).unwrap()
}
fn oracle(capture: &Capture, name: &str) {
    let prior=name=="prior";let full=name=="full";let two=name=="two";
    let copied=if full {512} else if prior {575} else if two {191} else {63};
    assert_eq!(capture.manifest.summary.copied,copied,"actual producer copied-byte oracle");
    assert_eq!(capture.units.len(),if prior {2} else {1});
    let failed=capture.units.last().unwrap();
    assert_eq!(failed.native.requested,512);
    assert_eq!(failed.native.copied,if full {512} else if two {191} else {63});
    assert_eq!(failed.native.returned,if full {0} else {-14});
    assert_eq!(capture.committed.len(),if full || prior {512} else {0});
    let observation=failed.observation.as_ref().unwrap();
    assert_eq!(observation.begin.before,if prior {512} else {0});
    assert_eq!(observation.after,if full || prior {512} else {0});
    let pattern=|index:usize| (index*17+31) as u8;
    let page0:Vec<u8>=(100..612).map(pattern).collect();
    let expected:Vec<u8>=if full {page0.clone()} else if prior {
        page0.iter().chain(&page0[..63]).copied().collect()
    } else if two {page0[..128].iter().copied().chain((4096+200..4096+263).map(pattern)).collect()}
    else {page0[..63].to_vec()};
    let observed:Vec<u8>=capture.records.iter().filter(|r|r.kind==DATA)
        .flat_map(|r|r.bytes[..r.length as usize].iter().copied()).collect();
    assert_eq!(observed,expected);
    if full || prior {assert_eq!(capture.committed,page0);}
    assert_eq!(capture.manifest.summary.final_count,if full {0} else {512});
}
fn retain_bytes(cohort: &str, name: &str, bytes: &[u8]) {
    // Durable exact emitted records in the ordinary test log; no source bytes
    // or semantic fields are synthesized by this rendering.
    let hex:String=bytes.iter().map(|byte|format!("{byte:02x}")).collect();
    eprintln!("ACTUAL_C_RECORDS cohort={cohort} case={name} hex={hex}");
}
fn corrupt_actual_records(bytes: &[u8]) {
    let end = bytes
        .as_chunks::<584>()
        .0
        .iter()
        .position(|r| {
            u32::from_ne_bytes(r[68..72].try_into().unwrap()) == frontier::FINISH
        })
        .unwrap();
    for mutation in ["missing-data","request","failed-frontier"] {
        let mut changed=bytes.to_vec();
        match mutation {
            "missing-data" => {
                let data=changed.as_chunks::<584>().0.iter().position(|r|
                    u32::from_ne_bytes(r[68..72].try_into().unwrap())==DATA).unwrap();
                changed.drain(data*584..(data+1)*584);
            }
            "request" => {
                changed[end*584+72+24..end*584+72+32].copy_from_slice(&63u64.to_ne_bytes());
            }
            _ => {
                changed[end*584+72+80..end*584+72+88].copy_from_slice(&63u64.to_ne_bytes());
            }
        }
        assert!(std::panic::catch_unwind(||decode_export(&changed)).is_err(),
            "actual C record mutation accepted: {mutation}");
        eprintln!("actual C-derived decoder mutation refused: {mutation}");
    }
    // The qualifying original must remain accepted after every independent corruption.
    oracle(&decode_export(bytes),"fault");
}
#[test]
fn actual_c_failed_prefix_reaches_existing_rust_decoder() {
    let start=Instant::now();
    let scratch=tempfile::Builder::new().prefix("hermit-copy-producer-").tempdir().unwrap();
    let inputs=scratch.path().join("sources");fs::create_dir(&inputs).unwrap();
    let source=Path::new(env!("CARGO_MANIFEST_DIR")).join("../hermit-cli/network-provider");
    for (name,bytes) in driver_ftrace_inputs::C_INPUTS.iter().chain(EXTRA) {
        assert_eq!(fs::read(source.join(name)).unwrap(),*bytes,"stale producer: {name}");
        fs::write(inputs.join(name),bytes).unwrap();
    }
    let out=scratch.path().join("stdout");let err=scratch.path().join("stderr");
    File::create(&out).unwrap();File::create(&err).unwrap();
    let fixed=scratch.path().join("fixed");
    compile(&inputs,&fixed,start,&out,&err);
    for name in ["full","fault","prior","two"] {
        let before=fs::metadata(&out).unwrap().len() as usize;
        stage(Command::new(&fixed).arg(name),start,&out,&err);
        let bytes=fs::read(&out).unwrap();
        retain_bytes("fixed",name,&bytes[before..]);
        let capture=decode_export(&bytes[before..]);
        oracle(&capture,name);
        if name=="fault" {corrupt_actual_records(&bytes[before..]);}
        eprintln!("actual C -> Rust {name}: copied={} committed={}",
            capture.manifest.summary.copied,capture.committed.len());
    }
    // Exact pre-916 production exit body, not a handwritten semantic record or
    // helper-supplied copied count. Only this historical preimage is replaced.
    fs::write(inputs.join("stream-copy-unit-exit.inc"),
        include_bytes!("stream-copy-unit-exit-before-916.inc")).unwrap();
    let old=scratch.path().join("before");
    compile(&inputs,&old,start,&out,&err);
    for name in ["full","fault"] {
        let before=fs::metadata(&out).unwrap().len() as usize;
        stage(Command::new(&old).arg(name),start,&out,&err);
        let bytes=fs::read(&out).unwrap();
        retain_bytes("before-916",name,&bytes[before..]);
        let capture=decode_export(&bytes[before..]);
        if name=="full" {oracle(&capture,name);}
        else {
            assert_eq!(capture.manifest.summary.copied,0);
            assert!(std::panic::catch_unwind(||oracle(&capture,name)).is_err(),
                "historical missing failed DATA must fail the identical oracle");
        }
    }
    assert_eq!(process_group::bounds(start,&out,&err,driver_ftrace_process::LIMITS).unwrap(),(false,false));
}
