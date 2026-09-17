//! Temporary coordinator-only diagnostic output. Never a guest stream or verdict.
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use detcore::getrandom_diagnostic as buffer;
use serde_json::Value;
use serde_json::json;

use crate::getrandom_diagnostic_reader::Object;

pub const DIRECTORY_ENV: &str = "HERMIT_GETRANDOM_DIAGNOSTIC_DIR";
const SYMBOL: &str = "HERMIT_GETRANDOM_DIAGNOSTIC_V1";
const MAX_EVENTS: usize = 128;
const MAX_CAPTURES: usize = 16;
const MAX_JSON: usize = 1024 * 1024;

pub struct Session {
    directory: PathBuf,
    events: Vec<Value>,
    captures: Vec<Value>,
    lost: u64,
    lineage_changed: bool,
    object: Option<Object>,
}

fn bounded_text(text: &str) -> String {
    text.chars().take(4096).collect()
}

impl Session {
    pub fn begin(backend: &str, plugin: Option<&Path>) -> Result<Option<Self>, Error> {
        let Some(directory) = std::env::var_os(DIRECTORY_ENV).map(PathBuf::from) else {
            return Ok(None);
        };
        if !directory.is_absolute() {
            return Err(anyhow!("getrandom diagnostic directory must be absolute"));
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .context("create fresh coordinator diagnostic directory")?;
        let mut session = Self {
            directory,
            events: Vec::new(),
            captures: Vec::new(),
            lost: 0,
            lineage_changed: false,
            object: None,
        };
        session.write_json("begin.json", &json!({"backend":backend,"pid":std::process::id(),"mount_namespace":fs::read_link("/proc/self/ns/mnt")?.to_string_lossy(),"max_captures":MAX_CAPTURES,"max_events":MAX_EVENTS,"raw_bytes_per_capture":buffer::IMAGE_BYTES,"max_json_bytes_per_file":MAX_JSON,"meaning":"diagnostic only; missing final.json is incomplete"}))?;
        if let Some(plugin) = plugin {
            match Object::open(plugin, SYMBOL) {
                Ok(object) => session.object = Some(object),
                Err(error) => {
                    session.write_json(
                        "prelaunch-refusal.json",
                        &json!({"error":bounded_text(&error)}),
                    )?;
                    return Err(anyhow!(error).context("bind diagnostic plugin before launch"));
                }
            }
        }
        Ok(Some(session))
    }

    fn new_file(&self, name: &str) -> Result<File, Error> {
        Ok(OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join(name))?)
    }

    fn write_json(&self, name: &str, value: &Value) -> Result<(), Error> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > MAX_JSON {
            return Err(anyhow!("diagnostic JSON exceeds fixed bound"));
        }
        self.new_file(name)?.write_all(&bytes)?;
        Ok(())
    }

    pub fn event(&mut self, value: Value) {
        if self.events.len() < MAX_EVENTS {
            self.events.push(value);
        } else {
            self.lost = self.lost.saturating_add(1);
        }
    }

    pub fn lineage(&mut self, value: Value, replaces_or_copies_mm: bool) {
        self.lineage_changed |= replaces_or_copies_mm;
        self.event(value);
    }

    pub fn exit_stop(
        &mut self,
        pid: i32,
        mm: Option<u64>,
        members: usize,
        all_known: bool,
    ) -> Result<(), Error> {
        self.event(json!({"event":"existing_exit_stop","pid":pid,"mm":mm,"registered_mm_members":members,"all_tracees_have_mm":all_known}));
        if mm.is_none() || members != 1 || !all_known {
            self.event(json!({"event":"snapshot_unavailable","pid":pid,"mm":mm,"reason":"exit stop does not prove exclusive remaining mm membership"}));
            return Ok(());
        }
        if self.captures.len() == MAX_CAPTURES {
            self.lost = self.lost.saturating_add(1);
            return Ok(());
        }
        let capture = (|| {
            let object = self.object.as_ref().ok_or("no held plugin object")?;
            let prepared = object.prepare(pid as u32, buffer::IMAGE_BYTES)?;
            let binding = prepared.description();
            if binding.len() > 24 * 1024 {
                return Err("diagnostic binding exceeds fixed bound".to_owned());
            }
            let image = prepared.finish(|read| buffer::capture(read))?;
            Ok((image, binding))
        })();
        let index = self.captures.len();
        match capture {
            Ok((image, binding)) => {
                self.new_file(&format!("mm-{index}.bin"))?
                    .write_all(&image)?;
                let records = buffer::decode(&image).map_err(|e| anyhow!(e))?;
                let words: Vec<_> = records.into_iter().map(|r| r.0.to_vec()).collect();
                let row = json!({"pid":pid,"mm":mm,"byte_capture_valid":true,"exclusive_mm_at_exit_stop":true,"binding":binding,"records":words});
                self.write_json(&format!("mm-{index}.json"), &row)?;
                self.captures
                    .push(json!({"index":index,"pid":pid,"mm":mm,"byte_capture_valid":true}));
            }
            Err(error) => {
                let row = json!({"index":index,"pid":pid,"mm":mm,"byte_capture_valid":false,"error":bounded_text(&error)});
                self.write_json(&format!("mm-{index}.json"), &row)?;
                self.captures.push(row);
            }
        }
        Ok(())
    }

    pub fn finish_sabre(&self) -> Result<(), Error> {
        self.write_json("final.json", &json!({"all_tracees_physically_exited":true,"complete_single_mm_history":self.lost==0 && !self.lineage_changed && self.captures.len()==1 && self.captures[0]["byte_capture_valid"]==true,"fork_or_exec_lineage":self.lineage_changed,"lost_metadata_or_capture_requests":self.lost,"captures":self.captures,"events":self.events,"meaning":"buffer completeness is not a guest-success or parity verdict"}))
    }

    pub fn finish_local<T>(self, result: Result<T, Error>) -> Result<T, Error> {
        // The ptrace backend has finished before this snapshot. If it returned
        // an error, retain bytes but make no completed-lifetime claim.
        let image = buffer::BUFFER.snapshot();
        self.new_file("coordinator.bin")?.write_all(&image)?;
        let decoded = buffer::decode(&image);
        self.write_json("final.json", &json!({"backend_returned_ok":result.is_ok(),"byte_capture_valid":decoded.is_ok(),"records":decoded.as_ref().ok().map(|records|records.iter().map(|r|r.0.to_vec()).collect::<Vec<_>>()),"error":decoded.err(),"meaning":"coordinator buffer only; backend error means lifetime completeness unavailable"}))?;
        result
    }
}
