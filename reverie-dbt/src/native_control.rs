/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Fixed native executor ABI used inside the existing borrowed RPC frame.

use std::sync::OnceLock;

use reverie_rpc_transport::native_control::CaptureHandle;
use reverie_rpc_transport::native_control::CaptureReceipt;
use reverie_rpc_transport::native_control::NativeControlOperation;
use reverie_rpc_transport::native_control::NativeControlReply;
use reverie_rpc_transport::native_control::NativeOwner;
use reverie_rpc_transport::native_control::PublicationHandle;
use reverie_rpc_transport::native_control::PublicationReceipt;
use reverie_rpc_transport::native_control::PublicationRequest;

/// C-compatible request. Operation 0 describes the current owner; 1–4 are the
/// four bounded SDK operations. Operation 5 is the private terminal failure
/// path if local cancellation cannot complete. Operations 6–10 observe a domain
/// or arm/close/release/cancel one publication interval. No arbitrary syscall
/// or callback is accepted.
#[derive(Default)]
#[repr(C)]
pub struct NativeControlRequest {
    /// Fixed operation number (0–5 owner/capture, 6–10 domain/publication).
    pub operation: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Expected current native callback owner; zero only for Describe.
    pub owner: NativeOwner,
    /// Caller epoch for Begin.
    pub owner_epoch: u64,
    /// Value handle for Close/Cancel.
    pub handle: CaptureHandle,
    /// Value receipt for Release.
    pub receipt: CaptureReceipt,
    /// Authenticated parent callback, child and reservation for PublicationArm.
    pub publication_request: PublicationRequest,
    /// Exact interval handle for PublicationClose or PublicationCancel.
    pub publication_handle: PublicationHandle,
    /// Exact current-domain receipt consumed by PublicationRelease.
    pub publication_receipt: PublicationReceipt,
}

/// Native-only executor. The pointers are borrowed for this synchronous call;
/// it neither enters Tool nor accesses the Rust Runtime or RPC mutex.
pub type NativeControlInvoker =
    unsafe extern "C" fn(usize, *const NativeControlRequest, *mut NativeControlReply);

static INVOKER: OnceLock<NativeControlInvoker> = OnceLock::new();

/// Registers the admitted native callback once during image initialization.
/// Matching repeated initialization is harmless; changing the executor refuses.
pub fn register(invoker: Option<NativeControlInvoker>) -> Result<(), &'static str> {
    let Some(invoker) = invoker else {
        return Err("native control executor absent");
    };
    match INVOKER.set(invoker) {
        Ok(()) => Ok(()),
        Err(_)
            if INVOKER
                .get()
                .is_some_and(|old| std::ptr::fn_addr_eq(*old, invoker)) =>
        {
            Ok(())
        }
        Err(_) => Err("native control executor changed within an image"),
    }
}

pub(crate) fn describe(context: usize) -> std::io::Result<NativeControlReply> {
    invoke(context, NativeControlRequest::default())
}

pub(crate) fn execute(
    context: usize,
    owner: NativeOwner,
    operation: NativeControlOperation,
) -> std::io::Result<NativeControlReply> {
    let mut request = NativeControlRequest {
        owner,
        ..Default::default()
    };
    match operation {
        NativeControlOperation::Begin { owner_epoch } => {
            request.operation = 1;
            request.owner_epoch = owner_epoch;
        }
        NativeControlOperation::Close { handle } => {
            request.operation = 2;
            request.handle = handle;
        }
        NativeControlOperation::Release { receipt } => {
            request.operation = 3;
            request.receipt = receipt;
        }
        NativeControlOperation::Cancel { handle } => {
            request.operation = 4;
            request.handle = handle;
        }
        NativeControlOperation::DomainObserve => request.operation = 6,
        NativeControlOperation::PublicationArm {
            request: publication,
        } => {
            request.operation = 7;
            request.publication_request = publication;
        }
        NativeControlOperation::PublicationClose { handle } => {
            request.operation = 8;
            request.publication_handle = handle;
        }
        NativeControlOperation::PublicationRelease { receipt } => {
            request.operation = 9;
            request.publication_receipt = receipt;
        }
        NativeControlOperation::PublicationCancel { handle } => {
            request.operation = 10;
            request.publication_handle = handle;
        }
    }
    invoke(context, request)
}

pub(crate) fn terminate(context: usize, owner: NativeOwner) {
    let _ = invoke(
        context,
        NativeControlRequest {
            operation: 5,
            owner,
            ..Default::default()
        },
    );
    // The native tree-failure endpoint must not return. No guessed success if
    // the callback ABI is broken and it unexpectedly does.
    std::process::abort();
}

fn invoke(context: usize, request: NativeControlRequest) -> std::io::Result<NativeControlReply> {
    let invoker = INVOKER
        .get()
        .ok_or_else(|| std::io::Error::other("native control was not initialized"))?;
    let mut reply = NativeControlReply::default();
    unsafe { invoker(context, &request, &mut reply) };
    Ok(reply)
}
