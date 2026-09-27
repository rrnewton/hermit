use super::*;

fn plain(bytes: &[u8]) -> InboundOutcome {
    InboundOutcome::Stream {
        bytes: bytes.iter().copied().collect(),
        ancillary: None,
        message_flags: 0,
        requires_message_io: false,
    }
}
fn channel(fragments: &[&[u8]]) -> ChannelState {
    ChannelState {
        transport: NetworkTransportV2::Tcp,
        inbound_consumed: 0,
        receive_input_generation: None,
        inbound: fragments.iter().map(|b| plain(b)).collect(),
        explicit_readiness: NetworkReadinessV2::default(),
        transmitted: 0,
        outbound: VecDeque::new(),
        local_write_closed: false,
        local_read_shutdown: false,
        local_control_generation: 0,
        peer_write_closed: false,
        readiness: NetworkReadinessV2::default(),
        published_ingress: Some(PublishedIngress {
            stream_offset: fragments.iter().map(|b| b.len() as u64).sum(),
            ..PublishedIngress::default()
        }),
    }
}

#[test]
fn plans_multiple_published_fragments_then_only_selected_private_suffix() {
    let c = channel(&[b"ab", b"cde"]);
    let before = format!("{c:?}");
    let p = plan_prefix(&c, b"abcdefgh", 0).unwrap();
    assert_eq!(
        p.fragments,
        vec![
            Fragment {
                length: 2,
                whole: true
            },
            Fragment {
                length: 3,
                whole: true
            }
        ]
    );
    assert_eq!(p.private_suffix, 5..8);
    assert_eq!((p.consumed_after, p.published_after), (8, 8));
    assert_eq!(p.readiness_after, NetworkReadinessV2::default());
    assert_eq!(format!("{c:?}"), before);
}

#[test]
fn plans_all_private_all_published_and_selection_ending_inside_fragment() {
    let mut c = channel(&[]);
    c.published_ingress = None;
    assert_eq!(plan_prefix(&c, b"abc", 0).unwrap().private_suffix, 0..3);
    let c = channel(&[b"ab", b"cdefgh"]);
    let p = plan_prefix(&c, b"abcd", 0).unwrap();
    assert_eq!(
        p.fragments,
        vec![
            Fragment {
                length: 2,
                whole: true
            },
            Fragment {
                length: 2,
                whole: false
            }
        ]
    );
    assert_eq!(p.private_suffix, 4..4);
    assert_eq!((p.consumed_after, p.published_after), (4, 8));
    assert!(p.readiness_after.readable);
    let p = plan_prefix(&c, b"abcdefgh", 0).unwrap();
    assert_eq!(p.private_suffix, 8..8);
    assert!(!p.readiness_after.readable);
}

#[test]
fn later_fragment_mismatch_and_missing_published_bytes_refuse_without_mutation() {
    for c in [channel(&[b"ab", b"Xd"]), {
        let mut c = channel(&[b"ab"]);
        c.published_ingress.as_mut().unwrap().stream_offset = 4;
        c
    }] {
        let before = format!("{c:?}");
        assert!(plan_prefix(&c, b"abcd", 0).is_err());
        assert_eq!(format!("{c:?}"), before);
    }
}

#[test]
fn every_intervening_control_ancillary_message_and_empty_fragment_refuses() {
    let mut ancillary = plain(b"cd");
    if let InboundOutcome::Stream { ancillary, .. } = &mut ancillary {
        *ancillary = Some(NetworkAncillaryDataV2 {
            bytes: vec![],
            objects: vec![],
            truncated: false,
        });
    }
    let mut flags = plain(b"cd");
    if let InboundOutcome::Stream { message_flags, .. } = &mut flags {
        *message_flags = libc::MSG_EOR;
    }
    let mut message = plain(b"cd");
    if let InboundOutcome::Stream {
        requires_message_io,
        ..
    } = &mut message
    {
        *requires_message_io = true;
    }
    for boundary in [
        ancillary,
        flags,
        message,
        plain(b""),
        InboundOutcome::Error {
            stream_offset: 2,
            errno: libc::ECONNRESET,
        },
        InboundOutcome::PeerShutdown {
            stream_offset: 2,
            direction: NetworkShutdownV2::Write,
        },
        InboundOutcome::Control(ConnectionOutcome::Accept {
            accepted: NetworkChannelId(7),
            peer: None,
            ancillary: None,
        }),
    ] {
        let mut c = channel(&[b"ab", b"cd"]);
        c.inbound.insert(1, boundary);
        let before = format!("{c:?}");
        assert!(plan_prefix(&c, b"abcd", 0).is_err());
        assert_eq!(format!("{c:?}"), before);
    }
}

#[test]
fn private_suffix_refuses_terminal_or_unpublished_queue_boundary() {
    for variant in 0..5 {
        let mut c = channel(&[b"ab"]);
        match variant {
            0 => c.published_ingress.as_mut().unwrap().terminal = true,
            1 => c.peer_write_closed = true,
            2 => c.inbound.push_back(InboundOutcome::Error {
                stream_offset: 2,
                errno: libc::ECONNRESET,
            }),
            3 => c.inbound.push_back(plain(b"cd")),
            4 => c.local_read_shutdown = true,
            _ => unreachable!(),
        }
        let before = format!("{c:?}");
        assert!(plan_prefix(&c, b"abcd", 0).is_err(), "variant {variant}");
        assert_eq!(format!("{c:?}"), before);
    }
}

#[test]
fn frontiers_overflow_transport_empty_selection_and_origin_changes_refuse() {
    for variant in 0..7 {
        let mut c = channel(&[]);
        let mut origin = 0;
        let mut bytes = &b"a"[..];
        match variant {
            0 => {
                origin = u64::MAX;
                c.inbound_consumed = origin;
                c.published_ingress.as_mut().unwrap().stream_offset = origin;
            }
            1 => {
                origin = 1;
                c.inbound_consumed = 1;
            }
            2 => c.transport = NetworkTransportV2::Udp,
            3 => bytes = &[],
            4 => c.inbound_consumed = 1,
            5 => c.published_ingress.as_mut().unwrap().stream_offset = 2,
            6 => c.local_read_shutdown = true,
            _ => unreachable!(),
        }
        let before = format!("{c:?}");
        assert!(plan_prefix(&c, bytes, origin).is_err(), "variant {variant}");
        assert_eq!(format!("{c:?}"), before);
    }
}

#[test]
fn readiness_projection_preserves_error_hangup_and_explicit_levels_after_exact_prefix() {
    for next in [
        InboundOutcome::Error {
            stream_offset: 2,
            errno: libc::ECONNRESET,
        },
        InboundOutcome::PeerShutdown {
            stream_offset: 2,
            direction: NetworkShutdownV2::Write,
        },
    ] {
        let mut c = channel(&[b"ab"]);
        c.inbound.push_back(next);
        c.explicit_readiness.writable = true;
        let p = plan_prefix(&c, b"ab", 0).unwrap();
        assert!(p.readiness_after.readable && p.readiness_after.writable);
        assert_eq!(
            p.readiness_after.error,
            matches!(c.inbound.get(1), Some(InboundOutcome::Error { .. }))
        );
        assert_eq!(
            p.readiness_after.hangup,
            matches!(c.inbound.get(1), Some(InboundOutcome::PeerShutdown { .. }))
        );
        assert_eq!(p.private_suffix, 2..2);
    }
}

impl PreparedPrivatePublication {
    pub(crate) fn private_publication_fixture_summary(
        &self,
    ) -> (Vec<(usize, bool)>, Range<usize>, u64, u64, bool) {
        let p = &self.plan.prefix;
        (
            p.fragments.iter().map(|f| (f.length, f.whole)).collect(),
            p.private_suffix.clone(),
            p.consumed_after,
            p.published_after,
            p.readiness_after.readable,
        )
    }
}
impl NetworkReplayEngine {
    pub(crate) fn private_publication_fixture(
        &self,
        call: NetworkStreamCallId,
    ) -> Option<Arc<PreparedPrivatePublication>> {
        self.stream_calls[&call]
            .private_drain
            .as_ref()
            .and_then(|d| d.publication.clone())
    }

    /// Explicit old-codec published queue premise for component planning tests.
    /// Production preparation never invokes publish_ingress or fabricates units.
    pub(crate) fn publish_private_prefix_fixture(
        &mut self,
        call: NetworkStreamCallId,
        fragments: &[Vec<u8>],
    ) {
        let file = self.stream_calls[&call].open_file.unwrap();
        let channel = self.bound_channel(file).unwrap();
        let EngineState::Record(trace) = &self.mode else {
            panic!()
        };
        let release = NetworkReleaseV2 {
            not_before_global_time: trace.epoch_global_time().unwrap(),
            after_transmitted_offset: 0,
        };
        let mut offset = 0;
        for bytes in fragments {
            self.publish_ingress(
                file,
                NetworkInputEventV2 {
                    ordinal: 0,
                    channel,
                    release,
                    event: NetworkInputKindV2::StreamBytes {
                        stream_offset: offset,
                        bytes: bytes.clone(),
                    },
                },
            )
            .unwrap();
            offset += bytes.len() as u64;
        }
    }

    pub(crate) fn change_private_publication_fixture(
        &mut self,
        call: NetworkStreamCallId,
        variant: usize,
    ) {
        let state = &self.stream_calls[&call];
        let file = state.open_file.unwrap();
        let lease = state.foreground_store.as_ref().unwrap().lease();
        let channel = self.stream_operations[&lease].channel;
        match variant {
            0 => self.channels.get_mut(&channel).unwrap().inbound_consumed += 1,
            1 => {
                self.shadow
                    .as_mut()
                    .unwrap()
                    .sockets
                    .get_mut(&file)
                    .unwrap()
                    .native
                    .as_mut()
                    .unwrap()
                    .physical_observed
                    .bytes += 1
            }
            2 => {
                self.shadow
                    .as_mut()
                    .unwrap()
                    .sockets
                    .get_mut(&file)
                    .unwrap()
                    .consume_epoch = u64::MAX
            }
            3 => self.shadow_deliveries.get_mut(&lease).unwrap().selected_len += 1,
            4 => {
                self.stream_calls
                    .get_mut(&call)
                    .unwrap()
                    .native_receive
                    .last_mut()
                    .unwrap()
                    .joined = false
            }
            5 => {
                self.stream_calls
                    .get_mut(&call)
                    .unwrap()
                    .private_drain
                    .as_mut()
                    .unwrap()
                    .matched = false
            }
            6 => self.channels.get_mut(&channel).unwrap().local_read_shutdown = true,
            7 => self.stream_calls.get_mut(&call).unwrap().final_wait = true,
            _ => panic!("unknown explicit private publication fixture mutation"),
        }
    }
}
