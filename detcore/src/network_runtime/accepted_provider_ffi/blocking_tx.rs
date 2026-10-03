//! ABI10 has its own typed writer. Historical buffers never reach that symbol.
use std::ffi::c_int;
use std::io;

use super::OriginalBlockingSendCapture;
use super::SessionPtr;
use crate::network_runtime::ProviderWireFormat;

pub(super) type Prepare = unsafe extern "C" fn(
    SessionPtr,
    c_int,
    u64,
    u64,
    c_int,
    u64,
    u64,
    c_int,
    u64,
    *mut u64,
) -> c_int;
pub(super) type Capture =
    unsafe extern "C" fn(SessionPtr, u64, *mut OriginalBlockingSendCapture) -> c_int;
pub(super) struct Api {
    pub prepare: Prepare,
    pub capture: Capture,
}
fn read_into(
    wire: ProviderWireFormat,
    raw: &mut OriginalBlockingSendCapture,
    call: impl FnOnce(*mut OriginalBlockingSendCapture) -> c_int,
) -> io::Result<()> {
    if !wire.has_blocking_tx() {
        return Err(io::Error::other("blocking TX requires authenticated ABI10"));
    }
    *raw = OriginalBlockingSendCapture::default();
    if call(std::ptr::from_mut(raw)) != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub(super) fn read(
    wire: ProviderWireFormat,
    call: impl FnOnce(*mut OriginalBlockingSendCapture) -> c_int,
) -> io::Result<OriginalBlockingSendCapture> {
    let mut raw = OriginalBlockingSendCapture::default();
    read_into(wire, &mut raw, call)?;
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::super::LoadError;
    use super::super::authenticate_provider_declarations;
    use super::*;
    use crate::network_runtime::ProviderTopology;

    #[test]
    fn blocking_tx_buffer_is_exact_zeroed_and_historical_writers_are_unreachable() {
        #[repr(C)]
        struct Guarded {
            before: u64,
            raw: OriginalBlockingSendCapture,
            after: u64,
        }
        for wire in [
            ProviderWireFormat::Abi10Copy4,
            ProviderWireFormat::Abi10Copy5,
        ] {
            let mut value = Guarded {
                before: 0xfeed,
                raw: OriginalBlockingSendCapture::default(),
                after: 0xbeef,
            };
            value.raw.bytes.fill(0xff);
            read_into(wire, &mut value.raw, |raw| {
                // Controlled C writer premise; this is the actual typed read helper.
                let raw = unsafe { &mut *raw };
                assert_eq!(*raw, OriginalBlockingSendCapture::default());
                raw.provider = 3;
                raw.summary = [2, 7, 512, 512, 0, 512, 512, 1, 5000];
                raw.bytes.fill(0x61);
                0
            })
            .unwrap();
            assert_eq!((value.before, value.after), (0xfeed, 0xbeef));
            assert_eq!(value.raw.bytes, [0x61; 512]);
            assert_eq!(value.raw.summary[8], 5000);
            let err = read(wire, |_| {
                unsafe {
                    *libc::__errno_location() = libc::EIO;
                }
                -1
            })
            .unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::EIO));
        }
        for wire in [
            ProviderWireFormat::Abi7Copy4,
            ProviderWireFormat::Abi8Copy5,
            ProviderWireFormat::Abi9Copy4,
            ProviderWireFormat::Abi9Copy5,
        ] {
            assert!(read(wire, |_| panic!("legacy ABI reached632-byte writer")).is_err());
        }
        assert_eq!(
            std::mem::size_of::<super::super::OriginalSendCapture>(),
            624
        );
    }

    #[test]
    fn blocking_tx_requires_exact_command_declaration_before_any_operation() {
        for wire in [
            ProviderWireFormat::Abi10Copy4,
            ProviderWireFormat::Abi10Copy5,
        ] {
            for size in [
                None,
                Some(0),
                Some(64),
                Some(71),
                Some(72),
                Some(73),
                Some(80),
                Some(u64::MAX),
            ] {
                let mut names = Vec::new();
                let result = authenticate_provider_declarations(
                    wire,
                    &ProviderTopology::ClassicV40,
                    |name| {
                        names.push(name.to_owned());
                        match name.to_bytes() {
                            b"ap_adapter_abi_version" => Ok(wire.abi_version()),
                            b"ap_adapter_copy_version" => Ok(wire.copy_version()),
                            b"ap_adapter_task_command_size" => {
                                size.ok_or_else(|| LoadError("missing size".into()))
                            }
                            b"ap_provider_topology_version" => Ok(0),
                            _ => panic!("operation resolved before declarations"),
                        }
                    },
                );
                assert_eq!(result.is_ok(), size == Some(72));
                assert_eq!(
                    names.last().unwrap().to_bytes(),
                    if size == Some(72) {
                        b"ap_provider_topology_version".as_slice()
                    } else {
                        b"ap_adapter_task_command_size".as_slice()
                    }
                );
            }
        }
    }
}
