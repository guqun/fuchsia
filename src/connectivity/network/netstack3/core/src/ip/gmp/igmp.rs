// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Internet Group Management Protocol, Version 2 (IGMPv2).
//!
//! IGMPv2 is a communications protocol used by hosts and adjacent routers on
//! IPv4 networks to establish multicast group memberships.

use core::{fmt::Debug, time::Duration};

use log::{debug, error};
use net_types::{
    ip::{AddrSubnet, Ip as _, Ipv4, Ipv4Addr},
    MulticastAddr, SpecifiedAddr, Witness,
};
use packet::{BufferMut, EmptyBuf, InnerPacketBuilder, Serializer};
use packet_formats::{
    igmp::{
        messages::{IgmpLeaveGroup, IgmpMembershipReportV1, IgmpMembershipReportV2, IgmpPacket},
        IgmpMessage, IgmpPacketBuilder, MessageType,
    },
    ip::Ipv4Proto,
    ipv4::{
        options::{Ipv4Option, Ipv4OptionData},
        Ipv4OptionsTooLongError, Ipv4PacketBuilder, Ipv4PacketBuilderWithOptions,
    },
    utils::NonZeroDuration,
};
use thiserror::Error;
use zerocopy::ByteSlice;

use crate::{
    context::{FrameContext, InstantContext, RngContext, TimerContext, TimerHandler},
    ip::{
        gmp::{
            gmp_handle_disabled, gmp_handle_maybe_enabled, gmp_handle_timer, gmp_join_group,
            gmp_leave_group, handle_query_message, handle_report_message, GmpContext,
            GmpDelayedReportTimerId, GmpHandler, GmpMessage, GmpMessageType, GmpStateMachine,
            GroupJoinResult, GroupLeaveResult, MulticastGroupSet, ProtocolSpecific, QueryTarget,
        },
        IpDeviceIdContext,
    },
    Instant,
};

/// Metadata for sending an IGMP packet.
///
/// `IgmpPacketMetadata` is used by [`IgmpContext`]'s [`FrameContext`] bound.
/// When [`FrameContext::send_frame`] is called with an `IgmpPacketMetadata`,
/// the body will be encapsulated in an IP packet with a TTL of 1 and with the
/// "Router Alert" option set.
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct IgmpPacketMetadata<D> {
    pub(crate) device: D,
    pub(crate) dst_ip: MulticastAddr<Ipv4Addr>,
}

impl<D> IgmpPacketMetadata<D> {
    fn new(device: D, dst_ip: MulticastAddr<Ipv4Addr>) -> IgmpPacketMetadata<D> {
        IgmpPacketMetadata { device, dst_ip }
    }
}

/// The execution context for the Internet Group Management Protocol (IGMP).
pub(crate) trait IgmpContext:
    IpDeviceIdContext<Ipv4>
    + TimerContext<IgmpTimerId<Self::DeviceId>>
    + FrameContext<EmptyBuf, IgmpPacketMetadata<Self::DeviceId>>
    + RngContext
{
    /// Gets an IP address and subnet associated with this device.
    fn get_ip_addr_subnet(&self, device: Self::DeviceId) -> Option<AddrSubnet<Ipv4Addr>>;

    /// Is IGMP enabled for `device`?
    ///
    /// If `igmp_enabled` returns false, then [`GmpHandler::gmp_join_group`] and
    /// [`GmpHandler::gmp_leave_group`] will still join/leave multicast groups
    /// locally, and inbound IGMP packets will still be processed, but no timers
    /// will be installed, and no outbound IGMP traffic will be generated.
    fn igmp_enabled(&self, device: Self::DeviceId) -> bool;

    /// Gets mutable access to the device's IGMP state and RNG.
    fn get_state_mut_and_rng(
        &mut self,
        device: Self::DeviceId,
    ) -> (
        &mut MulticastGroupSet<Ipv4Addr, IgmpGroupState<<Self as InstantContext>::Instant>>,
        &mut Self::Rng,
    );

    /// Gets mutable access to the device's IGMP state.
    fn get_state_mut(
        &mut self,
        device: Self::DeviceId,
    ) -> &mut MulticastGroupSet<Ipv4Addr, IgmpGroupState<<Self as InstantContext>::Instant>> {
        let (state, _rng): (_, &mut Self::Rng) = self.get_state_mut_and_rng(device);
        state
    }

    /// Gets immutable access to the device's IGMP state.
    fn get_state(
        &self,
        device: Self::DeviceId,
    ) -> &MulticastGroupSet<Ipv4Addr, IgmpGroupState<<Self as InstantContext>::Instant>>;
}

impl<C: IgmpContext> GmpHandler<Ipv4> for C {
    fn gmp_handle_maybe_enabled(&mut self, device: Self::DeviceId) {
        gmp_handle_maybe_enabled(self, &mut (), device)
    }

    fn gmp_handle_disabled(&mut self, device: Self::DeviceId) {
        gmp_handle_disabled(self, &mut (), device)
    }

    fn gmp_join_group(
        &mut self,
        device: C::DeviceId,
        group_addr: MulticastAddr<Ipv4Addr>,
    ) -> GroupJoinResult {
        gmp_join_group(self, &mut (), device, group_addr)
    }

    fn gmp_leave_group(
        &mut self,
        device: C::DeviceId,
        group_addr: MulticastAddr<Ipv4Addr>,
    ) -> GroupLeaveResult {
        gmp_leave_group(self, &mut (), device, group_addr)
    }
}

/// A handler for incoming IGMP packets.
///
/// A blanket implementation is provided for all `C: IgmpContext`.
pub(crate) trait IgmpPacketHandler<DeviceId, B: BufferMut> {
    /// Receive an IGMP message in an IP packet.
    fn receive_igmp_packet(
        &mut self,
        device: DeviceId,
        src_ip: Ipv4Addr,
        dst_ip: SpecifiedAddr<Ipv4Addr>,
        buffer: B,
    );
}

impl<C: IgmpContext, B: BufferMut> IgmpPacketHandler<C::DeviceId, B> for C {
    fn receive_igmp_packet(
        &mut self,
        device: C::DeviceId,
        _src_ip: Ipv4Addr,
        _dst_ip: SpecifiedAddr<Ipv4Addr>,
        mut buffer: B,
    ) {
        let packet = match buffer.parse_with::<_, IgmpPacket<&[u8]>>(()) {
            Ok(packet) => packet,
            Err(_) => {
                debug!("Cannot parse the incoming IGMP packet, dropping.");
                return;
            } // TODO: Do something else here?
        };

        if let Err(e) = match packet {
            IgmpPacket::MembershipQueryV2(msg) => {
                let addr = msg.group_addr();
                SpecifiedAddr::new(addr)
                    .map_or(Some(QueryTarget::Unspecified), |addr| {
                        MulticastAddr::new(addr.get()).map(QueryTarget::Specified)
                    })
                    .map_or(Err(IgmpError::NotAMember { addr }), |group_addr| {
                        handle_query_message(
                            self,
                            device,
                            group_addr,
                            msg.max_response_time().into(),
                        )
                    })
            }
            IgmpPacket::MembershipReportV1(msg) => {
                let addr = msg.group_addr();
                MulticastAddr::new(addr).map_or(Err(IgmpError::NotAMember { addr }), |group_addr| {
                    handle_report_message(self, &mut (), device, group_addr)
                })
            }
            IgmpPacket::MembershipReportV2(msg) => {
                let addr = msg.group_addr();
                MulticastAddr::new(addr).map_or(Err(IgmpError::NotAMember { addr }), |group_addr| {
                    handle_report_message(self, &mut (), device, group_addr)
                })
            }
            IgmpPacket::LeaveGroup(_) => {
                debug!("Hosts are not interested in Leave Group messages");
                return;
            }
            _ => {
                debug!("We do not support IGMPv3 yet");
                return;
            }
        } {
            error!("Error occurred when handling IGMPv2 message: {}", e);
        }
    }
}

impl<B: ByteSlice, M: MessageType<B, FixedHeader = Ipv4Addr>> GmpMessage<Ipv4>
    for IgmpMessage<B, M>
{
    fn group_addr(&self) -> Ipv4Addr {
        self.group_addr()
    }
}

impl<C: IgmpContext> GmpContext<Ipv4, Igmpv2ProtocolSpecific> for C {
    type Err = IgmpError;
    type GroupState = IgmpGroupState<Self::Instant>;

    fn gmp_disabled(&self, device: Self::DeviceId, _addr: MulticastAddr<Ipv4Addr>) -> bool {
        !self.igmp_enabled(device)
    }

    fn send_message(
        &mut self,
        device: Self::DeviceId,
        group_addr: MulticastAddr<Ipv4Addr>,
        msg_type: GmpMessageType<Igmpv2ProtocolSpecific>,
    ) {
        let result = match msg_type {
            GmpMessageType::Report(Igmpv2ProtocolSpecific { v1_router_present }) => {
                if v1_router_present {
                    send_igmp_message::<_, _, IgmpMembershipReportV1>(
                        self,
                        &mut (),
                        device,
                        group_addr,
                        group_addr,
                        (),
                    )
                } else {
                    send_igmp_message::<_, _, IgmpMembershipReportV2>(
                        self,
                        &mut (),
                        device,
                        group_addr,
                        group_addr,
                        (),
                    )
                }
            }
            GmpMessageType::Leave => send_igmp_message::<_, _, IgmpLeaveGroup>(
                self,
                &mut (),
                device,
                group_addr,
                Ipv4::ALL_ROUTERS_MULTICAST_ADDRESS,
                (),
            ),
        };

        match result {
            Ok(()) => {}
            Err(err) => error!(
                "error sending IGMP message ({:?}) on device {} for group {}: {}",
                msg_type, device, group_addr, err
            ),
        }
    }

    fn run_actions(&mut self, device: Self::DeviceId, actions: Igmpv2Actions) {
        match actions {
            Igmpv2Actions::ScheduleV1RouterPresentTimer(duration) => {
                let _: Option<C::Instant> =
                    self.schedule_timer(duration, IgmpTimerId::new_v1_router_present(device));
            }
        }
    }

    fn not_a_member_err(addr: Ipv4Addr) -> Self::Err {
        Self::Err::NotAMember { addr }
    }

    fn get_state_mut_and_rng(
        &mut self,
        device: C::DeviceId,
    ) -> (&mut MulticastGroupSet<Ipv4Addr, Self::GroupState>, &mut Self::Rng) {
        self.get_state_mut_and_rng(device)
    }

    fn get_state(&self, device: C::DeviceId) -> &MulticastGroupSet<Ipv4Addr, Self::GroupState> {
        self.get_state(device)
    }
}

#[derive(Debug, Error)]
pub(crate) enum IgmpError {
    /// The host is trying to operate on an group address of which the host is
    /// not a member.
    #[error("the host has not already been a member of the address: {}", addr)]
    NotAMember { addr: Ipv4Addr },
    /// Failed to send an IGMP packet.
    #[error("failed to send out an IGMP packet to address: {}", addr)]
    SendFailure { addr: Ipv4Addr },
}

pub(crate) type IgmpResult<T> = Result<T, IgmpError>;

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub(crate) enum IgmpTimerId<DeviceId> {
    /// A GMP timer.
    Gmp(GmpDelayedReportTimerId<Ipv4Addr, DeviceId>),
    /// The timer used to determine whether there is a router speaking IGMPv1.
    V1RouterPresent { device: DeviceId },
}

impl<DeviceId> From<GmpDelayedReportTimerId<Ipv4Addr, DeviceId>> for IgmpTimerId<DeviceId> {
    fn from(id: GmpDelayedReportTimerId<Ipv4Addr, DeviceId>) -> IgmpTimerId<DeviceId> {
        IgmpTimerId::Gmp(id)
    }
}

// Impl `TimerContext<GmpDelayedReportTimerId<Ipv4Addr, C::DeviceId>>` for all
// implementors of `TimerContext<IgmpTimerId<C::DeviceId>> + IpDeviceIdContext<Ipv4>`.
impl_timer_context!(
    IpDeviceIdContext<Ipv4>,
    IgmpTimerId<C::DeviceId>,
    GmpDelayedReportTimerId<Ipv4Addr, C::DeviceId>,
    IgmpTimerId::Gmp(id),
    id
);

impl<DeviceId> IgmpTimerId<DeviceId> {
    fn new_v1_router_present(device: DeviceId) -> IgmpTimerId<DeviceId> {
        IgmpTimerId::V1RouterPresent { device }
    }
}

impl<C: IgmpContext> TimerHandler<IgmpTimerId<C::DeviceId>> for C {
    fn handle_timer(&mut self, timer: IgmpTimerId<C::DeviceId>) {
        match timer {
            IgmpTimerId::Gmp(id) => gmp_handle_timer(self, &mut (), id),
            IgmpTimerId::V1RouterPresent { device } => {
                for (_, IgmpGroupState(state)) in self.get_state_mut(device).iter_mut() {
                    state.v1_router_present_timer_expired();
                }
            }
        }
    }
}

fn send_igmp_message<SC: IgmpContext, C, M>(
    sync_ctx: &mut SC,
    _ctx: &mut C,
    device: SC::DeviceId,
    group_addr: MulticastAddr<Ipv4Addr>,
    dst_ip: MulticastAddr<Ipv4Addr>,
    max_resp_time: M::MaxRespTime,
) -> IgmpResult<()>
where
    M: MessageType<EmptyBuf, FixedHeader = Ipv4Addr, VariableBody = ()>,
{
    // As per RFC 3376 section 4.2.13,
    //
    //   An IGMP report is sent with a valid IP source address for the
    //   destination subnet. The 0.0.0.0 source address may be used by a
    //   system that has not yet acquired an IP address.
    //
    // Note that RFC 3376 targets IGMPv3 but we implement IGMPv2. However,
    // we still allow sending IGMP packets with the unspecified source when no
    // address is available so that IGMP snooping switches know to forward
    // multicast packets to us before an address is available. See RFC 4541 for
    // some details regarding considerations for IGMP/MLD snooping switches.
    let src_ip =
        sync_ctx.get_ip_addr_subnet(device).map_or(Ipv4::UNSPECIFIED_ADDRESS, |a| a.addr().get());

    let body =
        IgmpPacketBuilder::<EmptyBuf, M>::new_with_resp_time(group_addr.get(), max_resp_time);
    let builder = match Ipv4PacketBuilderWithOptions::new(
        Ipv4PacketBuilder::new(src_ip, dst_ip, 1, Ipv4Proto::Igmp),
        &[Ipv4Option { copied: true, data: Ipv4OptionData::RouterAlert { data: 0 } }],
    ) {
        Err(Ipv4OptionsTooLongError) => return Err(IgmpError::SendFailure { addr: *group_addr }),
        Ok(builder) => builder,
    };
    let body = body.into_serializer().encapsulate(builder);

    sync_ctx
        .send_frame(IgmpPacketMetadata::new(device, dst_ip), body)
        .map_err(|_| IgmpError::SendFailure { addr: *group_addr })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Igmpv2ProtocolSpecific {
    v1_router_present: bool,
}

impl Default for Igmpv2ProtocolSpecific {
    fn default() -> Self {
        Igmpv2ProtocolSpecific { v1_router_present: false }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum Igmpv2Actions {
    ScheduleV1RouterPresentTimer(Duration),
}

#[derive(Debug)]
struct Igmpv2HostConfig {
    // When a host wants to send a report not because of a query, this value is
    // used as the delay timer.
    unsolicited_report_interval: Duration,
    // When this option is true, the host can send a leave message even when it
    // is not the last one in the multicast group.
    send_leave_anyway: bool,
    // Default timer value for Version 1 Router Present Timeout.
    v1_router_present_timeout: Duration,
}

/// The default value for `unsolicited_report_interval` as per [RFC 2236 Section
/// 8.10].
///
/// [RFC 2236 Section 8.10]: https://tools.ietf.org/html/rfc2236#section-8.10
const DEFAULT_UNSOLICITED_REPORT_INTERVAL: Duration = Duration::from_secs(10);
/// The default value for `v1_router_present_timeout` as per [RFC 2236 Section
/// 8.11].
///
/// [RFC 2236 Section 8.11]: https://tools.ietf.org/html/rfc2236#section-8.11
const DEFAULT_V1_ROUTER_PRESENT_TIMEOUT: Duration = Duration::from_secs(400);
/// The default value for the `MaxRespTime` if the query is a V1 query, whose
/// `MaxRespTime` field is 0 in the packet. Please refer to [RFC 2236 Section
/// 4].
///
/// [RFC 2236 Section 4]: https://tools.ietf.org/html/rfc2236#section-4
const DEFAULT_V1_QUERY_MAX_RESP_TIME: Duration = Duration::from_secs(10);

impl Default for Igmpv2HostConfig {
    fn default() -> Self {
        Igmpv2HostConfig {
            unsolicited_report_interval: DEFAULT_UNSOLICITED_REPORT_INTERVAL,
            send_leave_anyway: false,
            v1_router_present_timeout: DEFAULT_V1_ROUTER_PRESENT_TIMEOUT,
        }
    }
}

impl ProtocolSpecific for Igmpv2ProtocolSpecific {
    type Actions = Igmpv2Actions;
    type Config = Igmpv2HostConfig;

    fn cfg_unsolicited_report_interval(cfg: &Self::Config) -> Duration {
        cfg.unsolicited_report_interval
    }

    fn cfg_send_leave_anyway(cfg: &Self::Config) -> bool {
        cfg.send_leave_anyway
    }

    fn get_max_resp_time(resp_time: Duration) -> Option<NonZeroDuration> {
        // As per RFC 2236 section 4,
        //
        //   An IGMPv2 host may be placed on a subnet where the Querier router
        //   has not yet been upgraded to IGMPv2. The following requirements
        //   apply:
        //
        //        The IGMPv1 router will send General Queries with the Max
        //        Response Time set to 0.  This MUST be interpreted as a value
        //        of 100 (10 seconds).
        Some(NonZeroDuration::new(resp_time).unwrap_or_else(|| {
            const_unwrap::const_unwrap_option(NonZeroDuration::new(DEFAULT_V1_QUERY_MAX_RESP_TIME))
        }))
    }

    fn do_query_received_specific(
        cfg: &Self::Config,
        max_resp_time: Duration,
        _old: Igmpv2ProtocolSpecific,
    ) -> (Igmpv2ProtocolSpecific, Option<Igmpv2Actions>) {
        // IGMPv2 hosts should be compatible with routers that only speak
        // IGMPv1. When an IGMPv2 host receives an IGMPv1 query (whose
        // `MaxRespCode` is 0), it should set up a timer and only respond with
        // IGMPv1 responses before the timer expires. Please refer to
        // https://tools.ietf.org/html/rfc2236#section-4 for details.
        let v1_router_present = max_resp_time.as_micros() == 0;
        (
            Igmpv2ProtocolSpecific { v1_router_present },
            v1_router_present.then(|| {
                Igmpv2Actions::ScheduleV1RouterPresentTimer(cfg.v1_router_present_timeout)
            }),
        )
    }
}

#[cfg_attr(test, derive(Debug))]
pub(crate) struct IgmpGroupState<I: Instant>(GmpStateMachine<I, Igmpv2ProtocolSpecific>);

impl<I: Instant> From<GmpStateMachine<I, Igmpv2ProtocolSpecific>> for IgmpGroupState<I> {
    fn from(state: GmpStateMachine<I, Igmpv2ProtocolSpecific>) -> IgmpGroupState<I> {
        IgmpGroupState(state)
    }
}

impl<I: Instant> From<IgmpGroupState<I>> for GmpStateMachine<I, Igmpv2ProtocolSpecific> {
    fn from(
        IgmpGroupState(state): IgmpGroupState<I>,
    ) -> GmpStateMachine<I, Igmpv2ProtocolSpecific> {
        state
    }
}

impl<I: Instant> AsMut<GmpStateMachine<I, Igmpv2ProtocolSpecific>> for IgmpGroupState<I> {
    fn as_mut(&mut self) -> &mut GmpStateMachine<I, Igmpv2ProtocolSpecific> {
        let Self(s) = self;
        s
    }
}

impl<I: Instant> GmpStateMachine<I, Igmpv2ProtocolSpecific> {
    fn v1_router_present_timer_expired(&mut self) {
        self.update_with_protocol_specific(Igmpv2ProtocolSpecific { v1_router_present: false });
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::convert::TryInto;

    use net_types::{
        ethernet::Mac,
        ip::{AddrSubnet, Ip as _},
    };
    use packet::{
        serialize::{Buf, InnerPacketBuilder, Serializer},
        ParsablePacket as _,
    };
    use packet_formats::{
        igmp::messages::IgmpMembershipQueryV2,
        testutil::{parse_ip_packet, parse_ip_packet_in_ethernet_frame},
    };
    use rand_xorshift::XorShiftRng;
    use test_case::test_case;

    use super::*;
    use crate::{
        context::{
            testutil::{DummyInstant, DummyTimerCtxExt},
            DualStateContext,
        },
        ip::{
            gmp::{
                MemberState, QueryReceivedActions, ReportReceivedActions, ReportTimerExpiredActions,
            },
            DummyDeviceId,
        },
        testutil::{
            assert_empty, new_rng, run_with_many_seeds, DummyEventDispatcher,
            DummyEventDispatcherConfig, FakeCryptoRng, TestIpExt as _,
        },
        Ctx, StackStateBuilder, TimerId, TimerIdInner,
    };

    /// A dummy [`IgmpContext`] that stores the [`MulticastGroupSet`] and an
    /// optional IPv4 address and subnet that may be returned in calls to
    /// [`IgmpContext::get_ip_addr_subnet`].
    struct DummyIgmpCtx {
        groups: MulticastGroupSet<Ipv4Addr, IgmpGroupState<DummyInstant>>,
        igmp_enabled: bool,
        addr_subnet: Option<AddrSubnet<Ipv4Addr>>,
    }

    impl Default for DummyIgmpCtx {
        fn default() -> DummyIgmpCtx {
            DummyIgmpCtx {
                groups: MulticastGroupSet::default(),
                igmp_enabled: true,
                addr_subnet: None,
            }
        }
    }

    type DummyCtx = crate::context::testutil::DummyCtx<
        DummyIgmpCtx,
        IgmpTimerId<DummyDeviceId>,
        IgmpPacketMetadata<DummyDeviceId>,
        (),
        DummyDeviceId,
    >;

    impl IgmpContext for DummyCtx {
        fn get_ip_addr_subnet(&self, _device: DummyDeviceId) -> Option<AddrSubnet<Ipv4Addr>> {
            self.get_ref().addr_subnet
        }

        fn igmp_enabled(&self, _device: DummyDeviceId) -> bool {
            self.get_ref().igmp_enabled
        }

        fn get_state_mut_and_rng(
            &mut self,
            _id0: DummyDeviceId,
        ) -> (
            &mut MulticastGroupSet<Ipv4Addr, IgmpGroupState<DummyInstant>>,
            &mut FakeCryptoRng<XorShiftRng>,
        ) {
            let (state, rng) = self.get_states_mut();
            (&mut state.groups, rng)
        }

        fn get_state(
            &self,
            DummyDeviceId: DummyDeviceId,
        ) -> &MulticastGroupSet<Ipv4Addr, IgmpGroupState<DummyInstant>> {
            let (state, _rng) = self.get_states();
            &state.groups
        }
    }

    #[test]
    fn test_igmp_state_with_igmpv1_router() {
        run_with_many_seeds(|seed| {
            let mut rng = new_rng(seed);
            let (mut s, _actions) =
                GmpStateMachine::join_group(&mut rng, DummyInstant::default(), false);
            assert_eq!(
                s.query_received(&mut rng, Duration::from_secs(0), DummyInstant::default()),
                QueryReceivedActions {
                    generic: None,
                    protocol_specific: Some(Igmpv2Actions::ScheduleV1RouterPresentTimer(
                        DEFAULT_V1_ROUTER_PRESENT_TIMEOUT
                    ))
                }
            );
            assert_eq!(
                s.report_timer_expired(),
                ReportTimerExpiredActions {
                    send_report: Igmpv2ProtocolSpecific { v1_router_present: true }
                }
            );
        });
    }

    #[test]
    fn test_igmp_state_igmpv1_router_present_timer_expires() {
        run_with_many_seeds(|seed| {
            let mut rng = new_rng(seed);
            let (mut s, _actions) = GmpStateMachine::<_, Igmpv2ProtocolSpecific>::join_group(
                &mut rng,
                DummyInstant::default(),
                false,
            );
            assert_eq!(
                s.query_received(&mut rng, Duration::from_secs(0), DummyInstant::default()),
                QueryReceivedActions {
                    generic: None,
                    protocol_specific: Some(Igmpv2Actions::ScheduleV1RouterPresentTimer(
                        DEFAULT_V1_ROUTER_PRESENT_TIMEOUT
                    ))
                }
            );
            match s.get_inner() {
                MemberState::Delaying(state) => {
                    assert!(state.get_protocol_specific().v1_router_present);
                }
                _ => panic!("Wrong State!"),
            }
            s.v1_router_present_timer_expired();
            match s.get_inner() {
                MemberState::Delaying(state) => {
                    assert!(!state.get_protocol_specific().v1_router_present);
                }
                _ => panic!("Wrong State!"),
            }
            assert_eq!(
                s.query_received(&mut rng, Duration::from_secs(0), DummyInstant::default()),
                QueryReceivedActions {
                    generic: None,
                    protocol_specific: Some(Igmpv2Actions::ScheduleV1RouterPresentTimer(
                        DEFAULT_V1_ROUTER_PRESENT_TIMEOUT
                    ))
                }
            );
            assert_eq!(s.report_received(), ReportReceivedActions { stop_timer: true });
            s.v1_router_present_timer_expired();
            match s.get_inner() {
                MemberState::Idle(state) => {
                    assert!(!state.get_protocol_specific().v1_router_present);
                }
                _ => panic!("Wrong State!"),
            }
        });
    }

    const MY_ADDR: SpecifiedAddr<Ipv4Addr> =
        unsafe { SpecifiedAddr::new_unchecked(Ipv4Addr::new([192, 168, 0, 2])) };
    const ROUTER_ADDR: Ipv4Addr = Ipv4Addr::new([192, 168, 0, 1]);
    const OTHER_HOST_ADDR: Ipv4Addr = Ipv4Addr::new([192, 168, 0, 3]);
    const GROUP_ADDR: MulticastAddr<Ipv4Addr> = Ipv4::ALL_ROUTERS_MULTICAST_ADDRESS;
    const GROUP_ADDR_2: MulticastAddr<Ipv4Addr> =
        unsafe { MulticastAddr::new_unchecked(Ipv4Addr::new([224, 0, 0, 4])) };
    const REPORT_DELAY_TIMER_ID: IgmpTimerId<DummyDeviceId> =
        IgmpTimerId::Gmp(GmpDelayedReportTimerId { device: DummyDeviceId, group_addr: GROUP_ADDR });
    const REPORT_DELAY_TIMER_ID_2: IgmpTimerId<DummyDeviceId> =
        IgmpTimerId::Gmp(GmpDelayedReportTimerId {
            device: DummyDeviceId,
            group_addr: GROUP_ADDR_2,
        });
    const V1_ROUTER_PRESENT_TIMER_ID: IgmpTimerId<DummyDeviceId> =
        IgmpTimerId::V1RouterPresent { device: DummyDeviceId };

    fn receive_igmp_query(ctx: &mut DummyCtx, resp_time: Duration) {
        let ser = IgmpPacketBuilder::<Buf<Vec<u8>>, IgmpMembershipQueryV2>::new_with_resp_time(
            GROUP_ADDR.get(),
            resp_time.try_into().unwrap(),
        );
        let buff = ser.into_serializer().serialize_vec_outer().unwrap();
        ctx.receive_igmp_packet(DummyDeviceId, ROUTER_ADDR, MY_ADDR, buff);
    }

    fn receive_igmp_general_query(ctx: &mut DummyCtx, resp_time: Duration) {
        let ser = IgmpPacketBuilder::<Buf<Vec<u8>>, IgmpMembershipQueryV2>::new_with_resp_time(
            Ipv4Addr::new([0, 0, 0, 0]),
            resp_time.try_into().unwrap(),
        );
        let buff = ser.into_serializer().serialize_vec_outer().unwrap();
        ctx.receive_igmp_packet(DummyDeviceId, ROUTER_ADDR, MY_ADDR, buff);
    }

    fn receive_igmp_report(ctx: &mut DummyCtx) {
        let ser = IgmpPacketBuilder::<Buf<Vec<u8>>, IgmpMembershipReportV2>::new(GROUP_ADDR.get());
        let buff = ser.into_serializer().serialize_vec_outer().unwrap();
        ctx.receive_igmp_packet(DummyDeviceId, OTHER_HOST_ADDR, MY_ADDR, buff);
    }

    fn setup_simple_test_environment_with_addr_subnet(
        seed: u128,
        a: Option<AddrSubnet<Ipv4Addr>>,
    ) -> DummyCtx {
        let mut ctx = DummyCtx::default();
        ctx.seed_rng(seed);
        ctx.get_mut().addr_subnet = a;
        ctx
    }

    fn setup_simple_test_environment(seed: u128) -> DummyCtx {
        setup_simple_test_environment_with_addr_subnet(
            seed,
            Some(AddrSubnet::new(MY_ADDR.get(), 24).unwrap()),
        )
    }

    fn ensure_ttl_ihl_rtr(ctx: &DummyCtx) {
        for (_, frame) in ctx.frames() {
            assert_eq!(frame[8], 1); // TTL,
            assert_eq!(&frame[20..24], &[148, 4, 0, 0]); // RTR
            assert_eq!(frame[0], 0x46); // IHL
        }
    }

    #[test_case(Some(MY_ADDR); "specified_src")]
    #[test_case(None; "unspecified_src")]
    fn test_igmp_simple_integration(src_ip: Option<SpecifiedAddr<Ipv4Addr>>) {
        let check_report = |ctx: &mut DummyCtx| {
            let expected_src_ip = src_ip.map_or(Ipv4::UNSPECIFIED_ADDRESS, |a| a.get());

            assert_matches::assert_matches!(
                &ctx.take_frames()[..],
                [(IgmpPacketMetadata { device: DummyDeviceId, dst_ip }, frame)] => {
                    assert_eq!(dst_ip, &GROUP_ADDR);
                    let (body, src_ip, dst_ip, proto, ttl) =
                        parse_ip_packet::<Ipv4>(frame).unwrap();
                    assert_eq!(src_ip, expected_src_ip);
                    assert_eq!(dst_ip, GROUP_ADDR.get());
                    assert_eq!(proto, Ipv4Proto::Igmp);
                    assert_eq!(ttl, 1);
                    let mut bv = &body[..];
                    assert_matches::assert_matches!(
                        IgmpPacket::parse(&mut bv, ()).unwrap(),
                        IgmpPacket::MembershipReportV2(msg) => {
                            assert_eq!(msg.group_addr(), GROUP_ADDR.get());
                        }
                    );
                }
            );
        };

        let addr_subnet = src_ip.map(|a| AddrSubnet::new(a.get(), 16).unwrap());
        run_with_many_seeds(|seed| {
            let mut ctx = setup_simple_test_environment_with_addr_subnet(seed, addr_subnet);

            // Joining a group should send a report.
            assert_eq!(ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR), GroupJoinResult::Joined(()));
            check_report(&mut ctx);

            // Should send a report after a query.
            receive_igmp_query(&mut ctx, Duration::from_secs(10));
            assert_eq!(
                ctx.trigger_next_timer(TimerHandler::handle_timer),
                Some(REPORT_DELAY_TIMER_ID)
            );
            check_report(&mut ctx);
        });
    }

    #[test]
    fn test_igmp_integration_fallback_from_idle() {
        run_with_many_seeds(|seed| {
            let mut ctx = setup_simple_test_environment(seed);
            assert_eq!(ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR), GroupJoinResult::Joined(()));
            assert_eq!(ctx.frames().len(), 1);

            assert_eq!(
                ctx.trigger_next_timer(TimerHandler::handle_timer),
                Some(REPORT_DELAY_TIMER_ID)
            );
            assert_eq!(ctx.frames().len(), 2);

            receive_igmp_query(&mut ctx, Duration::from_secs(10));

            // We have received a query, hence we are falling back to Delay
            // Member state.
            let IgmpGroupState(group_state) =
                IgmpContext::get_state_mut(&mut ctx, DummyDeviceId).get(&GROUP_ADDR).unwrap();
            match group_state.get_inner() {
                MemberState::Delaying(_) => {}
                _ => panic!("Wrong State!"),
            }

            assert_eq!(
                ctx.trigger_next_timer(TimerHandler::handle_timer),
                Some(REPORT_DELAY_TIMER_ID)
            );
            assert_eq!(ctx.frames().len(), 3);
            ensure_ttl_ihl_rtr(&ctx);
        });
    }

    #[test]
    fn test_igmp_integration_igmpv1_router_present() {
        run_with_many_seeds(|seed| {
            let mut ctx = setup_simple_test_environment(seed);

            assert_eq!(ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR), GroupJoinResult::Joined(()));
            let now = ctx.now();
            ctx.timer_ctx().assert_timers_installed([(
                REPORT_DELAY_TIMER_ID,
                now..=(now + DEFAULT_UNSOLICITED_REPORT_INTERVAL),
            )]);
            let instant1 = ctx.timer_ctx().timers()[0].0.clone();

            receive_igmp_query(&mut ctx, Duration::from_secs(0));
            assert_eq!(ctx.frames().len(), 1);

            // Since we have heard from the v1 router, we should have set our
            // flag.
            let IgmpGroupState(group_state) =
                IgmpContext::get_state_mut(&mut ctx, DummyDeviceId).get(&GROUP_ADDR).unwrap();
            match group_state.get_inner() {
                MemberState::Delaying(state) => {
                    assert!(state.get_protocol_specific().v1_router_present)
                }
                _ => panic!("Wrong State!"),
            }

            assert_eq!(ctx.frames().len(), 1);
            // Two timers: one for the delayed report, one for the v1 router
            // timer.
            let now = ctx.now();
            ctx.timer_ctx().assert_timers_installed([
                (REPORT_DELAY_TIMER_ID, now..=(now + DEFAULT_UNSOLICITED_REPORT_INTERVAL)),
                (V1_ROUTER_PRESENT_TIMER_ID, now..=(now + DEFAULT_V1_ROUTER_PRESENT_TIMEOUT)),
            ]);
            let instant2 = ctx.timer_ctx().timers()[1].0.clone();
            assert_eq!(instant1, instant2);

            assert_eq!(
                ctx.trigger_next_timer(TimerHandler::handle_timer),
                Some(REPORT_DELAY_TIMER_ID)
            );
            // After the first timer, we send out our V1 report.
            assert_eq!(ctx.frames().len(), 2);
            // The last frame being sent should be a V1 report.
            let (_, frame) = ctx.frames().last().unwrap();
            // 34 and 0x12 are hacky but they can quickly tell it is a V1
            // report.
            assert_eq!(frame[24], 0x12);

            assert_eq!(
                ctx.trigger_next_timer(TimerHandler::handle_timer),
                Some(V1_ROUTER_PRESENT_TIMER_ID)
            );
            // After the second timer, we should reset our flag for v1 routers.
            let IgmpGroupState(group_state) =
                IgmpContext::get_state_mut(&mut ctx, DummyDeviceId).get(&GROUP_ADDR).unwrap();
            match group_state.get_inner() {
                MemberState::Idle(state) => {
                    assert!(!state.get_protocol_specific().v1_router_present)
                }
                _ => panic!("Wrong State!"),
            }

            receive_igmp_query(&mut ctx, Duration::from_secs(10));
            assert_eq!(
                ctx.trigger_next_timer(TimerHandler::handle_timer),
                Some(REPORT_DELAY_TIMER_ID)
            );
            assert_eq!(ctx.frames().len(), 3);
            // Now we should get V2 report
            assert_eq!(ctx.frames().last().unwrap().1[24], 0x16);
            ensure_ttl_ihl_rtr(&ctx);
        });
    }

    #[test]
    fn test_igmp_integration_delay_reset_timer() {
        // This seed value was chosen to later produce a timer duration > 100ms.
        let mut ctx = setup_simple_test_environment(123456);
        assert_eq!(ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR), GroupJoinResult::Joined(()));
        let now = ctx.now();
        ctx.timer_ctx().assert_timers_installed([(
            REPORT_DELAY_TIMER_ID,
            now..=(now + DEFAULT_UNSOLICITED_REPORT_INTERVAL),
        )]);
        let instant1 = ctx.timer_ctx().timers()[0].0.clone();
        let start = ctx.now();
        let duration = Duration::from_micros(((instant1 - start).as_micros() / 2) as u64);
        assert!(duration.as_millis() > 100);
        receive_igmp_query(&mut ctx, duration);
        assert_eq!(ctx.frames().len(), 1);
        let now = ctx.now();
        ctx.timer_ctx().assert_timers_installed([(REPORT_DELAY_TIMER_ID, now..=(now + duration))]);
        let instant2 = ctx.timer_ctx().timers()[0].0.clone();
        // Because of the message, our timer should be reset to a nearer future.
        assert!(instant2 <= instant1);
        assert_eq!(ctx.trigger_next_timer(TimerHandler::handle_timer), Some(REPORT_DELAY_TIMER_ID));
        assert!(ctx.now() - start <= duration);
        assert_eq!(ctx.frames().len(), 2);
        // Make sure it is a V2 report.
        assert_eq!(ctx.frames().last().unwrap().1[24], 0x16);
        ensure_ttl_ihl_rtr(&ctx);
    }

    #[test]
    fn test_igmp_integration_last_send_leave() {
        run_with_many_seeds(|seed| {
            let mut ctx = setup_simple_test_environment(seed);
            assert_eq!(ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR), GroupJoinResult::Joined(()));
            let now = ctx.now();
            ctx.timer_ctx().assert_timers_installed([(
                REPORT_DELAY_TIMER_ID,
                now..=(now + DEFAULT_UNSOLICITED_REPORT_INTERVAL),
            )]);
            // The initial unsolicited report.
            assert_eq!(ctx.frames().len(), 1);
            assert_eq!(
                ctx.trigger_next_timer(TimerHandler::handle_timer),
                Some(REPORT_DELAY_TIMER_ID)
            );
            // The report after the delay.
            assert_eq!(ctx.frames().len(), 2);
            assert_eq!(ctx.gmp_leave_group(DummyDeviceId, GROUP_ADDR), GroupLeaveResult::Left(()));
            // Our leave message.
            assert_eq!(ctx.frames().len(), 3);

            let leave_frame = &ctx.frames().last().unwrap().1;

            // Make sure it is a leave message.
            assert_eq!(leave_frame[24], 0x17);
            // Make sure the destination is ALL-ROUTERS (224.0.0.2).
            assert_eq!(leave_frame[16], 224);
            assert_eq!(leave_frame[17], 0);
            assert_eq!(leave_frame[18], 0);
            assert_eq!(leave_frame[19], 2);
            ensure_ttl_ihl_rtr(&ctx);
        });
    }

    #[test]
    fn test_igmp_integration_not_last_does_not_send_leave() {
        run_with_many_seeds(|seed| {
            let mut ctx = setup_simple_test_environment(seed);
            assert_eq!(ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR), GroupJoinResult::Joined(()));
            let now = ctx.now();
            ctx.timer_ctx().assert_timers_installed([(
                REPORT_DELAY_TIMER_ID,
                now..=(now + DEFAULT_UNSOLICITED_REPORT_INTERVAL),
            )]);
            assert_eq!(ctx.frames().len(), 1);
            receive_igmp_report(&mut ctx);
            ctx.timer_ctx().assert_no_timers_installed();
            // The report should be discarded because we have received from
            // someone else.
            assert_eq!(ctx.frames().len(), 1);
            assert_eq!(ctx.gmp_leave_group(DummyDeviceId, GROUP_ADDR), GroupLeaveResult::Left(()));
            // A leave message is not sent.
            assert_eq!(ctx.frames().len(), 1);
            ensure_ttl_ihl_rtr(&ctx);
        });
    }

    #[test]
    fn test_receive_general_query() {
        run_with_many_seeds(|seed| {
            let mut ctx = setup_simple_test_environment(seed);
            assert_eq!(ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR), GroupJoinResult::Joined(()));
            assert_eq!(
                ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR_2),
                GroupJoinResult::Joined(())
            );
            let now = ctx.now();
            let range = now..=(now + DEFAULT_UNSOLICITED_REPORT_INTERVAL);
            ctx.timer_ctx().assert_timers_installed([
                (REPORT_DELAY_TIMER_ID, range.clone()),
                (REPORT_DELAY_TIMER_ID_2, range),
            ]);
            // The initial unsolicited report.
            assert_eq!(ctx.frames().len(), 2);
            ctx.trigger_timers_and_expect_unordered(
                [REPORT_DELAY_TIMER_ID, REPORT_DELAY_TIMER_ID_2],
                TimerHandler::handle_timer,
            );
            assert_eq!(ctx.frames().len(), 4);
            const RESP_TIME: Duration = Duration::from_secs(10);
            receive_igmp_general_query(&mut ctx, RESP_TIME);
            // Two new timers should be there.
            let now = ctx.now();
            let range = now..=(now + RESP_TIME);
            ctx.timer_ctx().assert_timers_installed([
                (REPORT_DELAY_TIMER_ID, range.clone()),
                (REPORT_DELAY_TIMER_ID_2, range),
            ]);
            ctx.trigger_timers_and_expect_unordered(
                [REPORT_DELAY_TIMER_ID, REPORT_DELAY_TIMER_ID_2],
                TimerHandler::handle_timer,
            );
            // Two new reports should be sent.
            assert_eq!(ctx.frames().len(), 6);
            ensure_ttl_ihl_rtr(&ctx);
        });
    }

    #[test]
    fn test_skip_igmp() {
        run_with_many_seeds(|seed| {
            // Test that we do not perform IGMP when IGMP is disabled.

            let mut ctx = DummyCtx::default();
            ctx.seed_rng(seed);
            ctx.get_mut().igmp_enabled = false;

            // Assert that no observable effects have taken place.
            let assert_no_effect = |ctx: &DummyCtx| {
                ctx.timer_ctx().assert_no_timers_installed();
                assert_empty(ctx.frames());
            };

            assert_eq!(ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR), GroupJoinResult::Joined(()));
            // We should join the group but left in the GMP's non-member
            // state.
            assert_gmp_state!(ctx, &GROUP_ADDR, NonMember);
            assert_no_effect(&ctx);

            receive_igmp_report(&mut ctx);
            // We should have done no state transitions/work.
            assert_gmp_state!(ctx, &GROUP_ADDR, NonMember);
            assert_no_effect(&ctx);

            receive_igmp_query(&mut ctx, Duration::from_secs(10));
            // We should have done no state transitions/work.
            assert_gmp_state!(ctx, &GROUP_ADDR, NonMember);
            assert_no_effect(&ctx);

            assert_eq!(ctx.gmp_leave_group(DummyDeviceId, GROUP_ADDR), GroupLeaveResult::Left(()));
            // We should have left the group but not executed any `Actions`.
            assert!(ctx.get_ref().groups.get(&GROUP_ADDR).is_none());
            assert_no_effect(&ctx);
        });
    }

    #[test]
    fn test_igmp_integration_with_local_join_leave() {
        run_with_many_seeds(|seed| {
            // Simple IGMP integration test to check that when we call top-level
            // multicast join and leave functions, IGMP is performed.

            let mut ctx = setup_simple_test_environment(seed);

            assert_eq!(ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR), GroupJoinResult::Joined(()));
            assert_gmp_state!(ctx, &GROUP_ADDR, Delaying);
            assert_eq!(ctx.frames().len(), 1);
            let now = ctx.now();
            let range = now..=(now + DEFAULT_UNSOLICITED_REPORT_INTERVAL);
            ctx.timer_ctx().assert_timers_installed([(REPORT_DELAY_TIMER_ID, range.clone())]);
            ensure_ttl_ihl_rtr(&ctx);

            assert_eq!(
                ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR),
                GroupJoinResult::AlreadyMember
            );
            assert_gmp_state!(ctx, &GROUP_ADDR, Delaying);
            assert_eq!(ctx.frames().len(), 1);
            ctx.timer_ctx().assert_timers_installed([(REPORT_DELAY_TIMER_ID, range.clone())]);

            assert_eq!(
                ctx.gmp_leave_group(DummyDeviceId, GROUP_ADDR),
                GroupLeaveResult::StillMember
            );
            assert_gmp_state!(ctx, &GROUP_ADDR, Delaying);
            assert_eq!(ctx.frames().len(), 1);
            ctx.timer_ctx().assert_timers_installed([(REPORT_DELAY_TIMER_ID, range)]);

            assert_eq!(ctx.gmp_leave_group(DummyDeviceId, GROUP_ADDR), GroupLeaveResult::Left(()));
            assert_eq!(ctx.frames().len(), 2);
            ctx.timer_ctx().assert_no_timers_installed();
            ensure_ttl_ihl_rtr(&ctx);
        });
    }

    #[test]
    fn test_igmp_enable_disable() {
        run_with_many_seeds(|seed| {
            let mut ctx = setup_simple_test_environment(seed);
            assert_eq!(ctx.take_frames(), []);

            assert_eq!(ctx.gmp_join_group(DummyDeviceId, GROUP_ADDR), GroupJoinResult::Joined(()));
            assert_gmp_state!(ctx, &GROUP_ADDR, Delaying);
            assert_matches::assert_matches!(
                &ctx.take_frames()[..],
                [(IgmpPacketMetadata { device: DummyDeviceId, dst_ip }, frame)] => {
                    assert_eq!(dst_ip, &GROUP_ADDR);
                    let (body, src_ip, dst_ip, proto, ttl) =
                        parse_ip_packet::<Ipv4>(frame).unwrap();
                    assert_eq!(src_ip, MY_ADDR.get());
                    assert_eq!(dst_ip, GROUP_ADDR.get());
                    assert_eq!(proto, Ipv4Proto::Igmp);
                    assert_eq!(ttl, 1);
                    let mut bv = &body[..];
                    assert_matches::assert_matches!(
                        IgmpPacket::parse(&mut bv, ()).unwrap(),
                        IgmpPacket::MembershipReportV2(msg) => {
                            assert_eq!(msg.group_addr(), GROUP_ADDR.get());
                        }
                    );
                }
            );

            // Should do nothing.
            ctx.gmp_handle_maybe_enabled(DummyDeviceId);
            assert_gmp_state!(ctx, &GROUP_ADDR, Delaying);
            assert_eq!(ctx.take_frames(), []);

            // Should send done message.
            ctx.gmp_handle_disabled(DummyDeviceId);
            assert_gmp_state!(ctx, &GROUP_ADDR, NonMember);
            assert_matches::assert_matches!(
                &ctx.take_frames()[..],
                [(IgmpPacketMetadata { device: DummyDeviceId, dst_ip }, frame)] => {
                    assert_eq!(dst_ip, &Ipv4::ALL_ROUTERS_MULTICAST_ADDRESS);
                    let (body, src_ip, dst_ip, proto, ttl) =
                        parse_ip_packet::<Ipv4>(frame).unwrap();
                    assert_eq!(src_ip, MY_ADDR.get());
                    assert_eq!(dst_ip, Ipv4::ALL_ROUTERS_MULTICAST_ADDRESS.get());
                    assert_eq!(proto, Ipv4Proto::Igmp);
                    assert_eq!(ttl, 1);
                    let mut bv = &body[..];
                    assert_matches::assert_matches!(
                        IgmpPacket::parse(&mut bv, ()).unwrap(),
                        IgmpPacket::LeaveGroup(msg) => {
                            assert_eq!(msg.group_addr(), GROUP_ADDR.get());
                        }
                    );
                }
            );

            // Should do nothing.
            ctx.gmp_handle_disabled(DummyDeviceId);
            assert_gmp_state!(ctx, &GROUP_ADDR, NonMember);
            assert_eq!(ctx.take_frames(), []);

            // Should send report message.
            ctx.gmp_handle_maybe_enabled(DummyDeviceId);
            assert_gmp_state!(ctx, &GROUP_ADDR, Delaying);
            assert_matches::assert_matches!(
                &ctx.take_frames()[..],
                [(IgmpPacketMetadata { device: DummyDeviceId, dst_ip }, frame)] => {
                    assert_eq!(dst_ip, &GROUP_ADDR);
                    let (body, src_ip, dst_ip, proto, ttl) =
                        parse_ip_packet::<Ipv4>(frame).unwrap();
                    assert_eq!(src_ip, MY_ADDR.get());
                    assert_eq!(dst_ip, GROUP_ADDR.get());
                    assert_eq!(proto, Ipv4Proto::Igmp);
                    assert_eq!(ttl, 1);
                    let mut bv = &body[..];
                    assert_matches::assert_matches!(
                        IgmpPacket::parse(&mut bv, ()).unwrap(),
                        IgmpPacket::MembershipReportV2(msg) => {
                            assert_eq!(msg.group_addr(), GROUP_ADDR.get());
                        }
                    );
                }
            );
        });
    }

    #[test]
    fn test_igmp_enable_disable_integration() {
        let DummyEventDispatcherConfig {
            local_mac,
            remote_mac: _,
            local_ip: _,
            remote_ip: _,
            subnet: _,
        } = Ipv4::DUMMY_CONFIG;

        let mut ctx: crate::testutil::DummyCtx = Ctx::new(
            StackStateBuilder::default().build(),
            DummyEventDispatcher::default(),
            Default::default(),
        );
        let device_id =
            ctx.state.device.add_ethernet_device(local_mac, Ipv4::MINIMUM_LINK_MTU.into());
        crate::ip::device::add_ipv4_addr_subnet(
            &mut ctx,
            &mut (),
            device_id,
            AddrSubnet::new(MY_ADDR.get(), 24).unwrap(),
        )
        .unwrap();
        ctx.ctx.timer_ctx().assert_no_timers_installed();

        let now = ctx.now();
        let timer_id = TimerId(TimerIdInner::Ipv4Device(
            IgmpTimerId::Gmp(GmpDelayedReportTimerId {
                device: device_id,
                group_addr: Ipv4::ALL_SYSTEMS_MULTICAST_ADDRESS,
            })
            .into(),
        ));
        let range = now..=(now + DEFAULT_UNSOLICITED_REPORT_INTERVAL);
        struct TestConfig {
            ip_enabled: bool,
            gmp_enabled: bool,
        }

        let set_config = |ctx: &mut crate::testutil::DummyCtx,
                          TestConfig { ip_enabled, gmp_enabled }| {
            crate::ip::device::update_ipv4_configuration(ctx, &mut (), device_id, |config| {
                config.ip_config.ip_enabled = ip_enabled;
                config.ip_config.gmp_enabled = gmp_enabled;
            });
        };
        let check_sent_report = |ctx: &mut crate::testutil::DummyCtx| {
            assert_matches::assert_matches!(
                &ctx.dispatcher.take_frames()[..],
                [(egress_device, frame)] => {
                    assert_eq!(egress_device, &device_id);
                    let (body, src_mac, dst_mac, src_ip, dst_ip, proto, ttl) =
                        parse_ip_packet_in_ethernet_frame::<Ipv4>(frame).unwrap();
                    assert_eq!(src_mac, local_mac.get());
                    assert_eq!(dst_mac, Mac::from(&Ipv4::ALL_SYSTEMS_MULTICAST_ADDRESS));
                    assert_eq!(src_ip, MY_ADDR.get());
                    assert_eq!(dst_ip, Ipv4::ALL_SYSTEMS_MULTICAST_ADDRESS.get());
                    assert_eq!(proto, Ipv4Proto::Igmp);
                    assert_eq!(ttl, 1);
                    let mut bv = &body[..];
                    assert_matches::assert_matches!(
                        IgmpPacket::parse(&mut bv, ()).unwrap(),
                        IgmpPacket::MembershipReportV2(msg) => {
                            assert_eq!(msg.group_addr(), Ipv4::ALL_SYSTEMS_MULTICAST_ADDRESS.get());
                        }
                    );
                }
            );
        };
        let check_sent_leave = |ctx: &mut crate::testutil::DummyCtx| {
            assert_matches::assert_matches!(
                &ctx.dispatcher.take_frames()[..],
                [(egress_device, frame)] => {
                    assert_eq!(egress_device, &device_id);
                    let (body, src_mac, dst_mac, src_ip, dst_ip, proto, ttl) =
                        parse_ip_packet_in_ethernet_frame::<Ipv4>(frame).unwrap();
                    assert_eq!(src_mac, local_mac.get());
                    assert_eq!(dst_mac, Mac::from(&Ipv4::ALL_ROUTERS_MULTICAST_ADDRESS));
                    assert_eq!(src_ip, MY_ADDR.get());
                    assert_eq!(dst_ip, Ipv4::ALL_ROUTERS_MULTICAST_ADDRESS.get());
                    assert_eq!(proto, Ipv4Proto::Igmp);
                    assert_eq!(ttl, 1);
                    let mut bv = &body[..];
                    assert_matches::assert_matches!(
                        IgmpPacket::parse(&mut bv, ()).unwrap(),
                        IgmpPacket::LeaveGroup(msg) => {
                            assert_eq!(msg.group_addr(), Ipv4::ALL_SYSTEMS_MULTICAST_ADDRESS.get());
                        }
                    );
                }
            );
        };

        // Enable IPv4 and IGMP.
        //
        // Should send report for the all-systems multicast group that all
        // interfaces join.
        set_config(&mut ctx, TestConfig { ip_enabled: true, gmp_enabled: true });
        ctx.ctx.timer_ctx().assert_timers_installed([(timer_id, range.clone())]);
        check_sent_report(&mut ctx);

        // Disable IGMP.
        set_config(&mut ctx, TestConfig { ip_enabled: true, gmp_enabled: false });
        ctx.ctx.timer_ctx().assert_no_timers_installed();
        check_sent_leave(&mut ctx);

        // Enable IGMP but disable IPv4.
        //
        // Should do nothing.
        set_config(&mut ctx, TestConfig { ip_enabled: false, gmp_enabled: true });
        ctx.ctx.timer_ctx().assert_no_timers_installed();
        assert_matches::assert_matches!(&ctx.dispatcher.take_frames()[..], []);

        // Disable IGMP but enable IPv4.
        //
        // Should do nothing.
        set_config(&mut ctx, TestConfig { ip_enabled: true, gmp_enabled: false });
        ctx.ctx.timer_ctx().assert_no_timers_installed();
        assert_matches::assert_matches!(&ctx.dispatcher.take_frames()[..], []);

        // Enable IGMP.
        set_config(&mut ctx, TestConfig { ip_enabled: true, gmp_enabled: true });
        ctx.ctx.timer_ctx().assert_timers_installed([(timer_id, range.clone())]);
        check_sent_report(&mut ctx);

        // Disable IPv4.
        set_config(&mut ctx, TestConfig { ip_enabled: false, gmp_enabled: true });
        ctx.ctx.timer_ctx().assert_no_timers_installed();
        check_sent_leave(&mut ctx);

        // Enable IPv4.
        set_config(&mut ctx, TestConfig { ip_enabled: true, gmp_enabled: true });
        ctx.ctx.timer_ctx().assert_timers_installed([(timer_id, range.clone())]);
        check_sent_report(&mut ctx);
    }
}
