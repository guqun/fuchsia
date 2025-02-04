// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! IPv6 Route Discovery as defined by [RFC 4861 section 6.3.4].
//!
//! [RFC 4861 section 6.3.4]: https://datatracker.ietf.org/doc/html/rfc4861#section-6.3.4

use core::hash::Hash;

use fakealloc::collections::HashSet;
use net_types::{
    ip::{Ipv6, Ipv6Addr, Subnet},
    LinkLocalUnicastAddr,
};
use packet_formats::icmp::ndp::NonZeroNdpLifetime;

use crate::{
    context::{EventContext, TimerContext, TimerHandler},
    ip::IpDeviceIdContext,
};

#[derive(Default)]
#[cfg_attr(test, derive(Debug, Eq, PartialEq))]
pub(super) struct Ipv6RouteDiscoveryState {
    // The valid (non-zero lifetime) discovered routes.
    //
    // Routes with a finite lifetime must have a timer set; routes with an
    // infinite lifetime must not.
    routes: HashSet<Ipv6DiscoveredRoute>,
}

/// A discovered route.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash)]
pub struct Ipv6DiscoveredRoute {
    /// The destination subnet for the route.
    pub subnet: Subnet<Ipv6Addr>,

    /// The next-hop node for the route, if required.
    ///
    /// `None` indicates that the subnet is on-link/directly-connected.
    pub gateway: Option<LinkLocalUnicastAddr<Ipv6Addr>>,
}

/// The action taken on a route.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash)]
pub enum Ipv6RouteDiscoverAction {
    /// Indicates that a route was newly discovered.
    Discovered,

    /// Indicates that a previously discovered route was invalidated.
    Invalidated,
}

/// An IPv6 route discovery event.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash)]
pub struct Ipv6RouteDiscoveryEvent<DeviceId> {
    /// The device ID for the event.
    pub device_id: DeviceId,

    /// The route triggering the event.
    pub route: Ipv6DiscoveredRoute,

    /// The change on the route.
    pub action: Ipv6RouteDiscoverAction,
}

/// A timer ID for IPv6 route discovery.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash)]
pub(crate) struct Ipv6DiscoveredRouteTimerId<DeviceId> {
    device_id: DeviceId,
    route: Ipv6DiscoveredRoute,
}

/// The state context provided to IPv6 route discovery.
pub(super) trait Ipv6RouteDiscoveryStateContext: IpDeviceIdContext<Ipv6> {
    /// Gets the route discovery state, mutably.
    fn get_discovered_routes_mut(
        &mut self,
        device_id: Self::DeviceId,
    ) -> &mut Ipv6RouteDiscoveryState;
}

/// The execution context for IPv6 route discovery.
trait Ipv6RouteDiscoveryContext:
    Ipv6RouteDiscoveryStateContext
    + TimerContext<Ipv6DiscoveredRouteTimerId<Self::DeviceId>>
    + EventContext<Ipv6RouteDiscoveryEvent<Self::DeviceId>>
{
}

impl<
        C: Ipv6RouteDiscoveryStateContext
            + TimerContext<Ipv6DiscoveredRouteTimerId<Self::DeviceId>>
            + EventContext<Ipv6RouteDiscoveryEvent<Self::DeviceId>>,
    > Ipv6RouteDiscoveryContext for C
{
}

/// An implementation of IPv6 route discovery.
pub(crate) trait RouteDiscoveryHandler: IpDeviceIdContext<Ipv6> {
    /// Handles an update affecting discovered routes.
    ///
    /// A `None` value for `lifetime` indicates that the route is not valid and
    /// must be invalidated if it has been discovered; a `Some(_)` value
    /// indicates the new maximum lifetime that the route may be valid for
    /// before being invalidated.
    fn update_route(
        &mut self,
        device_id: Self::DeviceId,
        route: Ipv6DiscoveredRoute,
        lifetime: Option<NonZeroNdpLifetime>,
    );

    /// Invalidates all discovered routes.
    fn invalidate_routes(&mut self, device_id: Self::DeviceId);
}

impl<C: Ipv6RouteDiscoveryContext> RouteDiscoveryHandler for C {
    fn update_route(
        &mut self,
        device_id: C::DeviceId,
        route: Ipv6DiscoveredRoute,
        lifetime: Option<NonZeroNdpLifetime>,
    ) {
        let Ipv6RouteDiscoveryState { routes } = self.get_discovered_routes_mut(device_id);

        match lifetime {
            Some(lifetime) => {
                let newly_added = routes.insert(route.clone());
                let timer_id = Ipv6DiscoveredRouteTimerId { device_id, route };
                let prev_timer_fires_at: Option<C::Instant> = match lifetime {
                    NonZeroNdpLifetime::Finite(lifetime) => {
                        self.schedule_timer(lifetime.get(), timer_id)
                    }
                    // Routes with an infinite lifetime have no timers
                    //
                    // TODO(https://fxbug.dev/97751): Hold timers scheduled to
                    // fire at infinity.
                    NonZeroNdpLifetime::Infinite => self.cancel_timer(timer_id),
                };
                if newly_added {
                    if let Some(prev_timer_fires_at) = prev_timer_fires_at {
                        panic!("newly added timer ID {:?} should not have already been scheduled to fire at {:?}", timer_id, prev_timer_fires_at);
                    }

                    send_event(self, device_id, route, Ipv6RouteDiscoverAction::Discovered);
                }
            }
            None => {
                if routes.remove(&route) {
                    invalidate_route(self, device_id, route);
                }
            }
        }
    }

    fn invalidate_routes(&mut self, device_id: C::DeviceId) {
        let Ipv6RouteDiscoveryState { routes } = self.get_discovered_routes_mut(device_id);
        for route in core::mem::take(routes).into_iter() {
            invalidate_route(self, device_id, route);
        }
    }
}

impl<C: Ipv6RouteDiscoveryContext> TimerHandler<Ipv6DiscoveredRouteTimerId<C::DeviceId>> for C {
    fn handle_timer(
        &mut self,
        Ipv6DiscoveredRouteTimerId { device_id, route }: Ipv6DiscoveredRouteTimerId<C::DeviceId>,
    ) {
        let Ipv6RouteDiscoveryState { routes } = self.get_discovered_routes_mut(device_id);
        assert!(routes.remove(&route), "invalidated route should be discovered");
        send_event(self, device_id, route, Ipv6RouteDiscoverAction::Invalidated);
    }
}

fn invalidate_route<C: Ipv6RouteDiscoveryContext>(
    ctx: &mut C,
    device_id: C::DeviceId,
    route: Ipv6DiscoveredRoute,
) {
    // Routes with an infinite lifetime have no timers.
    //
    // TODO(https://fxbug.dev/97751): Hold timers scheduled to fire at infinity.
    let _: Option<C::Instant> = ctx.cancel_timer(Ipv6DiscoveredRouteTimerId { device_id, route });
    send_event(ctx, device_id, route, Ipv6RouteDiscoverAction::Invalidated);
}

fn send_event<C: Ipv6RouteDiscoveryContext>(
    ctx: &mut C,
    device_id: C::DeviceId,
    route: Ipv6DiscoveredRoute,
    action: Ipv6RouteDiscoverAction,
) {
    ctx.on_event(Ipv6RouteDiscoveryEvent { device_id, route, action })
}

#[cfg(test)]
mod tests {
    use core::{
        convert::{AsMut, TryInto as _},
        num::NonZeroU64,
        time::Duration,
    };

    use net_types::{ip::Ip as _, Witness as _};
    use packet::{BufferMut, InnerPacketBuilder as _, Serializer as _};
    use packet_formats::{
        icmp::{
            ndp::{
                options::{NdpOptionBuilder, PrefixInformation},
                OptionSequenceBuilder, RouterAdvertisement,
            },
            IcmpPacketBuilder, IcmpUnusedCode,
        },
        ip::Ipv6Proto,
        ipv6::Ipv6PacketBuilder,
        utils::NonZeroDuration,
    };

    use super::*;
    use crate::{
        context::testutil::{DummyCtx, DummyEventCtx, DummyInstant, DummyTimerCtxExt as _},
        device::FrameDestination,
        ip::{device::Ipv6DeviceTimerId, receive_ipv6_packet, DummyDeviceId, IPV6_DEFAULT_SUBNET},
        testutil::{DummyEventDispatcher, DummyEventDispatcherConfig, TestIpExt as _},
        Ctx, DeviceId, StackStateBuilder, TimerId, TimerIdInner,
    };

    #[derive(Default)]
    struct MockIpv6RouteDiscoveryContext {
        state: Ipv6RouteDiscoveryState,
    }

    type MockCtx = DummyCtx<
        MockIpv6RouteDiscoveryContext,
        Ipv6DiscoveredRouteTimerId<DummyDeviceId>,
        (),
        Ipv6RouteDiscoveryEvent<DummyDeviceId>,
        DummyDeviceId,
    >;

    impl Ipv6RouteDiscoveryStateContext for MockCtx {
        fn get_discovered_routes_mut(
            &mut self,
            DummyDeviceId: Self::DeviceId,
        ) -> &mut Ipv6RouteDiscoveryState {
            let MockIpv6RouteDiscoveryContext { state } = self.get_mut();
            state
        }
    }

    const ROUTE1: Ipv6DiscoveredRoute =
        Ipv6DiscoveredRoute { subnet: IPV6_DEFAULT_SUBNET, gateway: None };
    const ROUTE2: Ipv6DiscoveredRoute = Ipv6DiscoveredRoute {
        subnet: unsafe {
            Subnet::new_unchecked(Ipv6Addr::new([0x2620, 0x1012, 0x1000, 0x5000, 0, 0, 0, 0]), 64)
        },
        gateway: None,
    };

    const ONE_SECOND: NonZeroDuration =
        NonZeroDuration::from_nonzero_secs(const_unwrap::const_unwrap_option(NonZeroU64::new(1)));
    const TWO_SECONDS: NonZeroDuration =
        NonZeroDuration::from_nonzero_secs(const_unwrap::const_unwrap_option(NonZeroU64::new(2)));

    #[test]
    fn new_route_no_lifetime() {
        let mut ctx = MockCtx::default();

        RouteDiscoveryHandler::update_route(&mut ctx, DummyDeviceId, ROUTE1, None);
        assert_eq!(ctx.take_events(), []);
        ctx.timer_ctx().assert_no_timers_installed();
    }

    fn discover_new_route(
        ctx: &mut MockCtx,
        route: Ipv6DiscoveredRoute,
        duration: NonZeroNdpLifetime,
    ) {
        RouteDiscoveryHandler::update_route(ctx, DummyDeviceId, route, Some(duration));
        assert_eq!(
            ctx.take_events(),
            [Ipv6RouteDiscoveryEvent {
                device_id: DummyDeviceId,
                route,
                action: Ipv6RouteDiscoverAction::Discovered
            }]
        );

        ctx.timer_ctx().assert_some_timers_installed(
            match duration {
                NonZeroNdpLifetime::Finite(duration) => Some((
                    Ipv6DiscoveredRouteTimerId { device_id: DummyDeviceId, route },
                    DummyInstant::from(duration.get()),
                )),
                NonZeroNdpLifetime::Infinite => None,
            }
            .into_iter(),
        )
    }

    fn assert_single_invalidation_timer(ctx: &mut MockCtx, route: Ipv6DiscoveredRoute) {
        assert_eq!(
            ctx.trigger_next_timer(TimerHandler::handle_timer),
            Some(Ipv6DiscoveredRouteTimerId { device_id: DummyDeviceId, route })
        );
        assert_eq!(
            ctx.take_events(),
            [Ipv6RouteDiscoveryEvent {
                device_id: DummyDeviceId,
                route,
                action: Ipv6RouteDiscoverAction::Invalidated
            }]
        );
        ctx.timer_ctx().assert_no_timers_installed();
    }

    #[test]
    fn new_route_with_infinite_lifetime() {
        let mut ctx = MockCtx::default();

        discover_new_route(&mut ctx, ROUTE1, NonZeroNdpLifetime::Infinite);
        ctx.timer_ctx().assert_no_timers_installed();
    }

    #[test]
    fn update_route_from_infinite_to_finite_lifetime() {
        let mut ctx = MockCtx::default();

        discover_new_route(&mut ctx, ROUTE1, NonZeroNdpLifetime::Infinite);
        ctx.timer_ctx().assert_no_timers_installed();

        RouteDiscoveryHandler::update_route(
            &mut ctx,
            DummyDeviceId,
            ROUTE1,
            Some(NonZeroNdpLifetime::Finite(ONE_SECOND)),
        );
        assert_eq!(ctx.take_events(), []);
        ctx.timer_ctx().assert_some_timers_installed([(
            Ipv6DiscoveredRouteTimerId { device_id: DummyDeviceId, route: ROUTE1 },
            DummyInstant::from(ONE_SECOND.get()),
        )]);
        assert_single_invalidation_timer(&mut ctx, ROUTE1);
    }

    fn update_to_invalidate_check_invalidation(ctx: &mut MockCtx, route: Ipv6DiscoveredRoute) {
        RouteDiscoveryHandler::update_route(ctx, DummyDeviceId, ROUTE1, None);
        assert_eq!(
            ctx.take_events(),
            [Ipv6RouteDiscoveryEvent {
                device_id: DummyDeviceId,
                route,
                action: Ipv6RouteDiscoverAction::Invalidated
            }]
        );
        ctx.timer_ctx().assert_no_timers_installed();
    }

    #[test]
    fn invalidate_route_with_infinite_lifetime() {
        let mut ctx = MockCtx::default();

        discover_new_route(&mut ctx, ROUTE1, NonZeroNdpLifetime::Infinite);
        ctx.timer_ctx().assert_no_timers_installed();

        update_to_invalidate_check_invalidation(&mut ctx, ROUTE1);
    }
    #[test]
    fn new_route_with_finite_lifetime() {
        let mut ctx = MockCtx::default();

        discover_new_route(&mut ctx, ROUTE1, NonZeroNdpLifetime::Finite(ONE_SECOND));
        assert_single_invalidation_timer(&mut ctx, ROUTE1);
    }

    #[test]
    fn update_route_from_finite_to_infinite_lifetime() {
        let mut ctx = MockCtx::default();

        discover_new_route(&mut ctx, ROUTE1, NonZeroNdpLifetime::Finite(ONE_SECOND));

        RouteDiscoveryHandler::update_route(
            &mut ctx,
            DummyDeviceId,
            ROUTE1,
            Some(NonZeroNdpLifetime::Infinite),
        );
        assert_eq!(ctx.take_events(), []);
        ctx.timer_ctx().assert_no_timers_installed();
    }

    #[test]
    fn update_route_from_finite_to_finite_lifetime() {
        let mut ctx = MockCtx::default();

        discover_new_route(&mut ctx, ROUTE1, NonZeroNdpLifetime::Finite(ONE_SECOND));

        RouteDiscoveryHandler::update_route(
            &mut ctx,
            DummyDeviceId,
            ROUTE1,
            Some(NonZeroNdpLifetime::Finite(TWO_SECONDS)),
        );
        assert_eq!(ctx.take_events(), []);
        ctx.timer_ctx().assert_timers_installed([(
            Ipv6DiscoveredRouteTimerId { device_id: DummyDeviceId, route: ROUTE1 },
            DummyInstant::from(TWO_SECONDS.get()),
        )]);

        assert_single_invalidation_timer(&mut ctx, ROUTE1);
    }

    #[test]
    fn invalidate_route_with_finite_lifetime() {
        let mut ctx = MockCtx::default();

        discover_new_route(&mut ctx, ROUTE1, NonZeroNdpLifetime::Finite(ONE_SECOND));

        update_to_invalidate_check_invalidation(&mut ctx, ROUTE1);
    }

    #[test]
    fn invalidate_all_routes() {
        let mut ctx = MockCtx::default();
        discover_new_route(&mut ctx, ROUTE1, NonZeroNdpLifetime::Finite(ONE_SECOND));
        discover_new_route(&mut ctx, ROUTE2, NonZeroNdpLifetime::Finite(TWO_SECONDS));

        RouteDiscoveryHandler::invalidate_routes(&mut ctx, DummyDeviceId);
        assert_eq!(
            ctx.take_events().into_iter().collect::<HashSet<_>>(),
            HashSet::from([
                Ipv6RouteDiscoveryEvent {
                    device_id: DummyDeviceId,
                    route: ROUTE1,
                    action: Ipv6RouteDiscoverAction::Invalidated
                },
                Ipv6RouteDiscoveryEvent {
                    device_id: DummyDeviceId,
                    route: ROUTE2,
                    action: Ipv6RouteDiscoverAction::Invalidated
                },
            ])
        );
        ctx.timer_ctx().assert_no_timers_installed();
    }

    fn router_advertisement_buf(
        src_ip: LinkLocalUnicastAddr<Ipv6Addr>,
        router_lifetime_secs: u16,
        on_link_prefix: Subnet<Ipv6Addr>,
        on_link_prefix_flag: bool,
        on_link_prefix_valid_lifetime_secs: u32,
    ) -> impl BufferMut {
        let src_ip: Ipv6Addr = src_ip.get();
        let dst_ip = Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS.get();
        let p = PrefixInformation::new(
            on_link_prefix.prefix(),
            on_link_prefix_flag,
            false, /* autonomous_address_configuration_flag */
            on_link_prefix_valid_lifetime_secs,
            0, /* preferred_lifetime */
            on_link_prefix.network(),
        );
        let options = &[NdpOptionBuilder::PrefixInformation(p)];
        OptionSequenceBuilder::new(options.iter())
            .into_serializer()
            .encapsulate(IcmpPacketBuilder::<Ipv6, &[u8], _>::new(
                src_ip,
                dst_ip,
                IcmpUnusedCode,
                RouterAdvertisement::new(
                    0,     /* hop_limit */
                    false, /* managed_flag */
                    false, /* other_config_flag */
                    router_lifetime_secs,
                    0, /* reachable_time */
                    0, /* retransmit_timer */
                ),
            ))
            .encapsulate(Ipv6PacketBuilder::new(
                src_ip,
                dst_ip,
                crate::ip::device::integration::REQUIRED_NDP_IP_PACKET_HOP_LIMIT,
                Ipv6Proto::Icmpv6,
            ))
            .serialize_vec_outer()
            .unwrap()
            .unwrap_b()
    }

    fn setup() -> (crate::testutil::DummyCtx, DeviceId, DummyEventDispatcherConfig<Ipv6Addr>) {
        let DummyEventDispatcherConfig {
            local_mac,
            remote_mac: _,
            local_ip: _,
            remote_ip: _,
            subnet: _,
        } = Ipv6::DUMMY_CONFIG;

        let mut ctx: crate::testutil::DummyCtx = Ctx::new(
            {
                let mut stack_builder = StackStateBuilder::default();

                let mut ipv6_config = crate::ip::device::state::Ipv6DeviceConfiguration::default();
                ipv6_config.dad_transmits = None;
                ipv6_config.max_router_solicitations = None;
                stack_builder.device_builder().set_default_ipv6_config(ipv6_config);

                stack_builder.build()
            },
            DummyEventDispatcher::default(),
            Default::default(),
        );
        let device_id =
            ctx.state.device.add_ethernet_device(local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::ip::device::update_ipv6_configuration(&mut ctx, &mut (), device_id, |config| {
            config.ip_config.ip_enabled = true;
        });

        ctx.ctx.timer_ctx().assert_no_timers_installed();

        (ctx, device_id, Ipv6::DUMMY_CONFIG)
    }

    fn as_secs(d: NonZeroDuration) -> u16 {
        d.get().as_secs().try_into().unwrap()
    }

    fn timer_id(route: Ipv6DiscoveredRoute, device_id: DeviceId) -> TimerId {
        TimerId(TimerIdInner::Ipv6Device(Ipv6DeviceTimerId::RouteDiscovery(
            Ipv6DiscoveredRouteTimerId { device_id, route },
        )))
    }

    #[test]
    fn discovery_integration() {
        let (
            mut ctx,
            device_id,
            DummyEventDispatcherConfig {
                local_mac: _,
                remote_mac,
                local_ip: _,
                remote_ip: _,
                subnet,
            },
        ) = setup();
        let ctx = &mut ctx;

        let src_ip = remote_mac.to_ipv6_link_local().addr();

        let buf = |router_lifetime_secs, on_link_prefix_flag, prefix_valid_lifetime_secs| {
            router_advertisement_buf(
                src_ip,
                router_lifetime_secs,
                subnet,
                on_link_prefix_flag,
                prefix_valid_lifetime_secs,
            )
        };

        let timer_id = |route| timer_id(route, device_id);

        let check_event = |ctx: &mut crate::testutil::DummyCtx,
                           event: Option<Ipv6RouteDiscoveryEvent<_>>| {
            assert_eq!(
                AsMut::<DummyEventCtx<_>>::as_mut(ctx).take().into_iter().collect::<HashSet<_>>(),
                event.map_or_else(HashSet::default, |e| HashSet::from([e.into()])),
            );
        };

        // Do nothing as router with no valid lifetime has not been discovered
        // yet and prefix does not make on-link determination.
        receive_ipv6_packet(
            ctx,
            device_id,
            FrameDestination::Unicast,
            buf(0, false, as_secs(ONE_SECOND).into()),
        );
        check_event(ctx, None);
        ctx.ctx.timer_ctx().assert_no_timers_installed();

        // Discover a default router only as on-link prefix has no valid
        // lifetime.
        receive_ipv6_packet(
            ctx,
            device_id,
            FrameDestination::Unicast,
            buf(as_secs(ONE_SECOND), true, 0),
        );
        let gateway_route =
            Ipv6DiscoveredRoute { subnet: IPV6_DEFAULT_SUBNET, gateway: Some(src_ip) };
        check_event(
            ctx,
            Some(Ipv6RouteDiscoveryEvent {
                device_id,
                route: gateway_route,
                action: Ipv6RouteDiscoverAction::Discovered,
            }),
        );
        ctx.ctx.timer_ctx().assert_timers_installed([(
            timer_id(gateway_route),
            DummyInstant::from(ONE_SECOND.get()),
        )]);

        // Discover an on-link prefix and update valid lifetime for default
        // router.
        receive_ipv6_packet(
            ctx,
            device_id,
            FrameDestination::Unicast,
            buf(as_secs(TWO_SECONDS), true, as_secs(ONE_SECOND).into()),
        );
        let on_link_route = Ipv6DiscoveredRoute { subnet, gateway: None };
        check_event(
            ctx,
            Some(Ipv6RouteDiscoveryEvent {
                device_id,
                route: on_link_route,
                action: Ipv6RouteDiscoverAction::Discovered,
            }),
        );
        ctx.ctx.timer_ctx().assert_timers_installed([
            (timer_id(gateway_route), DummyInstant::from(TWO_SECONDS.get())),
            (timer_id(on_link_route), DummyInstant::from(ONE_SECOND.get())),
        ]);

        // Invalidate default router and update valid lifetime for on-link
        // prefix.
        receive_ipv6_packet(
            ctx,
            device_id,
            FrameDestination::Unicast,
            buf(0, true, as_secs(TWO_SECONDS).into()),
        );
        check_event(
            ctx,
            Some(Ipv6RouteDiscoveryEvent {
                device_id,
                route: gateway_route,
                action: Ipv6RouteDiscoverAction::Invalidated,
            }),
        );
        ctx.ctx.timer_ctx().assert_timers_installed([(
            timer_id(on_link_route),
            DummyInstant::from(TWO_SECONDS.get()),
        )]);

        // Do nothing as prefix does not make on-link determination and router
        // with valid lifetime is not discovered.
        receive_ipv6_packet(ctx, device_id, FrameDestination::Unicast, buf(0, false, 0));
        check_event(ctx, None);
        ctx.ctx.timer_ctx().assert_timers_installed([(
            timer_id(on_link_route),
            DummyInstant::from(TWO_SECONDS.get()),
        )]);

        // Invalidate on-link prefix.
        receive_ipv6_packet(ctx, device_id, FrameDestination::Unicast, buf(0, true, 0));
        check_event(
            ctx,
            Some(Ipv6RouteDiscoveryEvent {
                device_id,
                route: on_link_route,
                action: Ipv6RouteDiscoverAction::Invalidated,
            }),
        );
        ctx.ctx.timer_ctx().assert_no_timers_installed();
    }

    #[test]
    fn discovery_integration_infinite_to_finite_to_infinite_lifetime() {
        let (
            mut ctx,
            device_id,
            DummyEventDispatcherConfig {
                local_mac: _,
                remote_mac,
                local_ip: _,
                remote_ip: _,
                subnet,
            },
        ) = setup();
        let ctx = &mut ctx;

        let src_ip = remote_mac.to_ipv6_link_local().addr();

        let buf = |router_lifetime_secs, on_link_prefix_flag, prefix_valid_lifetime_secs| {
            router_advertisement_buf(
                src_ip,
                router_lifetime_secs,
                subnet,
                on_link_prefix_flag,
                prefix_valid_lifetime_secs,
            )
        };

        let timer_id = |route| timer_id(route, device_id);

        let gateway_route =
            Ipv6DiscoveredRoute { subnet: IPV6_DEFAULT_SUBNET, gateway: Some(src_ip) };
        let on_link_route = Ipv6DiscoveredRoute { subnet, gateway: None };

        // Router with finite lifetime and on-link prefix with infinite
        // lifetime.
        let router_lifetime_secs = u16::MAX;
        let prefix_lifetime_secs = u32::MAX;
        receive_ipv6_packet(
            ctx,
            device_id,
            FrameDestination::Unicast,
            buf(router_lifetime_secs, true, prefix_lifetime_secs),
        );
        assert_eq!(
            AsMut::<DummyEventCtx<_>>::as_mut(ctx).take().into_iter().collect::<HashSet<_>>(),
            HashSet::from([
                Ipv6RouteDiscoveryEvent {
                    device_id,
                    route: gateway_route,
                    action: Ipv6RouteDiscoverAction::Discovered,
                }
                .into(),
                Ipv6RouteDiscoveryEvent {
                    device_id,
                    route: on_link_route,
                    action: Ipv6RouteDiscoverAction::Discovered,
                }
                .into(),
            ]),
        );
        ctx.ctx.timer_ctx().assert_timers_installed([(
            timer_id(gateway_route),
            DummyInstant::from(Duration::from_secs(router_lifetime_secs.into())),
        )]);

        // Router and prefix with finite lifetimes.
        let router_lifetime_secs = u16::MAX - 1;
        let prefix_lifetime_secs = u32::MAX - 1;
        receive_ipv6_packet(
            ctx,
            device_id,
            FrameDestination::Unicast,
            buf(router_lifetime_secs, true, prefix_lifetime_secs),
        );
        ctx.ctx.timer_ctx().assert_timers_installed([
            (
                timer_id(gateway_route),
                DummyInstant::from(Duration::from_secs(router_lifetime_secs.into())),
            ),
            (
                timer_id(on_link_route),
                DummyInstant::from(Duration::from_secs(prefix_lifetime_secs.into())),
            ),
        ]);

        // Router with finite lifetime and on-link prefix with infinite
        // lifetime.
        let router_lifetime_secs = u16::MAX;
        let prefix_lifetime_secs = u32::MAX;
        receive_ipv6_packet(
            ctx,
            device_id,
            FrameDestination::Unicast,
            buf(router_lifetime_secs, true, prefix_lifetime_secs),
        );
        ctx.ctx.timer_ctx().assert_timers_installed([(
            timer_id(gateway_route),
            DummyInstant::from(Duration::from_secs(router_lifetime_secs.into())),
        )]);

        // Router and prefix invalidated.
        let router_lifetime_secs = 0;
        let prefix_lifetime_secs = 0;
        receive_ipv6_packet(
            ctx,
            device_id,
            FrameDestination::Unicast,
            buf(router_lifetime_secs, true, prefix_lifetime_secs),
        );
        assert_eq!(
            AsMut::<DummyEventCtx<_>>::as_mut(ctx).take().into_iter().collect::<HashSet<_>>(),
            HashSet::from([
                Ipv6RouteDiscoveryEvent {
                    device_id,
                    route: gateway_route,
                    action: Ipv6RouteDiscoverAction::Invalidated,
                }
                .into(),
                Ipv6RouteDiscoveryEvent {
                    device_id,
                    route: on_link_route,
                    action: Ipv6RouteDiscoverAction::Invalidated,
                }
                .into(),
            ]),
        );
        ctx.ctx.timer_ctx().assert_no_timers_installed();
    }

    #[test]
    fn flush_routes_on_interface_disabled_integration() {
        let (
            mut ctx,
            device_id,
            DummyEventDispatcherConfig {
                local_mac: _,
                remote_mac,
                local_ip: _,
                remote_ip: _,
                subnet,
            },
        ) = setup();
        let ctx = &mut ctx;

        let src_ip = remote_mac.to_ipv6_link_local().addr();
        let gateway_route =
            Ipv6DiscoveredRoute { subnet: IPV6_DEFAULT_SUBNET, gateway: Some(src_ip) };
        let on_link_route = Ipv6DiscoveredRoute { subnet, gateway: None };

        let timer_id = |route| timer_id(route, device_id);

        // Discover both an on-link prefix and default router.
        receive_ipv6_packet(
            ctx,
            device_id,
            FrameDestination::Unicast,
            router_advertisement_buf(
                src_ip,
                as_secs(TWO_SECONDS),
                subnet,
                true,
                as_secs(ONE_SECOND).into(),
            ),
        );
        assert_eq!(
            AsMut::<DummyEventCtx<_>>::as_mut(ctx).take().into_iter().collect::<HashSet<_>>(),
            HashSet::from([
                Ipv6RouteDiscoveryEvent {
                    device_id,
                    route: gateway_route,
                    action: Ipv6RouteDiscoverAction::Discovered,
                }
                .into(),
                Ipv6RouteDiscoveryEvent {
                    device_id,
                    route: on_link_route,
                    action: Ipv6RouteDiscoverAction::Discovered,
                }
                .into(),
            ]),
        );
        ctx.ctx.timer_ctx().assert_timers_installed([
            (timer_id(gateway_route), DummyInstant::from(TWO_SECONDS.get())),
            (timer_id(on_link_route), DummyInstant::from(ONE_SECOND.get())),
        ]);

        // Disable the interface.
        crate::ip::device::update_ipv6_configuration(ctx, &mut (), device_id, |config| {
            config.ip_config.ip_enabled = false;
        });
        assert_eq!(
            AsMut::<DummyEventCtx<_>>::as_mut(ctx).take().into_iter().collect::<HashSet<_>>(),
            HashSet::from([
                Ipv6RouteDiscoveryEvent {
                    device_id,
                    route: gateway_route,
                    action: Ipv6RouteDiscoverAction::Invalidated,
                }
                .into(),
                Ipv6RouteDiscoveryEvent {
                    device_id,
                    route: on_link_route,
                    action: Ipv6RouteDiscoverAction::Invalidated,
                }
                .into(),
            ]),
        );
        ctx.ctx.timer_ctx().assert_no_timers_installed();
    }
}
