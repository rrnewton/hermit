//! The existing native diagnostic and protected tracing channel, in the
//! single Detcore DSO. The C owner supplies its real sealed output callbacks.

use std::io;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

use detcore::GlobalState;
use reverie::GlobalTool;
use tracing::Event;
use tracing::field::{Field, Visit};

pub(super) type Emitter = unsafe extern "C" fn(*const u8, usize);
static DIAGNOSTIC: OnceLock<Emitter> = OnceLock::new();
static EVIDENCE: OnceLock<Emitter> = OnceLock::new();
static SINK: OnceLock<(Emitter, bool)> = OnceLock::new();
static LEVEL: AtomicI32 = AtomicI32::new(0);
const CANONICAL_PREFIX: &str = "1970-01-01T00:00:00.000000Z ";

pub(super) fn global_call(
    request: <GlobalState as GlobalTool>::Request,
) -> io::Result<<GlobalState as GlobalTool>::Response> {
    // This enters only the already selected common native application or
    // background scope. It supplies neither a fallback FD nor a new socket.
    reverie_liteinst::blocking_global_rpc::<GlobalState>(request)
}

pub(super) fn emit(message: &[u8]) {
    if let Some(&emit) = DIAGNOSTIC.get() {
        unsafe { emit(message.as_ptr(), message.len()); }
    }
}

pub(super) fn info_enabled() -> bool {
    matches!(LEVEL.load(Ordering::Acquire), 3..=5)
}

fn level(code: i32) -> Option<tracing::Level> {
    match code {
        1 => Some(tracing::Level::ERROR),
        2 => Some(tracing::Level::WARN),
        3 => Some(tracing::Level::INFO),
        4 => Some(tracing::Level::DEBUG),
        5 => Some(tracing::Level::TRACE),
        _ => None,
    }
}

pub(super) fn install(
    diagnostic: Emitter, evidence: Emitter, protected_level: i32, coordinated: bool,
) -> io::Result<bool> {
    DIAGNOSTIC.set(diagnostic).map_err(|_| io::Error::from_raw_os_error(libc::EALREADY))?;
    if !(0..=5).contains(&protected_level) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let effective = if protected_level != 0 {
        protected_level
    } else {
        match std::env::var("HERMIT_LOG").unwrap_or_default().to_ascii_lowercase().as_str() {
            "error" => 1, "warn" => 2, "info" => 3, "debug" => 4, "trace" => 5, _ => 0,
        }
    };
    LEVEL.store(effective, Ordering::Release);
    let canonical = protected_level != 0;
    let own_level = level(effective).or_else(|| (!canonical && !coordinated).then_some(tracing::Level::WARN));
    let own = own_level.map(|max_level| {
        let _ = SINK.set((if canonical { evidence } else { diagnostic }, canonical));
        detcore::guest_log::OwnChannel { max_level, write: write_event }
    });
    let installed = detcore::guest_log::install(global_call, own);
    if !installed {
        emit(b"native engine: WARNING Detcore's in-guest subscriber was not installed: the process already has one\n");
        if canonical {
            return Err(io::Error::other("protected evidence subscriber installation failed"));
        }
    }
    if canonical {
        let _ = EVIDENCE.set(evidence);
    }
    Ok(installed)
}

fn escaped(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            _ => output.push(character),
        }
    }
}

#[derive(Default)]
struct Fields(String);

impl Fields {
    fn push(&mut self, field: &Field, value: String) {
        if !self.0.is_empty() { self.0.push(' '); }
        if field.name() != "message" {
            self.0.push_str(field.name()); self.0.push('=');
        }
        escaped(&mut self.0, &value);
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field, format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_owned());
    }
}

fn write_event(event: &Event<'_>) {
    let Some(&(emit, canonical)) = SINK.get() else { return; };
    let mut fields = Fields::default();
    event.record(&mut fields);
    let metadata = event.metadata();
    let prefix = if canonical { CANONICAL_PREFIX } else { "" };
    let line = format!("{prefix}{} {}: {}\n", metadata.level().as_str(), metadata.target(), fields.0);
    unsafe { emit(line.as_ptr(), line.len()); }
}

pub(super) fn publish_determinism_loss() {
    let (Some(&emit), Some(loss)) = (EVIDENCE.get(), detcore::detlog::determinism_loss()) else { return; };
    let mut fields = String::new();
    escaped(&mut fields, &loss);
    let line = format!("{CANONICAL_PREFIX}ERROR {}: {fields}\n", detcore::detlog::DETERMINISM_LOSS_RECORD_TARGET);
    unsafe { emit(line.as_ptr(), line.len()); }
}
