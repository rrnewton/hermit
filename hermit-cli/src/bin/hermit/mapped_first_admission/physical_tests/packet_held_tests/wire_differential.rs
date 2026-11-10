//! Hermit's handoff producer against the Reverie producer at one real stop.
//! Both build from the same package image, the same held child, the same
//! admission and the same packet descriptor number; the handoff bytes must be
//! equal. Never maps or enters the image; the child is cancelled at its stop.
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use reverie::GlobalTool;
use reverie_inguest::launcher::exec_stop::CleanupReceipt;
use reverie_inguest::launcher::exec_stop::CopiedInputRequest;
use reverie_inguest::launcher::exec_stop::ExecStop;
use reverie_inguest::launcher::exec_stop::PreparedExec;
use reverie_inguest::launcher::exec_stop::producer::ArmingFacts;
use reverie_inguest::launcher::exec_stop::producer::AuditedRuntimeImage;
use reverie_inguest::launcher::exec_stop::producer::BirthKind;
use reverie_inguest::launcher::exec_stop::producer::EntryPoint;
use reverie_inguest::launcher::exec_stop::producer::MappedBirth;
use reverie_inguest::launcher::exec_stop::producer::MappedBirthArming;
use reverie_inguest::launcher::exec_stop::register_words;
use sha2::Digest;

use super::super::super::runtime_support::Context;
use super::super::super::runtime_support::PAGE;
use super::super::super::runtime_support::PacketMappingWire;
use super::super::super::runtime_support::WireInputs;
use super::super::super::runtime_support::aligned;
use super::super::super::runtime_support::checked_image;
use super::super::super::runtime_support::producer_error;
use super::super::super::runtime_support::reserved_signals;
use super::super::MEMFD_NAMES;
use super::*;

fn hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The first offset where `a` and `b` differ, or the shorter length.
fn first_difference(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()))
}

async fn run() -> io::Result<()> {
    let retained = super::super::fixture_package()?;
    let observed = retained.observed();
    let (image, descriptor) = checked_image(retained.bytes(), observed).map_err(rpc)?;
    let placement = retained.placement()?;
    let audited = AuditedRuntimeImage::new(
        retained.bytes().to_vec(),
        observed.facts(EntryPoint {
            entry: retained.entry(),
            terminal_trap: retained.terminal_int3(),
        }),
    )
    .map_err(rpc)?;
    let directory = tempfile::tempdir()?;
    let directory_fd = std::fs::File::open(directory.path())?;
    let socket = std::path::PathBuf::from(format!(
        "/proc/{}/fd/{}/a.sock",
        std::process::id(),
        directory_fd.as_raw_fd()
    ));
    let config = detcore::Config {
        no_rcb_time: true,
        max_timeslice: None,
        target_timeslice: None,
        sequentialize_threads: false,
        in_guest_detlog_forward_policy: Some("INFO".to_owned()),
        ..Default::default()
    };
    need(
        packet::config_violation(&config).is_none(),
        "real Config outside ordinary domain",
    )?;
    let global = Arc::new(<detcore::GlobalState as GlobalTool>::init_global_state(&config).await);
    let birth = MappedBirth::new(&audited, MEMFD_NAMES).map_err(rpc)?;
    let lease = PendingPacketLease::new()?;
    let (sender, receiver) = UnixStream::pair()?;
    receiver.set_nonblocking(true)?;
    let mut receiver = tokio::net::UnixStream::from_std(receiver)?;
    let [packet_fd, runtime_fd, handoff_fd] = birth.inherited();
    let request = PreparedExec::new(
        std::path::Path::new("/bin/true"),
        &["mapped-wire-differential".into()],
        &[("ORIGINAL".into(), "wire-differential".into())],
        None,
        vec![
            packet_fd,
            lease.inherited_fd(),
            sender.as_fd(),
            runtime_fd,
            handoff_fd,
        ],
    )
    .map_err(rpc)?;
    let receipt = CleanupReceipt::default();
    let mut held = HeldExec {
        stop: None,
        launch_failure: None,
        retired_owner: None,
        receipt: receipt.clone(),
    };
    match ExecStop::spawn(request, receipt) {
        Ok(stop) => held.stop = Some(stop),
        Err(failure) => held.launch_failure = Some(failure),
    }
    let mut admissions = Vec::new();
    let observed_outcome: io::Result<()> = async {
        let stop = held.ready()?;
        let captured = stop.capture_initial_inputs().map_err(rpc)?;
        let admission = admit(&mut admissions, &global, &config, &socket, stop, &sender).await?;
        let sealed = lease.fill(stop, &admission)?;
        let packet = birth
            .fill_packet(stop, &*admission)
            .map_err(producer_error)?;
        need(
            sealed.bytes() == packet,
            "the two producers sealed different packets",
        )?;

        // Hermit's copied-input request, exactly as mapped_run computed it.
        let control = placement.roles[4]
            .start
            .checked_add(PAGE)
            .ok_or_else(|| rpc("control overflow"))?;
        let relay = aligned(
            control
                .checked_add(descriptor[6])
                .ok_or_else(|| rpc("relay overflow"))?,
            descriptor[9],
        )
        .map_err(rpc)?;
        let address = placement.roles[3]
            .start
            .checked_add(PAGE)
            .ok_or_else(|| rpc("handoff address"))?;
        let copied_address = address
            .checked_add(2 * PAGE)
            .ok_or_else(|| rpc("copied address"))?;
        let copied_capacity = usize::try_from(
            placement.roles[3]
                .end
                .checked_sub(PAGE)
                .and_then(|end| end.checked_sub(copied_address))
                .ok_or_else(|| rpc("copied capacity"))?,
        )
        .map_err(rpc)?;
        let hermit_request = CopiedInputRequest {
            roles: placement.roles,
            free_band: placement.free_band,
            layout_address: observed.layout_address,
            copied_address,
            copied_capacity,
            control_address: control,
            relay_address: relay,
            filter_scratch: placement.filter_scratch,
        };
        let producer_request = birth.copied_input_request().map_err(rpc)?;
        let same_request = |a: &CopiedInputRequest, b: &CopiedInputRequest| {
            a.roles == b.roles
                && a.free_band == b.free_band
                && a.layout_address == b.layout_address
                && a.copied_address == b.copied_address
                && a.copied_capacity == b.copied_capacity
                && a.control_address == b.control_address
                && a.relay_address == b.relay_address
                && a.filter_scratch == b.filter_scratch
        };
        need(
            same_request(&hermit_request, &producer_request),
            "the two producers computed different copied-input requests",
        )?;
        let hermit_copied = stop
            .prepare_copied_inputs(&captured, &image, &hermit_request)
            .map_err(rpc)?;
        let producer_copied = stop
            .prepare_copied_inputs(
                &captured,
                &birth.validated_image().map_err(rpc)?,
                &producer_request,
            )
            .map_err(rpc)?;
        // SAFETY: actual unchanged stopped XState on this host's stable domain.
        let pkru = unsafe { stop.entry().xstate().pkru() }.map_err(rpc)?;
        let mask = stop.entry().signal_mask();
        need(
            register_words(stop.entry().regs()) == *captured.registers(),
            "stop registers differ from the captured registers",
        )?;
        // Both producers are given the producer's packet descriptor number, the
        // one descriptor the mapped birth would use.
        let wire = PacketMappingWire::build(
            &image,
            WireInputs {
                runtime: retained.bytes(),
                observed,
                roles: &placement.roles,
                descriptor: &descriptor,
                reference: observed.reference_address,
                copied: &hermit_copied,
                pkru,
                free_band: placement.free_band,
                packet: &sealed,
            },
            Context {
                pid: stop.pid().as_raw() as u64,
                regs: register_words(stop.entry().regs()),
                mask,
                input_fd: birth.leases().packet().as_raw_fd() as u64,
            },
        )
        .map_err(rpc)?;
        let armed = birth
            .arm(&ArmingFacts {
                birth: BirthKind::Initial,
                captured: &captured,
                copied: &producer_copied,
                mask,
                pkru,
                reserved_signals: reserved_signals(&config),
            })
            .map_err(rpc)?;
        let produced = armed.preparation.handoff_bytes;
        eprintln!(
            "wire differential: hermit {} bytes sha256 {}; producer {} bytes sha256 {}; \
             first difference at {}",
            wire.bytes.len(),
            hex(&wire.bytes),
            produced.len(),
            hex(produced),
            first_difference(&wire.bytes, produced),
        );
        need(
            wire.bytes == produced,
            "the two producers built different handoff bytes",
        )?;
        need(
            wire.address == armed.preparation.handoff_address
                && observed.layout_address == armed.preparation.layout_address
                && placement.free_band == armed.preparation.free_band
                && placement.filter_scratch == armed.preparation.filter_scratch,
            "the two producers prepared different addresses",
        )
    }
    .await;

    let mut cleanup = Vec::new();
    if let Err(error) = held.cancel() {
        cleanup.push(format!("terminal wait: {error}"));
    }
    for slot in &mut admissions {
        if observed_outcome.is_err() {
            slot.owner.fail(&"wire differential failed");
        }
        let drain = slot.owner.exit_admission().drain(WAIT).await;
        if let Err(error) = &drain {
            cleanup.push(format!("watch drain: {error:?}"));
        }
        if drain.is_ok() {
            slot.drained = true;
        }
    }
    drop(held);
    drop(sender);
    let eof = bounded(async {
        let mut byte = [0];
        need(
            receiver.read(&mut byte).await? == 0,
            "actual inherited DETLOG OFD not at EOF",
        )
    })
    .await;
    if let Err(error) = eof {
        cleanup.push(format!("DETLOG EOF: {error}"));
    }
    drop(admissions);
    need(cleanup.is_empty(), "wire differential cleanup unconfirmed").map_err(|error| {
        rpc(format!(
            "{error}: {}; observation={observed_outcome:?}",
            cleanup.join("; ")
        ))
    })?;
    observed_outcome
}

#[tokio::test(flavor = "current_thread")]
async fn hermit_and_producer_build_the_same_handoff_at_one_real_stop() {
    run().await.unwrap();
}
