//! The credential files a chat bridge's provider plugin and outbound helper are pointed at.
//!
//! A deployment can pass the plugin and the helper the path of a TLS client certificate and
//! key through their configured environment (`subscription_environment` and
//! `outbound_command.environment`). When that file expires or is removed, every subscription
//! attempt and every send fails inside the plugin or helper with whatever authorization error it
//! reports, which says nothing about the file. This module checks each such file the bridge
//! itself can see: whether it exists and can be read, and, when it holds a PEM certificate, that
//! certificate's notAfter. It reads a file only to find a `CERTIFICATE` block and never reports
//! anything from it but the notAfter; a private key in the same file is not parsed.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A credential is reported in `delivery-alarm.json` from this long before its certificate's
/// notAfter, so the alarm comes a day before the outage rather than after it.
pub(crate) const CREDENTIAL_EXPIRY_WARNING: Duration = Duration::from_secs(24 * 60 * 60);
// A certificate file is far smaller; a larger one is read no further than this.
const MAX_CREDENTIAL_FILE_BYTES: u64 = 1 << 20;

/// One credential file named by the plugin or helper environment, as `chat status` reports it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialFile {
    /// The environment variable that names the file. Its value, the path, is never reported:
    /// the bridge keeps the plugin and helper environment's values out of its state and status.
    pub(crate) variable: String,
    /// `present`, `missing`, `unreadable` (a regular file that cannot be read) or
    /// `inaccessible` (the path cannot be examined, so it is not known to be a file).
    pub(crate) state: String,
    /// The notAfter of the first PEM certificate in the file, in milliseconds since the Unix
    /// epoch (negative before 1970); absent when the file holds no certificate this module can
    /// read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) not_after_millis: Option<i64>,
}

/// What is wrong with a credential file at a given time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CredentialProblemKind {
    Missing,
    Unreadable,
    /// The path cannot be examined, so whether it is a credential file at all is unknown.
    Inaccessible,
    Expired,
    /// Valid now, but within [`CREDENTIAL_EXPIRY_WARNING`] of its notAfter.
    Expiring,
}

impl CredentialProblemKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
            Self::Inaccessible => "inaccessible",
            Self::Expired => "expired",
            Self::Expiring => "expiring",
        }
    }

    /// Whether the problem can explain a provider failure: an expiring certificate still works,
    /// and a path that cannot be examined is not known to be a credential file.
    pub(crate) fn explains_failure(self) -> bool {
        !matches!(self, Self::Expiring | Self::Inaccessible)
    }
}

/// `now_millis` as a signed time, saturating at the far future.
fn signed(now_millis: u64) -> i128 {
    i128::from(now_millis)
}

impl CredentialFile {
    /// What is wrong with this file at `now_millis`, if anything.
    pub(crate) fn problem(&self, now_millis: u64) -> Option<CredentialProblemKind> {
        let now = signed(now_millis);
        match self.state.as_str() {
            "missing" => Some(CredentialProblemKind::Missing),
            "present" => match self.not_after_millis.map(i128::from) {
                Some(not_after) if not_after <= now => Some(CredentialProblemKind::Expired),
                Some(not_after)
                    if not_after - now <= CREDENTIAL_EXPIRY_WARNING.as_millis() as i128 =>
                {
                    Some(CredentialProblemKind::Expiring)
                }
                _ => None,
            },
            "inaccessible" => Some(CredentialProblemKind::Inaccessible),
            _ => Some(CredentialProblemKind::Unreadable),
        }
    }

    /// One line naming the problem at `now_millis`, such as `credential file missing:
    /// SOME_TLS_CERT_PATH` or `credential expired 12 h ago: SOME_TLS_CERT_PATH`.
    pub(crate) fn describe(&self, now_millis: u64) -> Option<String> {
        let now = signed(now_millis);
        let not_after = i128::from(self.not_after_millis.unwrap_or_default());
        let hours = |millis: i128| millis / 3_600_000;
        let variable = &self.variable;
        Some(match self.problem(now_millis)? {
            CredentialProblemKind::Missing => format!("credential file missing: {variable}"),
            CredentialProblemKind::Unreadable => format!("credential file unreadable: {variable}"),
            CredentialProblemKind::Inaccessible => {
                format!("credential path cannot be examined: {variable}")
            }
            CredentialProblemKind::Expired => {
                format!(
                    "credential expired {} h ago: {variable}",
                    hours(now - not_after)
                )
            }
            CredentialProblemKind::Expiring => {
                format!(
                    "credential expires in {} h: {variable}",
                    hours(not_after - now)
                )
            }
        })
    }
}

/// A credential file with a problem, as `delivery-alarm.json` lists it. It holds no ages, so the
/// alarm changes only when a problem starts, changes kind or ends.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialProblem {
    pub(crate) variable: String,
    /// `missing`, `unreadable`, `expired` or `expiring`.
    pub(crate) problem: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) not_after_millis: Option<i64>,
}

/// Each file among `files` with a problem at `now_millis`, in order.
pub(crate) fn problems(files: &[CredentialFile], now_millis: u64) -> Vec<CredentialProblem> {
    files
        .iter()
        .filter_map(|file| {
            file.problem(now_millis).map(|kind| CredentialProblem {
                variable: file.variable.clone(),
                problem: kind.name().to_owned(),
                not_after_millis: file.not_after_millis,
            })
        })
        .collect()
}

/// The first problem among `files` at `now_millis` that can explain a provider failure, as one
/// line: see [`CredentialFile::describe`].
pub(crate) fn failure_explanation(files: &[CredentialFile], now_millis: u64) -> Option<String> {
    files
        .iter()
        .find(|file| {
            file.problem(now_millis)
                .is_some_and(CredentialProblemKind::explains_failure)
        })
        .and_then(|file| file.describe(now_millis))
}

/// Inspect the file each variable in `names` names, looking each value up with `lookup`. A
/// variable that is unset, whose value is not an absolute path, or whose path is not a regular
/// file (a directory such as a home, a device, a pipe) names no credential file and is skipped; a
/// variable listed twice is inspected once.
pub(crate) fn inspect<'a>(
    names: impl IntoIterator<Item = &'a String>,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Vec<CredentialFile> {
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            continue;
        }
        let Some(value) = lookup(name) else { continue };
        let path = Path::new(&value);
        if !path.is_absolute() {
            continue;
        }
        let (state, not_after_millis) = match read_bounded(path) {
            FileRead::Contents(contents) => {
                ("present", first_certificate_not_after_millis(&contents))
            }
            FileRead::NotAFile => continue,
            FileRead::Missing => ("missing", None),
            FileRead::Unreadable => ("unreadable", None),
            FileRead::Inaccessible => ("inaccessible", None),
        };
        files.push(CredentialFile {
            variable: name.clone(),
            state: state.to_owned(),
            not_after_millis,
        });
    }
    files
}

/// The process environment, as [`inspect`] reads it in production.
pub(crate) fn process_environment(name: &str) -> Option<OsString> {
    #[cfg(test)]
    if let Some(value) = test_environment::lookup(name) {
        return value;
    }
    std::env::var_os(name)
}

/// What reading a credential path found.
enum FileRead {
    /// A regular file, read up to [`MAX_CREDENTIAL_FILE_BYTES`].
    Contents(Vec<u8>),
    /// The path exists and is not a regular file.
    NotAFile,
    Missing,
    /// A regular file that cannot be read.
    Unreadable,
    /// Neither the open nor a look at the path's type worked, as under a directory this process
    /// cannot search, so it is not known to be a file.
    Inaccessible,
}

/// Read at most [`MAX_CREDENTIAL_FILE_BYTES`] of `path`. The file is opened once, without
/// blocking, and checked through that descriptor, so a path swapped for a pipe or device between
/// a check and the open cannot make the read wait. When the open fails, the path's type decides:
/// a non-file is not a credential file, a regular file is unreadable, and an unknown type is
/// inaccessible.
fn read_bounded(path: &Path) -> FileRead {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return FileRead::Missing,
        Err(_) => {
            return match fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => FileRead::Unreadable,
                Ok(_) => FileRead::NotAFile,
                Err(error) if error.kind() == io::ErrorKind::NotFound => FileRead::Missing,
                Err(_) => FileRead::Inaccessible,
            }
        }
    };
    match file.metadata() {
        Ok(metadata) if !metadata.is_file() => return FileRead::NotAFile,
        Ok(_) => {}
        // Opened, but its type is unknown: not confirmed to be a credential file.
        Err(_) => return FileRead::Inaccessible,
    }
    let mut contents = Vec::new();
    match file
        .take(MAX_CREDENTIAL_FILE_BYTES)
        .read_to_end(&mut contents)
    {
        Ok(_) => FileRead::Contents(contents),
        Err(_) => FileRead::Unreadable,
    }
}

fn first_certificate_not_after_millis(contents: &[u8]) -> Option<i64> {
    let der = pem_certificates(contents).into_iter().next()?;
    certificate_not_after_seconds(&der)?.checked_mul(1_000)
}

/// The DER bytes of each PEM `CERTIFICATE` block in `contents`, in order.
pub(crate) fn pem_certificates(contents: &[u8]) -> Vec<Vec<u8>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let text = String::from_utf8_lossy(contents);
    let mut certificates = Vec::new();
    let mut rest = text.as_ref();
    while let Some(start) = rest.find(BEGIN) {
        let after = &rest[start + BEGIN.len()..];
        let Some(end) = after.find(END) else { break };
        if let Some(der) = base64_decode(&after[..end]) {
            certificates.push(der);
        }
        rest = &after[end + END.len()..];
    }
    certificates
}

/// Standard base64 with padding, ignoring whitespace; `None` for any other character.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0_u32;
    let mut bits = 0_u32;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(output)
}

/// The notAfter of the X.509 certificate `der`, in seconds since the Unix epoch.
///
/// Certificate ::= SEQUENCE { tbsCertificate, ... }; tbsCertificate ::= SEQUENCE { [0] version
/// OPTIONAL, serialNumber INTEGER, signature AlgorithmIdentifier, issuer Name, validity
/// SEQUENCE { notBefore Time, notAfter Time }, ... } (RFC 5280, section 4.1).
pub(crate) fn certificate_not_after_seconds(der: &[u8]) -> Option<i64> {
    let (tag, certificate, _) = der_element(der)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, tbs, _) = der_element(certificate)?;
    if tag != 0x30 {
        return None;
    }
    let mut rest = tbs;
    let (tag, _, after) = der_element(rest)?;
    if tag == 0xa0 {
        rest = after;
    }
    // serialNumber, signature, issuer.
    for expected in [0x02, 0x30, 0x30] {
        let (tag, _, after) = der_element(rest)?;
        if tag != expected {
            return None;
        }
        rest = after;
    }
    let (tag, validity, _) = der_element(rest)?;
    if tag != 0x30 {
        return None;
    }
    let (_, _, after_not_before) = der_element(validity)?;
    let (tag, not_after, _) = der_element(after_not_before)?;
    parse_time(tag, not_after)
}

/// One DER element at the start of `input`: (tag, contents, the input after it).
fn der_element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let length = rest[..count]
            .iter()
            .fold(0_usize, |total, &byte| (total << 8) | usize::from(byte));
        (length, &rest[count..])
    };
    if rest.len() < length {
        return None;
    }
    Some((tag, &rest[..length], &rest[length..]))
}

/// A UTCTime (`YYMMDDHHMMSSZ`, years 1950 to 2049) or GeneralizedTime (`YYYYMMDDHHMMSSZ`).
fn parse_time(tag: u8, value: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(value).ok()?;
    let digits = text.strip_suffix('Z')?;
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| digits.get(range)?.parse::<i64>().ok();
    let (year, rest) = match (tag, digits.len()) {
        (0x17, 12) => {
            let short = number(0..2)?;
            (
                if short >= 50 {
                    1900 + short
                } else {
                    2000 + short
                },
                2,
            )
        }
        (0x18, 14) => (number(0..4)?, 4),
        _ => return None,
    };
    let month = number(rest..rest + 2)?;
    let day = number(rest + 2..rest + 4)?;
    let hour = number(rest + 4..rest + 6)?;
    let minute = number(rest + 6..rest + 8)?;
    let second = number(rest + 8..rest + 10)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// A per-thread override of [`process_environment`], so a test sets the values the bridge sees
/// without changing the process environment that other tests share.
#[cfg(test)]
pub(crate) mod test_environment {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    thread_local! {
        static VALUES: RefCell<Option<BTreeMap<String, Option<OsString>>>> = const { RefCell::new(None) };
    }

    /// Make `name` read as `value` (or unset) on this thread.
    pub(crate) fn set(name: &str, value: Option<&std::path::Path>) {
        VALUES.with(|values| {
            values
                .borrow_mut()
                .get_or_insert_with(BTreeMap::new)
                .insert(
                    name.to_owned(),
                    value.map(|path| path.as_os_str().to_owned()),
                );
        });
    }

    pub(super) fn lookup(name: &str) -> Option<Option<OsString>> {
        VALUES.with(|values| values.borrow().as_ref()?.get(name).cloned())
    }
}

/// Self-signed certificates generated for the tests with openssl; nothing else.
#[cfg(test)]
pub(crate) mod fixtures {
    pub(crate) const UTC_TIME_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----
MIIBgTCCASegAwIBAgIUXAe+ubr/ZkN0eFfi66cy4ZI471wwCgYIKoZIzj0EAwIw
FjEUMBIGA1UEAwwLZml4dHVyZS11dGMwHhcNMjYxMDAyMjIzOTAwWhcNMjYxMDA1
MjIzOTAwWjAWMRQwEgYDVQQDDAtmaXh0dXJlLXV0YzBZMBMGByqGSM49AgEGCCqG
SM49AwEHA0IABBTN/+1L4tdQrlKFbE/d7lMRi0E5I79sjaqlcb60gq3TTqEwL8Gd
sh+4Ynne8vy1vyh3qw680LAD4dla4MuMg4ajUzBRMB0GA1UdDgQWBBSSuwHblRfy
V0B4EghWIGfVYzJYDDAfBgNVHSMEGDAWgBSSuwHblRfyV0B4EghWIGfVYzJYDDAP
BgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0gAMEUCIA4Ez0dcwZVEDWle2y3o
Wmxp2Voh63iiahh5dOg++YG6AiEAvij5l/2uJpy29NF7fYSVUIC4wvWpMWQWa21V
6NOXx4Q=
-----END CERTIFICATE-----
";
    // notAfter 2026-10-05T22:39:00Z, encoded as UTCTime.
    pub(crate) const UTC_TIME_NOT_AFTER: i64 = 1_791_239_940;
    pub(crate) const GENERALIZED_TIME_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----
MIIBkzCCATmgAwIBAgIUbChpNHJEoAqtu6Vp7XOhv04toPgwCgYIKoZIzj0EAwIw
HjEcMBoGA1UEAwwTZml4dHVyZS1nZW5lcmFsaXplZDAgFw0yNjEwMDIwMDAwMDBa
GA8yMDYxMDEwMTAwMDAwMFowHjEcMBoGA1UEAwwTZml4dHVyZS1nZW5lcmFsaXpl
ZDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABGojucz3P0dIfYltWRoqnxTCKGXt
9GqIBjEcio8/vkXFJH6By7ejrYbNsUqPki+vE+bAuuwV8Kr8pmTlI7PeU3ujUzBR
MB0GA1UdDgQWBBSRvvxl+aBuL0t6Ykn5gokQ/JJhIjAfBgNVHSMEGDAWgBSRvvxl
+aBuL0t6Ykn5gokQ/JJhIjAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0gA
MEUCIQDqokz9TaJ16QYZJKA8eJFZaq0aS7yoHte7ujorqoGXowIgHeYoqdCQuvdc
M2iX2vXNlRqk5HpRaseewxoOKNXF1CE=
-----END CERTIFICATE-----
";
    // notAfter 2061-01-01T00:00:00Z, encoded as GeneralizedTime.
    pub(crate) const GENERALIZED_TIME_NOT_AFTER: i64 = 2_871_763_200;
    pub(crate) const BEFORE_1970_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----
MIIBgzCCASmgAwIBAgIUScnGqW+7pSnYBEEeh/CJ3bHKCC0wCgYIKoZIzj0EAwIw
FzEVMBMGA1UEAwwMZml4dHVyZS0xOTY5MB4XDTY4MDEwMTAwMDAwMFoXDTY5MDEw
MTAwMDAwMFowFzEVMBMGA1UEAwwMZml4dHVyZS0xOTY5MFkwEwYHKoZIzj0CAQYI
KoZIzj0DAQcDQgAE1K0yzqrJAoux//rRPIoEuautcH5fhGSFTkbNG+XE620EfLKw
OQT/UThynST4IdIvfdNgTwzqpkBTHo8tOTDi3qNTMFEwHQYDVR0OBBYEFMYhNrop
krfM5cwHrfde9YeiAXhEMB8GA1UdIwQYMBaAFMYhNropkrfM5cwHrfde9YeiAXhE
MA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIhAN6WtFJAR/6YGB35
J9M5GzF7TgoFfvWgPR4ki1rYLFDBAiBdVJ3UQpO2/G+kMnCvqtzc3MrKZL/IWHh+
7yMKuh3pVg==
-----END CERTIFICATE-----
";
    // notAfter 1969-01-01T00:00:00Z, before the Unix epoch, encoded as UTCTime.
    pub(crate) const BEFORE_1970_NOT_AFTER: i64 = -365 * 86_400;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use super::fixtures::*;

    fn temporary(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agentctl-credentials-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("create temporary directory");
        path
    }

    #[test]
    fn reads_not_after_in_both_der_time_encodings() {
        let utc = pem_certificates(UTC_TIME_CERTIFICATE.as_bytes());
        let generalized = pem_certificates(GENERALIZED_TIME_CERTIFICATE.as_bytes());
        assert_eq!(utc.len(), 1);
        assert_eq!(
            certificate_not_after_seconds(&utc[0]),
            Some(UTC_TIME_NOT_AFTER)
        );
        assert_eq!(
            certificate_not_after_seconds(&generalized[0]),
            Some(GENERALIZED_TIME_NOT_AFTER)
        );
    }

    #[test]
    fn a_key_before_the_certificate_and_damaged_input_are_handled() {
        let mut combined =
            String::from("-----BEGIN PRIVATE KEY-----\nnot parsed\n-----END PRIVATE KEY-----\n");
        combined.push_str(UTC_TIME_CERTIFICATE);
        assert_eq!(
            first_certificate_not_after_millis(combined.as_bytes()),
            Some(UTC_TIME_NOT_AFTER * 1_000)
        );
        assert_eq!(
            first_certificate_not_after_millis(b"no certificate here"),
            None
        );
        let truncated = &pem_certificates(UTC_TIME_CERTIFICATE.as_bytes())[0][..40];
        assert_eq!(certificate_not_after_seconds(truncated), None);
        assert_eq!(certificate_not_after_seconds(&[]), None);
    }

    #[test]
    fn inspect_reports_present_missing_and_skipped_variables() {
        let directory = temporary("inspect");
        let certificate = directory.join("cert.pem");
        fs::write(&certificate, UTC_TIME_CERTIFICATE).expect("write certificate");
        let missing = directory.join("gone.pem");
        let names = [
            "CERT".to_owned(),
            "GONE".to_owned(),
            "RELATIVE".to_owned(),
            "UNSET".to_owned(),
            "CERT".to_owned(),
            "DIRECTORY".to_owned(),
        ];
        let lookup = |name: &str| -> Option<OsString> {
            match name {
                "CERT" => Some(certificate.clone().into_os_string()),
                "GONE" => Some(missing.clone().into_os_string()),
                "RELATIVE" => Some(OsString::from("cert.pem")),
                "DIRECTORY" => Some(directory.clone().into_os_string()),
                _ => None,
            }
        };
        let files = inspect(&names, lookup);
        fs::remove_dir_all(&directory).expect("cleanup");
        let states: Vec<_> = files
            .iter()
            .map(|file| {
                (
                    file.variable.as_str(),
                    file.state.as_str(),
                    file.not_after_millis,
                )
            })
            .collect();
        assert_eq!(
            states,
            [
                ("CERT", "present", Some(UTC_TIME_NOT_AFTER * 1_000)),
                ("GONE", "missing", None),
            ]
        );
    }

    #[test]
    fn problems_and_their_descriptions_follow_the_clock() {
        let not_after = UTC_TIME_NOT_AFTER as u64 * 1_000;
        let file = CredentialFile {
            variable: "SOME_TLS_CERT_PATH".to_owned(),
            state: "present".to_owned(),
            not_after_millis: Some(UTC_TIME_NOT_AFTER * 1_000),
        };
        let hour = 3_600_000;
        assert_eq!(file.problem(not_after - 25 * hour), None);
        assert_eq!(
            file.problem(not_after - 24 * hour),
            Some(CredentialProblemKind::Expiring)
        );
        assert_eq!(
            file.describe(not_after - 3 * hour).as_deref(),
            Some("credential expires in 3 h: SOME_TLS_CERT_PATH")
        );
        assert_eq!(
            file.problem(not_after),
            Some(CredentialProblemKind::Expired)
        );
        assert_eq!(
            file.describe(not_after + 12 * hour).as_deref(),
            Some("credential expired 12 h ago: SOME_TLS_CERT_PATH")
        );
        // Only a problem that stops the credential working explains a failure.
        assert_eq!(
            failure_explanation(std::slice::from_ref(&file), not_after - hour),
            None
        );
        let missing = CredentialFile {
            state: "missing".to_owned(),
            not_after_millis: None,
            ..file.clone()
        };
        assert_eq!(
            failure_explanation(&[file, missing], 0).as_deref(),
            Some("credential file missing: SOME_TLS_CERT_PATH")
        );
    }

    #[test]
    fn an_expiry_before_1970_is_an_expired_credential() {
        let directory = temporary("before-1970");
        let certificate = directory.join("cert.pem");
        fs::write(&certificate, BEFORE_1970_CERTIFICATE).expect("write certificate");
        let names = ["OLD_CERT".to_owned()];
        let files = inspect(&names, |_| Some(certificate.clone().into_os_string()));
        fs::remove_dir_all(&directory).expect("cleanup");
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].not_after_millis,
            Some(BEFORE_1970_NOT_AFTER * 1_000)
        );
        assert_eq!(files[0].problem(0), Some(CredentialProblemKind::Expired));
        assert_eq!(
            files[0].describe(0).as_deref(),
            Some("credential expired 8760 h ago: OLD_CERT")
        );
    }

    #[test]
    fn an_inaccessible_directory_or_a_socket_is_not_a_credential_file() {
        let directory = temporary("not-files");
        let locked = directory.join("locked");
        fs::create_dir(&locked).expect("create directory");
        fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .expect("lock directory");
        let socket = directory.join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");
        let names = ["LOCKED".to_owned(), "SOCKET".to_owned()];
        let files = inspect(&names, |name| {
            Some(match name {
                "LOCKED" => locked.clone().into_os_string(),
                _ => socket.clone().into_os_string(),
            })
        });
        fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("unlock directory");
        fs::remove_dir_all(&directory).expect("cleanup");
        assert!(files.is_empty(), "{files:?}");
    }

    #[test]
    fn a_path_under_an_unsearchable_directory_is_inaccessible_and_explains_no_failure() {
        let directory = temporary("unsearchable");
        let parent = directory.join("parent");
        fs::create_dir(&parent).expect("create parent");
        let file = parent.join("cert.pem");
        fs::write(&file, UTC_TIME_CERTIFICATE).expect("write certificate");
        fs::set_permissions(&parent, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .expect("lock parent");
        let names = ["HIDDEN".to_owned()];
        let files = inspect(&names, |_| Some(file.clone().into_os_string()));
        fs::set_permissions(&parent, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("unlock parent");
        fs::remove_dir_all(&directory).expect("cleanup");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].state, "inaccessible");
        assert_eq!(
            files[0].problem(0),
            Some(CredentialProblemKind::Inaccessible)
        );
        assert_eq!(
            files[0].describe(0).as_deref(),
            Some("credential path cannot be examined: HIDDEN")
        );
        // Not known to be a credential file, so it does not stand in for a provider error.
        assert_eq!(failure_explanation(&files, 0), None);
        assert_eq!(problems(&files, 0)[0].problem, "inaccessible");
    }

    #[test]
    fn a_regular_file_that_cannot_be_read_is_unreadable() {
        let directory = temporary("unreadable");
        let file = directory.join("cert.pem");
        fs::write(&file, UTC_TIME_CERTIFICATE).expect("write certificate");
        fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .expect("lock file");
        let names = ["LOCKED".to_owned()];
        let files = inspect(&names, |_| Some(file.clone().into_os_string()));
        fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("unlock file");
        fs::remove_dir_all(&directory).expect("cleanup");
        assert_eq!(files[0].state, "unreadable");
        assert_eq!(
            failure_explanation(&files, 0).as_deref(),
            Some("credential file unreadable: LOCKED")
        );
    }

    #[test]
    fn a_pipe_where_a_credential_file_was_is_skipped_without_blocking() {
        let directory = temporary("fifo");
        let fifo = directory.join("cert.pem");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: `path` is a valid NUL-terminated path; mkfifo only creates the node.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0, "mkfifo");
        let names = ["FIFO".to_owned()];
        let started = std::time::Instant::now();
        let files = inspect(&names, |_| Some(fifo.clone().into_os_string()));
        let waited = started.elapsed();
        fs::remove_dir_all(&directory).expect("cleanup");
        assert!(files.is_empty(), "{files:?}");
        assert!(waited < std::time::Duration::from_secs(5), "{waited:?}");
    }

    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(
            days_from_civil(2061, 1, 1) * 86_400,
            GENERALIZED_TIME_NOT_AFTER
        );
    }
}
