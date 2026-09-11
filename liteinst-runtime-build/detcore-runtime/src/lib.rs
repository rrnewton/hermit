use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use detcore::Detcore;

mod startup;

#[cfg(feature = "private-crt")]
mod private_crt;

use startup::Failure;

#[path = "../../../hermit-cli/src/liteinst_bootstrap.rs"]
pub mod liteinst_bootstrap;
#[path = "../../../hermit-cli/src/liteinst_record.rs"]
mod liteinst_record;

const BOOTSTRAP_SELECTOR: &str = "hermit-detcore-liteinst-v1";

fn validate_payload(payload: &[u8]) -> Result<liteinst_bootstrap::EffectiveFilter, String> {
    liteinst_bootstrap::decode(payload, &detcore::config_wire_fingerprint())
}

trait CompleteRecords {
    fn record_failed(&mut self) -> io::Result<()>;
    fn write_record(&mut self, record: &[u8]) -> io::Result<()>;
}

impl CompleteRecords for reverie_liteinst::GuestLogWriter {
    fn record_failed(&mut self) -> io::Result<()> {
        reverie_liteinst::GuestLogWriter::record_failed(self)
    }
    fn write_record(&mut self, record: &[u8]) -> io::Result<()> {
        reverie_liteinst::GuestLogWriter::write_record(self, record).map(|_| ())
    }
}

struct GuestRecords<Writer: CompleteRecords> {
    writer: Mutex<Writer>,
    failed: AtomicBool,
}

impl<Writer: CompleteRecords> GuestRecords<Writer> {
    fn record_failed(&self) {
        self.failed.store(true, Ordering::Release);
        match self.writer.try_lock() {
            Ok(mut writer) => {
                let _ = writer.record_failed();
            }
            Err(std::sync::TryLockError::Poisoned(error)) => {
                let _ = error.into_inner().record_failed();
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
    }

    fn write_record(&self, record: &[u8]) -> io::Result<()> {
        let writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("guest record writer poisoned"))?;
        struct Publication<'owner, Writer: CompleteRecords> {
            owner: &'owner GuestRecords<Writer>,
            writer: Option<std::sync::MutexGuard<'owner, Writer>>,
        }
        impl<Writer: CompleteRecords> Drop for Publication<'_, Writer> {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    self.owner.failed.store(true, Ordering::Release);
                }
                drop(self.writer.take());
                if self.owner.failed.load(Ordering::Acquire) {
                    self.owner.record_failed();
                }
            }
        }
        let mut publication = Publication {
            owner: self,
            writer: Some(writer),
        };
        if self.failed.load(Ordering::Acquire) {
            return Err(io::Error::other("guest record publication already failed"));
        }
        publication.writer.as_mut().unwrap().write_record(record)
    }
}

#[cfg(all(not(test), not(feature = "private-crt")))]
reverie_liteinst::clocked_initializer!(HERMIT_LITEINST_INIT, initialize);

#[cfg(all(not(test), feature = "private-crt"))]
use private_crt::HERMIT_LITEINST_INIT;

#[cfg(not(test))]
#[repr(C)]
struct RuntimeDescriptor {
    magic: [u8; 8],
    version: u32,
    size: u32,
    mode: u32,
    reserved: u32,
    constructor: unsafe extern "C" fn(),
}

#[cfg(not(test))]
#[used]
#[unsafe(export_name = "hermit_liteinst_detcore_descriptor_v1")]
static DESCRIPTOR: RuntimeDescriptor = RuntimeDescriptor {
    magic: *b"HLI_DSO1",
    version: 1,
    size: 32,
    mode: 1,
    reserved: 0,
    constructor: HERMIT_LITEINST_INIT,
};

#[cfg(any(test, not(feature = "private-crt")))]
unsafe extern "C" fn initialize() -> i32 {
    unsafe { startup::finish_initializer(initialize_selected()) }
}

unsafe fn initialize_selected() -> Result<i32, Failure> {
    let selection = std::env::var_os("HERMIT_LITEINST_DETCORE_BOOTSTRAP");
    if !startup::selected(selection.as_deref())? {
        return Ok(0);
    }
    let bootstrap = unsafe { reverie_liteinst::take_preload_bootstrap() }
        .map_err(Failure::Bootstrap)?
        .ok_or(Failure::MissingBootstrap)?;
    let filter = validate_payload(&bootstrap.tool_data).map_err(Failure::Payload)?;
    {
        use tracing_subscriber::util::SubscriberInitExt;
        let log = bootstrap.log.ok_or(Failure::MissingLog)?;
        let writer = unsafe { log.install() }.map_err(Failure::Transport)?;
        let records = Arc::new(GuestRecords {
            writer: Mutex::new(writer),
            failed: AtomicBool::new(false),
        });
        let failed = records.clone();
        let limits = liteinst_record::FormatterLimits::new(
            liteinst_record::EVENT_BYTES,
            liteinst_record::SPAN_BYTES,
        )
        .map_err(Failure::Formatter)?;
        let (subscriber, status) = liteinst_record::record_subscriber_with_failure(
            filter,
            false,
            limits,
            move |record| records.write_record(record),
            move || failed.record_failed(),
        );
        subscriber.try_init().map_err(Failure::Subscriber)?;
        if let Some(error) = status.failure() {
            return Err(Failure::Record(error));
        }
    }
    unsafe {
        reverie_liteinst::install_tool_owned_native_from_bootstrap::<Detcore>(
            &bootstrap.coordinator,
            0,
        )
    }
    .map_err(Failure::Installation)?;
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Records {
        failures: Arc<std::sync::atomic::AtomicUsize>,
        during_write: Box<dyn Fn() + Send>,
    }
    impl CompleteRecords for Records {
        fn record_failed(&mut self) -> io::Result<()> {
            self.failures.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn write_record(&mut self, _: &[u8]) -> io::Result<()> {
            (self.during_write)();
            Ok(())
        }
    }

    #[test]
    fn record_failure_during_publication_is_not_lost_at_unlock() {
        let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let records = Arc::new_cyclic(|owner: &std::sync::Weak<GuestRecords<Records>>| {
            let owner = owner.clone();
            GuestRecords {
                writer: Mutex::new(Records {
                    failures: failures.clone(),
                    during_write: Box::new(move || owner.upgrade().unwrap().record_failed()),
                }),
                failed: AtomicBool::new(false),
            }
        });
        records.write_record(b"one complete record").unwrap();
        assert!(records.failed.load(Ordering::Acquire));
        assert!(failures.load(Ordering::SeqCst) > 0);
        assert!(records.write_record(b"refused").is_err());
    }

    #[test]
    fn record_unwind_reports_failure_after_releasing_writer() {
        let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let records = GuestRecords {
            writer: Mutex::new(Records {
                failures: failures.clone(),
                during_write: Box::new(|| panic!("publication panic")),
            }),
            failed: AtomicBool::new(false),
        };
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || records.write_record(b"record")
            ))
            .is_err()
        );
        assert!(records.failed.load(Ordering::Acquire));
        assert!(failures.load(Ordering::SeqCst) > 0);
        assert!(records.write_record(b"refused").is_err());
    }

    #[test]
    fn initializer_selection_contract() {
        const RESULT: &str = "HERMIT_LITEINST_TEST_INITIALIZER_RESULT";
        if let Ok(expected) = std::env::var(RESULT) {
            assert_eq!(unsafe { initialize() }, expected.parse::<i32>().unwrap());
            return;
        }
        for (selection, expected) in [
            (None, 0),
            (Some("wrong-tool"), 127),
            (Some(BOOTSTRAP_SELECTOR), 127),
        ] {
            let mut command = std::process::Command::new("/usr/bin/timeout");
            command
                .args(["--signal=KILL", "10"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::initializer_selection_contract",
                    "--nocapture",
                ])
                .env_remove("LD_PRELOAD")
                .env_remove("HERMIT_LITEINST_DETCORE_BOOTSTRAP")
                .env(RESULT, expected.to_string());
            if let Some(selection) = selection {
                command.env("HERMIT_LITEINST_DETCORE_BOOTSTRAP", selection);
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{selection:?}: {output:?}");
            if selection.is_none() {
                assert!(output.stderr.is_empty(), "{selection:?}: {output:?}");
            } else {
                assert!(
                    output
                        .stderr
                        .starts_with(b"hermit-liteinst startup failed: stage="),
                    "{selection:?}: {output:?}"
                );
                assert!(output.stderr.ends_with(b"\n"), "{selection:?}: {output:?}");
            }
        }
    }

    #[test]
    fn accepts_matching_tool_and_config_wire_schema() {
        let filter = liteinst_bootstrap::EffectiveFilter::from_directives_lossy(
            "",
            tracing::metadata::LevelFilter::INFO,
        );
        let payload =
            liteinst_bootstrap::encode(&detcore::config_wire_fingerprint(), &filter).unwrap();
        assert!(validate_payload(&payload).is_ok());
    }

    #[test]
    fn rejects_other_tool_identity_with_matching_schema() {
        let payload = serde_json::json!({"version":2,"tool":"other-tool","config_wire_fingerprint":detcore::config_wire_fingerprint(),"log_filter":"info"});
        assert_eq!(
            validate_payload(&serde_json::to_vec(&payload).unwrap()).unwrap_err(),
            "bootstrap tool mismatch"
        );
    }

    #[test]
    fn rejects_mismatched_schema() {
        let filter = liteinst_bootstrap::EffectiveFilter::from_directives_lossy(
            "",
            tracing::metadata::LevelFilter::INFO,
        );
        let payload = liteinst_bootstrap::encode("stale", &filter).unwrap();
        assert_eq!(
            validate_payload(&payload).unwrap_err(),
            "bootstrap config fingerprint mismatch"
        );
    }

    #[test]
    fn rejects_malformed_payloads() {
        for payload in [b"".as_slice(), BOOTSTRAP_SELECTOR.as_bytes(), b"\xff"] {
            assert!(validate_payload(payload).is_err());
        }
    }
}
