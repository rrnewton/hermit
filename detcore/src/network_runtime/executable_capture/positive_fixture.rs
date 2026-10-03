//! Explicit controlled packaged-C premise around real retained Unix transport.
//! This service never supplies source bytes or constructs a Reverie proof.
use std::os::fd::FromRawFd;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use super::*;
use crate::network_runtime::accepted_provider::CallStatus;
use crate::network_runtime::accepted_transport::AcceptedSession;
use crate::network_runtime::accepted_transport::Received;

pub(crate) struct Fixture {
    pub runtime: NetworkRuntimeResources,
    pub root: Arc<ForegroundRoot>,
    pub metadata: Arc<Mutex<crate::tool_local::FileMetadata>>,
    pub memory: Arc<Mutex<crate::memory::MemoryMetadata>>,
    pub claim: crate::network_runtime::InitialTableClaim,
    pub service: Service,
}

pub(crate) struct Service {
    controller: Arc<Controller>,
    worker: Option<std::thread::JoinHandle<()>>,
    phases: Arc<AtomicUsize>,
}
impl Service {
    pub(crate) fn joined(mut self) {
        self.worker.take().unwrap().join().unwrap();
        assert_eq!(self.phases.load(Ordering::Acquire), 3);
        assert!(self.controller.quiescent().unwrap());
    }
}
fn status(operation: &str) -> CallStatus {
    CallStatus {
        operation: operation.into(),
        returned: 0,
        errno: None,
    }
}

pub(crate) fn fixture(thread: i32) -> Fixture {
    let (mut runtime, root, metadata, memory, claim) =
        crate::network_runtime::controlled_foreground_runtime(thread);
    let mut pair = [-1; 2];
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                pair.as_mut_ptr(),
            )
        },
        0
    );
    let wire = crate::network_runtime::ProviderWireFormat::Abi11Copy5;
    let controller = Arc::new(
        Controller::from_startup(unsafe { OwnedFd::from_raw_fd(pair[0]) }, [7; 16], wire).unwrap(),
    );
    let mut provider =
        AcceptedSession::from_wire(unsafe { OwnedFd::from_raw_fd(pair[1]) }, [7; 16], wire)
            .unwrap();
    let shared = Arc::get_mut(&mut runtime.shared).unwrap();
    shared.copy_wire = Some(wire);
    *shared.controller.lock().unwrap() = Some(Ok(controller.clone()));
    let phases = Arc::new(AtomicUsize::new(0));
    let observed = phases.clone();
    let driver = controller.clone();
    let original = root.clone();
    let worker = std::thread::spawn(move || {
        // Outer fixture bound only; production ARM->ACK keeps its own unchanged
        // total one-second budget. No timeout grants guest progress.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut retained: Option<Intent> = None;
        let mut sequences = Vec::new();
        while observed.load(Ordering::Acquire) < 3 {
            assert!(
                Instant::now() < deadline,
                "controlled service did not finish"
            );
            driver.drive_once_retained(|| Ok(())).unwrap();
            if let Some(Received::Request(sequence)) = provider.try_receive().unwrap() {
                let (envelope, rights, _) = provider.retained_request(sequence).unwrap();
                let request = serde_json::from_slice::<Request>(&envelope.body).unwrap();
                let phase = observed.load(Ordering::Acquire);
                let reply = match (phase, request) {
                    (0, Request::PrepareExecutableSource { intent }) => {
                        assert!(intent.valid_unarmed());
                        assert_eq!(rights.len(), 1);
                        let target = runtime_target_identity(&original);
                        assert_eq!(PidfdIdentity::read(&rights[0]).unwrap(), target);
                        // These stable allocations are live in the actual R
                        // armer, before it performs native PRSTATUS GETREGSET.
                        let words = read_words::<2>(intent.iovec);
                        assert_eq!(words, [intent.registers, 216]);
                        let regs = read_words::<27>(intent.registers);
                        assert!(regs.iter().all(|word| *word == 0));
                        retained = Some(intent);
                        Reply::Prepared(Observation {
                            status: status("ap_prepare_executable_source"),
                            raw: 7,
                        })
                    }
                    (
                        1,
                        Request::CollectExecutableSource {
                            call,
                            command,
                            prepared_request,
                        },
                    ) => {
                        assert!(rights.is_empty());
                        let mut intent = retained.clone().unwrap();
                        assert_eq!(
                            (call, command, prepared_request),
                            (intent.call, 7, sequences[0])
                        );
                        intent.command = command;
                        let words = read_words::<2>(intent.iovec);
                        assert_eq!(words, [intent.registers, 216]);
                        let words = read_words::<27>(intent.registers);
                        // Owned complete native x86_64 PRSTATUS, copied by the
                        // kernel; no reference to the challenged allocation.
                        let regs: libc::user_regs_struct = unsafe { std::mem::transmute(words) };
                        assert_eq!(regs.orig_rax, libc::SYS_sendto as u64);
                        assert_eq!(
                            (regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9),
                            (
                                7,
                                intent.address,
                                intent.length,
                                libc::MSG_NOSIGNAL as u64,
                                0,
                                0
                            )
                        );
                        assert_ne!(regs.rip, 0);
                        Reply::ExecutableSource(collection(&original, intent))
                    }
                    (
                        2,
                        Request::RetireExecutableSource {
                            call,
                            prepared,
                            completed,
                        },
                    ) => {
                        assert!(rights.is_empty());
                        assert_eq!(
                            (call, prepared, completed),
                            (retained.as_ref().unwrap().call, sequences[0], sequences[1])
                        );
                        provider
                            .check_incoming_executable_source(
                                original.owner(),
                                call,
                                prepared,
                                completed,
                            )
                            .unwrap();
                        Reply::ExecutableSourceRetired(status("ap_ack_command"))
                    }
                    _ => panic!("unexpected actual executable request order"),
                };
                sequences.push(sequence);
                provider
                    .dispatch(sequence, |_, _| Ok(serde_json::to_vec(&reply).unwrap()))
                    .unwrap();
                if phase == 2 {
                    provider
                        .retire_incoming_executable_source(
                            original.owner(),
                            retained.as_ref().unwrap().call,
                            [sequences[0], sequences[1], sequences[2]],
                        )
                        .unwrap();
                }
                assert!(provider.try_reply(sequence).unwrap());
                driver.drive_once_retained(|| Ok(())).unwrap();
                observed.fetch_add(1, Ordering::Release);
            }
            std::thread::yield_now();
        }
    });
    Fixture {
        runtime,
        root,
        metadata,
        memory,
        claim,
        service: Service {
            controller,
            worker: Some(worker),
            phases,
        },
    }
}

fn read_words<const N: usize>(address: u64) -> [u64; N] {
    let mut words = [0u64; N];
    let length = std::mem::size_of_val(&words);
    let local = libc::iovec {
        iov_base: words.as_mut_ptr().cast(),
        iov_len: length,
    };
    let remote = libc::iovec {
        iov_base: address as usize as *mut libc::c_void,
        iov_len: length,
    };
    let copied = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
    assert_eq!(
        copied,
        length as isize,
        "challenge allocation read failed: {}",
        std::io::Error::last_os_error()
    );
    words
}

fn runtime_target_identity(root: &ForegroundRoot) -> PidfdIdentity {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, root.thread(), libc::O_EXCL) };
    assert!(fd >= 0);
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    PidfdIdentity::read(&fd).unwrap()
}

fn collection(root: &ForegroundRoot, intent: Intent) -> Observation<Effect> {
    let process = procfs::process::Process::new(root.thread()).unwrap();
    let maps = process.maps().unwrap();
    let map = maps
        .iter()
        .find(|map| {
            map.address.0 <= intent.address && intent.address + intent.length <= map.address.1
        })
        .unwrap();
    assert_ne!(map.inode, 0);
    let metadata = std::fs::metadata(format!("/proc/{}/exe", root.thread())).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(map.inode, metadata.ino());
    let (provider, task, start, _) = root.native_identity();
    let mut result = wire::controlled_collection(intent);
    result.raw.command.identity.provider = provider;
    result.raw.command.task = task;
    result.raw.command.start_boottime = start;
    let mut mapping = result.raw.receipt.entered.clone();
    mapping.task = task;
    mapping.start = start;
    // The C dispatcher, original mm/exe_file identity, denied-write count and
    // image authentication are CONTROLLED premises. Procfs geometry, registered
    // PIDFD, actual capture allocation and full frame above are real evidence.
    mapping.device = (map.dev.0 as u64) << 20 | map.dev.1 as u64;
    mapping.inode_number = map.inode;
    mapping.file_size = metadata.len();
    mapping.vm_start = map.address.0;
    mapping.vm_end = map.address.1;
    mapping.vm_pgoff = map.offset / 4096;
    result.raw.receipt.entered = mapping.clone();
    result.raw.receipt.returned = mapping;
    result
}
