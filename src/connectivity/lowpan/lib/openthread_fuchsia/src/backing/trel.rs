// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use crate::to_escaped_string::*;
use anyhow::Context as _;
use fidl::endpoints::create_endpoints;
use fidl_fuchsia_net_mdns::*;
use fuchsia_async::Task;
use futures::stream::FusedStream;
use openthread_sys::*;
use ot::PlatTrel as _;
use std::collections::HashMap;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::task::{Context, Poll};

pub(crate) struct TrelInstance {
    socket: fasync::net::UdpSocket,
    publication_responder: Option<Task<Result<(), anyhow::Error>>>,
    instance_name: String,
    peer_instance_sockaddr_map: HashMap<String, ot::SockAddr>,
    subscriber_request_stream: ServiceSubscriberRequestStream,
}

// Converts an optional vector of strings and to a single string.
fn flatten_txt(txt: Option<Vec<Vec<u8>>>) -> Vec<u8> {
    let txt: Vec<&[u8]> = txt.iter().flat_map(|x| x.iter()).map(Vec::as_slice).collect::<Vec<_>>();

    txt.join(&ot::DNSSD_TXT_SEPARATOR_BYTE)
}

// Splits the TXT record into individual values.
fn split_txt(txt: &[u8]) -> Vec<Vec<u8>> {
    info!("trel:split_txt: Splitting TXT record: {:?}", hex::encode(txt));
    txt.split(|&x| x == ot::DNSSD_TXT_SEPARATOR_BYTE)
        .filter(|x| !x.is_empty())
        .map(|x| x.to_vec())
        .collect::<Vec<_>>()
}

impl TrelInstance {
    fn new(instance_name: String) -> Result<TrelInstance, anyhow::Error> {
        Ok(TrelInstance {
            socket: fasync::net::UdpSocket::bind(&SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::UNSPECIFIED,
                0,
                0,
                0,
            )))
            .context("Unable to open TREL UDP socket")?,
            publication_responder: None,
            instance_name,
            peer_instance_sockaddr_map: HashMap::default(),
            subscriber_request_stream: Self::make_subscriber_request_stream(),
        })
    }

    fn make_subscriber_request_stream() -> ServiceSubscriberRequestStream {
        let (client, server) = create_endpoints::<ServiceSubscriberMarker>().unwrap();

        let subscriber =
            fuchsia_component::client::connect_to_protocol::<SubscriberMarker>().unwrap();

        if let Err(err) = subscriber.subscribe_to_service(ot::TREL_DNSSD_SERVICE_NAME, client) {
            error!("Unable to subscribe to {:?}: {:?}", ot::TREL_DNSSD_SERVICE_NAME, err);
        }

        // In the error case this channel will terminate and simply not discover any peers.
        server.into_stream().unwrap()
    }

    fn port(&self) -> u16 {
        self.socket.local_addr().unwrap().port()
    }

    fn register_service(&mut self, port: u16, txt: &[u8]) {
        let txt = split_txt(txt);
        let (client, server) =
            create_endpoints::<ServiceInstancePublicationResponder_Marker>().unwrap();

        let publisher =
            fuchsia_component::client::connect_to_protocol::<ServiceInstancePublisherMarker>()
                .unwrap();

        let publish_init_future = publisher
            .publish_service_instance(
                ot::TREL_DNSSD_SERVICE_NAME,
                self.instance_name.as_str(),
                ServiceInstancePublicationOptions::EMPTY,
                client,
            )
            .map(|x| -> Result<(), anyhow::Error> {
                match x {
                    Ok(Ok(x)) => Ok(x),
                    Ok(Err(err)) => Err(anyhow::format_err!("{:?}", err)),
                    Err(zx_err) => Err(zx_err.into()),
                }
            });

        let publish_responder_future =
            server.into_stream().unwrap().map_err(Into::into).try_for_each(
                move |ServiceInstancePublicationResponder_Request::OnPublication {
                          responder,
                          ..
                      }| {
                    let txt = txt.clone();
                    let _publisher = publisher.clone();
                    async move {
                        responder
                            .send(&mut Ok(ServiceInstancePublication {
                                port: Some(port),
                                text: Some(txt),
                                ..ServiceInstancePublication::EMPTY
                            }))
                            .map_err(Into::into)
                    }
                },
            );

        let future =
            futures::future::try_join(publish_init_future, publish_responder_future).map_ok(|_| ());

        self.publication_responder = Some(fuchsia_async::Task::spawn(future));
    }

    pub fn handle_service_subscriber_request(
        &mut self,
        ot_instance: &ot::Instance,
        service_subscriber_request: ServiceSubscriberRequest,
    ) -> Result<(), anyhow::Error> {
        match service_subscriber_request {
            // A DNS-SD IPv6 service instance has been discovered.
            ServiceSubscriberRequest::OnInstanceDiscovered {
                instance:
                    ServiceInstance {
                        instance: Some(instance_name),
                        ipv6_endpoint: Some(ipv6_endpoint),
                        text_strings,
                        ..
                    },
                responder,
            } => {
                let txt = flatten_txt(text_strings);
                let sockaddr: ot::SockAddr = ipv6_endpoint.into();

                self.peer_instance_sockaddr_map.insert(instance_name, sockaddr);

                let info = ot::PlatTrelPeerInfo::new(false, &txt, sockaddr);
                info!("otPlatTrelHandleDiscoveredPeerInfo: {:?}", info);
                ot_instance.plat_trel_handle_discovered_peer_info(&info);

                responder.send()?;
            }

            // A DNS-SD IPv6 service instance has changed.
            ServiceSubscriberRequest::OnInstanceChanged {
                instance:
                    ServiceInstance {
                        instance: Some(instance_name),
                        ipv6_endpoint: Some(ipv6_endpoint),
                        text_strings,
                        ..
                    },
                responder,
            } => {
                let txt = flatten_txt(text_strings);
                let sockaddr: ot::SockAddr = ipv6_endpoint.into();

                if let Some(old_sockaddr) =
                    self.peer_instance_sockaddr_map.insert(instance_name, sockaddr)
                {
                    if old_sockaddr != sockaddr {
                        // Remove old sockaddr with the same instance name
                        let info = ot::PlatTrelPeerInfo::new(true, &[], old_sockaddr);
                        info!("otPlatTrelHandleDiscoveredPeerInfo: {:?}", info);
                        ot_instance.plat_trel_handle_discovered_peer_info(&info);
                    }
                }

                let info = ot::PlatTrelPeerInfo::new(false, &txt, sockaddr);
                info!("otPlatTrelHandleDiscoveredPeerInfo: {:?}", info);
                ot_instance.plat_trel_handle_discovered_peer_info(&info);

                responder.send()?;
            }

            // A DNS-SD IPv6 service instance has been lost.
            ServiceSubscriberRequest::OnInstanceLost { instance, responder, .. } => {
                if let Some(sockaddr) = self.peer_instance_sockaddr_map.remove(&instance) {
                    let info = ot::PlatTrelPeerInfo::new(true, &[], sockaddr);
                    info!("otPlatTrelHandleDiscoveredPeerInfo: {:?}", info);
                    ot_instance.plat_trel_handle_discovered_peer_info(&info);
                }

                responder.send()?;
            }

            ServiceSubscriberRequest::OnInstanceChanged { responder, .. } => {
                // Skip changes without an IPv6 address.
                responder.send()?;
            }

            ServiceSubscriberRequest::OnInstanceDiscovered { responder, .. } => {
                // Skip discoveries without an IPv6 address.
                responder.send()?;
            }

            ServiceSubscriberRequest::OnQuery { responder, .. } => {
                // We don't care about queries.
                responder.send()?;
            }
        }
        Ok(())
    }

    /// Async entrypoint.
    pub fn poll(&mut self, instance: &ot::Instance, cx: &mut Context<'_>) {
        if let Some(task) = &mut self.publication_responder {
            if let Poll::Ready(x) = task.poll_unpin(cx) {
                warn!("TrelInstance: publication_responder finished unexpectedly: {:?}", x);
                self.publication_responder = None;
            }
        }

        let mut buffer = [0u8; crate::UDP_PACKET_MAX_LENGTH];
        match self.socket.async_recv_from(&mut buffer, cx) {
            Poll::Ready(Ok((len, sockaddr))) => {
                let sockaddr: ot::SockAddr = sockaddr.as_socket_ipv6().unwrap().into();
                info!("TrelInstance: Incoming {} byte packet from {:?}", len, sockaddr);
                instance.plat_trel_handle_received(&buffer[..len])
            }
            Poll::Ready(Err(err)) => {
                warn!("TrelInstance: Error receiving packet: {:?}", err);
            }
            _ => {}
        }

        if !self.subscriber_request_stream.is_terminated() {
            while let Poll::Ready(Some(event)) = self.subscriber_request_stream.poll_next_unpin(cx)
            {
                match event {
                    Ok(event) => {
                        if let Err(err) = self.handle_service_subscriber_request(instance, event) {
                            error!("handle_service_subscriber_request error {:?}", err);
                        }
                    }
                    Err(err) => {
                        error!("subscriber_request_stream error {:?}", err);
                    }
                }
            }
        }
    }
}

impl PlatformBacking {
    fn on_trel_enable(&self, instance: &ot::Instance) -> Result<u16, anyhow::Error> {
        let mut trel = self.trel.borrow_mut();
        if let Some(trel) = trel.as_ref() {
            Ok(trel.port())
        } else {
            let instance_name = hex::encode(instance.get_extended_address().as_slice());
            let trel_instance = TrelInstance::new(instance_name)?;
            let port = trel_instance.port();
            trel.replace(trel_instance);
            Ok(port)
        }
    }

    fn on_trel_disable(&self, _instance: &ot::Instance) {
        self.trel.replace(None);
    }

    fn on_trel_register_service(&self, _instance: &ot::Instance, port: u16, txt: &[u8]) {
        let mut trel = self.trel.borrow_mut();
        if let Some(trel) = trel.as_mut() {
            info!("otPlatTrelRegisterService: port:{} txt:{:?}", port, txt.to_escaped_string());
            trel.register_service(port, txt);
        } else {
            debug!("otPlatTrelRegisterService: TREL is disabled, cannot register.");
        }
    }

    fn on_trel_send(&self, _instance: &ot::Instance, payload: &[u8], sockaddr: &ot::SockAddr) {
        let trel = self.trel.borrow();
        if let Some(trel) = trel.as_ref() {
            info!("otPlatTrelSend: {:?} -> {}", sockaddr, hex::encode(payload));
            match trel.socket.send_to(payload, (*sockaddr).into()).now_or_never() {
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    warn!("otPlatTrelSend: send_to failed: {:?}", err);
                }
                None => {
                    warn!("otPlatTrelSend: send_to didn't finish immediately");
                }
            }
        } else {
            debug!("otPlatTrelSend: TREL is disabled, cannot send.");
        }
    }
}

#[no_mangle]
unsafe extern "C" fn otPlatTrelEnable(instance: *mut otInstance, port_ptr: *mut u16) {
    match PlatformBacking::on_trel_enable(
        // SAFETY: Must only be called from OpenThread thread,
        PlatformBacking::as_ref(),
        // SAFETY: `instance` must be a pointer to a valid `otInstance`,
        //         which is guaranteed by the caller.
        ot::Instance::ref_from_ot_ptr(instance).unwrap(),
    ) {
        Ok(port) => {
            info!("otPlatTrelEnable: Ready on port {}", port);
            *port_ptr = port;
        }
        Err(err) => {
            warn!("otPlatTrelEnable: Unable to start TREL: {:?}", err);
        }
    }
}

#[no_mangle]
unsafe extern "C" fn otPlatTrelDisable(instance: *mut otInstance) {
    PlatformBacking::on_trel_disable(
        // SAFETY: Must only be called from OpenThread thread,
        PlatformBacking::as_ref(),
        // SAFETY: `instance` must be a pointer to a valid `otInstance`,
        //         which is guaranteed by the caller.
        ot::Instance::ref_from_ot_ptr(instance).unwrap(),
    );
    info!("otPlatTrelDisable: Closed.");
}

#[no_mangle]
unsafe extern "C" fn otPlatTrelRegisterService(
    instance: *mut otInstance,
    port: u16,
    txt_data: *const u8,
    txt_len: u8,
) {
    PlatformBacking::on_trel_register_service(
        // SAFETY: Must only be called from OpenThread thread,
        PlatformBacking::as_ref(),
        // SAFETY: `instance` must be a pointer to a valid `otInstance`,
        //         which is guaranteed by the caller.
        ot::Instance::ref_from_ot_ptr(instance).unwrap(),
        port,
        // SAFETY: Caller guarantees either txt_data is valid or txt_len is zero.
        std::slice::from_raw_parts(txt_data, txt_len.into()),
    );
}

#[no_mangle]
unsafe extern "C" fn otPlatTrelSend(
    instance: *mut otInstance,
    payload_data: *const u8,
    payload_len: u16,
    dest: *const otSockAddr,
) {
    PlatformBacking::on_trel_send(
        // SAFETY: Must only be called from OpenThread thread,
        PlatformBacking::as_ref(),
        // SAFETY: `instance` must be a pointer to a valid `otInstance`,
        //         which is guaranteed by the caller.
        ot::Instance::ref_from_ot_ptr(instance).unwrap(),
        // SAFETY: Caller guarantees either payload_data is valid or payload_len is zero.
        std::slice::from_raw_parts(payload_data, payload_len.into()),
        // SAFETY: Caller guarantees dest points to a valid otSockAddr.
        ot::SockAddr::ref_from_ot_ptr(dest).unwrap(),
    );
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_split_txt() {
        assert_eq!(
            split_txt(b"\x0bxa=a7bfc4981f4e4d22\x0bxp=029c6f4dbae059cb"),
            vec![b"xa=a7bfc4981f4e4d22".to_vec(), b"xp=029c6f4dbae059cb".to_vec()]
        );
        assert_eq!(
            split_txt(b"xa=a7bfc4981f4e4d22\x0bxp=029c6f4dbae059cb"),
            vec![b"xa=a7bfc4981f4e4d22".to_vec(), b"xp=029c6f4dbae059cb".to_vec()]
        );
    }

    #[test]
    fn test_flatten_txt() {
        assert_eq!(flatten_txt(None), vec![]);
        assert_eq!(flatten_txt(Some(vec![])), vec![]);
        assert_eq!(
            flatten_txt(Some(vec![b"xa=a7bfc4981f4e4d22".to_vec()])),
            b"xa=a7bfc4981f4e4d22".to_vec()
        );
        assert_eq!(
            flatten_txt(Some(vec![
                b"xa=a7bfc4981f4e4d22".to_vec(),
                b"xp=029c6f4dbae059cb".to_vec()
            ])),
            b"xa=a7bfc4981f4e4d22\x0bxp=029c6f4dbae059cb".to_vec()
        );
    }
}
