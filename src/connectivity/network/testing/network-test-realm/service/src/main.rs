// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error, Result};
use async_utils::stream::FlattenUnorderedExt as _;
use fidl::prelude::*;
use fidl_fuchsia_component as fcomponent;
use fidl_fuchsia_component_decl as fdecl;
use fidl_fuchsia_hardware_ethernet as fethernet;
use fidl_fuchsia_hardware_network as fhwnet;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_net as fnet;
use fidl_fuchsia_net_debug as fnet_debug;
use fidl_fuchsia_net_ext as fnet_ext;
use fidl_fuchsia_net_interfaces as fnet_interfaces;
use fidl_fuchsia_net_interfaces_admin as fnet_interfaces_admin;
use fidl_fuchsia_net_interfaces_ext as fnet_interfaces_ext;
use fidl_fuchsia_net_test_realm as fntr;
use fidl_fuchsia_netstack as fnetstack;
use fidl_fuchsia_posix_socket as fposix_socket;
use fuchsia_async::{self as fasync, futures::StreamExt as _, TimeoutExt as _};
use fuchsia_zircon as zx;
use futures::{FutureExt as _, SinkExt as _, TryFutureExt as _, TryStreamExt as _};
use futures_lite::FutureExt as _;
use log::{error, warn};
use std::collections::HashMap;
use std::convert::TryFrom as _;
use std::path;

/// URL for the realm that contains the hermetic network components with a
/// Netstack2 instance.
const HERMETIC_NETWORK_V2_URL: &'static str = "#meta/hermetic_network_v2.cm";

/// Values for creating an interface on the hermetic Netstack.
///
/// Note that the topological path and the file path are not used by the
/// underlying Netstack. Consequently, fake values are defined here. Similarly,
/// the metric only needs to be a sensible value.
const DEFAULT_METRIC: u32 = 100;
const DEFAULT_INTERFACE_TOPOLOGICAL_PATH: &'static str = "/dev/fake/topological/path";
const DEFAULT_INTERFACE_FILE_PATH: &'static str = "/dev/fake/file/path";

trait ResultExt<T> {
    /// Converts from `Result<T, E>` to `Option<T>`.
    ///
    /// If there is an error, then the `msg` with the error appended is logged
    /// as a warning and `Option::None` is returned. Otherwise, `Option::Some`
    /// is returned with the value.
    fn ok_or_log_err(self, msg: &str) -> Option<T>;
}

impl<T, E: std::fmt::Debug> ResultExt<T> for Result<T, E> {
    fn ok_or_log_err(self, msg: &str) -> Option<T> {
        match self {
            Ok(val) => Some(val),
            Err(e) => {
                warn!("{}: {:?}", msg, e);
                None
            }
        }
    }
}

/// Returns a stream of existing `fhwnet::PortId`s for the provided
/// `device_proxy`.
fn existing_ports(
    device_proxy: &fhwnet::DeviceProxy,
) -> Result<impl futures::Stream<Item = fhwnet::PortId> + '_> {
    let client = netdevice_client::Client::new(Clone::clone(device_proxy));
    Ok(client.device_port_event_stream().context("device_port_event_stream failed")?.scan(
        (),
        |_: &mut (), event| match event {
            Ok(event) => match event {
                fhwnet::DevicePortEvent::Existing(port_id) => futures::future::ready(Some(port_id)),
                fhwnet::DevicePortEvent::Idle(fhwnet::Empty {}) => futures::future::ready(None),
                fhwnet::DevicePortEvent::Added(_) | fhwnet::DevicePortEvent::Removed(_) => {
                    // The first n events should correspond to existing
                    // ports and should be followed by an idle event. As a
                    // result, this shouldn't happen.
                    unreachable!("unexpected event: {:?}", event);
                }
            },
            Err(e) => {
                warn!("DevicePortEvent error: {:?}", e);
                futures::future::ready(None)
            }
        },
    ))
}

/// Returns a stream that contains a `P::Proxy` for each file in the provided
/// `directory`.
async fn file_proxies<P: fidl::endpoints::ProtocolMarker>(
    directory: &str,
) -> Result<impl futures::Stream<Item = P::Proxy> + '_, fntr::Error> {
    let (directory_proxy, directory_server_end) =
        fidl::endpoints::create_proxy::<fio::DirectoryMarker>().map_err(|e| {
            error!("create_proxy failed: {:?}", e);
            fntr::Error::Internal
        })?;
    fdio::service_connect(directory, directory_server_end.into_channel().into()).map_err(|e| {
        error!("service_connect failed to connect at {} with error: {:?}", directory, e);
        fntr::Error::Internal
    })?;

    let proxies: Result<Vec<P::Proxy>, fntr::Error> = files_async::readdir(&directory_proxy)
        .await
        .map_err(|e| {
            error!("failed to read files in {} with error {:?}", directory, e);
            fntr::Error::Internal
        })?
        .iter()
        .map(|file| {
            let filepath = path::Path::new(directory).join(&file.name);
            let filepath = filepath.to_str().ok_or_else(|| {
                error!("failed to convert file path to string");
                fntr::Error::Internal
            })?;
            let (proxy, server_end) = fidl::endpoints::create_proxy::<P>().map_err(|e| {
                error!("create_proxy failed: {:?}", e);
                fntr::Error::Internal
            })?;
            fdio::service_connect(filepath, server_end.into_channel().into()).map_err(|e| {
                error!("service_connect failed to connect at {} with error: {:?}", filepath, e);
                fntr::Error::Internal
            })?;
            Ok(proxy)
        })
        .collect();

    Ok(futures::stream::iter(proxies?))
}

/// Returns the `fhwnet::PortId` that corresponds to the provided
/// `expected_mac_address`.
///
/// If a matching port is not found, then `Option::None` is returned.
async fn find_matching_port_id(
    expected_mac_address: fnet_ext::MacAddress,
    device_proxy: &fhwnet::DeviceProxy,
) -> Result<Option<fhwnet::PortId>> {
    let results = existing_ports(&device_proxy)?.filter_map(|mut port_id| async move {
        // Note that errors are logged, but not propagated. In the event of an
        // error, this ensures that other ports/devices can be searched for the
        // `expected_mac_address`.

        let (port, port_server_end) = fidl::endpoints::create_proxy::<fhwnet::PortMarker>()
            .ok_or_log_err("create_proxy failed")?;

        device_proxy.get_port(&mut port_id, port_server_end).ok_or_log_err("get_port failed")?;

        let (mac, mac_server_end) = fidl::endpoints::create_proxy::<fhwnet::MacAddressingMarker>()
            .ok_or_log_err("create_proxy failed")?;

        port.get_mac(mac_server_end).ok_or_log_err("get_mac failed")?;

        let mac = mac.get_unicast_address().await.ok_or_log_err("get_unicast_address failed")?;

        (mac.octets == expected_mac_address.octets).then(|| port_id)
    });

    futures::pin_mut!(results);
    Ok(results.next().await)
}

/// Installs a netdevice with the provided `name` on the hermetic Netstack.
///
/// The `port_id` and `device_proxy` correspond to a system netdevice.
async fn install_netdevice(
    name: &str,
    mut port_id: fhwnet::PortId,
    device_proxy: fhwnet::DeviceProxy,
    connector: &HermeticNetworkConnector,
) -> Result<(), fntr::Error> {
    let installer = connector.connect_to_protocol::<fnet_interfaces_admin::InstallerMarker>()?;
    let (device_control, device_control_server_end) = fidl::endpoints::create_proxy::<
        fnet_interfaces_admin::DeviceControlMarker,
    >()
    .map_err(|e| {
        error!("create_proxy failed: {:?}", e);
        fntr::Error::Internal
    })?;

    let channel = fidl::endpoints::ClientEnd::<fhwnet::DeviceMarker>::new(
        device_proxy
            .into_channel()
            .map_err(|e| {
                error!("into_channel failed: {:?}", e);
                fntr::Error::Internal
            })?
            .into_zx_channel(),
    );
    installer.install_device(channel, device_control_server_end).map_err(|e| {
        error!("install_device failed: {:?}", e);
        fntr::Error::Internal
    })?;

    let (control, control_server_end) = fnet_interfaces_ext::admin::Control::create_endpoints()
        .map_err(|e| {
            error!("create_endpoints failed: {:?}", e);
            fntr::Error::Internal
        })?;

    device_control
        .create_interface(
            &mut port_id,
            control_server_end,
            fnet_interfaces_admin::Options {
                name: Some(name.to_string()),
                metric: Some(DEFAULT_METRIC),
                ..fnet_interfaces_admin::Options::EMPTY
            },
        )
        .map_err(|e| {
            error!("create_interface failed: {:?}", e);
            fntr::Error::Internal
        })?;

    // Enable the interface that was newly added to the hermetic Netstack. It is
    // not enabled by default. Note that the `enable_interface` function could
    // be used here, but is not since using the `Control` type is simpler in
    // this case (no need to fetch the interface ID).
    let _did_enable = control
        .enable()
        .await
        .map_err(|e| match e {
            fnet_interfaces_ext::admin::TerminalError::Fidl(e) => {
                error!("enable interface failed: {:?}", e);
                fntr::Error::Internal
            }
            fnet_interfaces_ext::admin::TerminalError::Terminal(removed_reason) => {
                match removed_reason {
                    fnet_interfaces_admin::InterfaceRemovedReason::DuplicateName => {
                        fntr::Error::AlreadyExists
                    }
                    e => {
                        error!("interface removed: {:?}", e);
                        fntr::Error::Internal
                    }
                }
            }
        })?
        .map_err(|e| {
            error!("enable interface error: {:?}", e);
            fntr::Error::Internal
        })?;

    // Extend the lifetime of the created interface beyond that of the `control`
    // and `device_control` types. Note that the lifetime of the created
    // interface is tied to the hermetic Netstack. That is, the interface will
    // be removed when the hermetic Netstack is shutdown.
    control.detach().map_err(|e| {
        error!("detatch failed for Control: {:?}", e);
        fntr::Error::Internal
    })?;
    device_control.detach().map_err(|e| {
        error!("detach failed for DeviceControl: {:?}", e);
        fntr::Error::Internal
    })
}

/// Attempts to install a netdevice on the hermetic Netstack.
///
/// If a device was installed, then true is returned. An error may be returned
/// if the `expected_mac_address` matches a netdevice, but installation of the
/// device on the hermetic Netstack failed.
async fn try_install_netdevice(
    name: &str,
    expected_mac_address: fnet_ext::MacAddress,
    connector: &HermeticNetworkConnector,
) -> Result<bool, fntr::Error> {
    const NETDEV_DIRECTORY_PATH: &'static str = "/dev/class/network";

    let results = file_proxies::<fhwnet::DeviceInstanceMarker>(NETDEV_DIRECTORY_PATH)
        .await?
        .filter_map(|device_instance_proxy| async move {
            // Note that errors are logged, but not propagated. In the event of
            // an error, this ensures that other devices can be searched for the
            // `expected_mac_address`.

            let (device_proxy, device_server_end) =
                fidl::endpoints::create_proxy::<fhwnet::DeviceMarker>()
                    .ok_or_log_err("create_proxy failed")?;

            device_instance_proxy
                .get_device(device_server_end)
                .ok_or_log_err("get_device failed")?;

            find_matching_port_id(expected_mac_address, &device_proxy)
                .await
                .ok_or_log_err("find_matching_port_id failed")?
                .and_then(|port_id| Some((port_id, device_proxy)))
        });

    futures::pin_mut!(results);

    match results.next().await {
        Some((port_id, device_proxy)) => {
            install_netdevice(name, port_id, device_proxy, connector).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Installs an ethernet device with the provided `name` on the hermetic
/// Netstack.
async fn install_eth_device(
    name: &str,
    device_proxy: fethernet::DeviceProxy,
    connector: &HermeticNetworkConnector,
) -> Result<(), fntr::Error> {
    let device_client_end = fidl::endpoints::ClientEnd::<fethernet::DeviceMarker>::new(
        device_proxy
            .into_channel()
            .map_err(|e| {
                error!("into_channel failed: {:?}", e);
                fntr::Error::Internal
            })?
            .into_zx_channel(),
    );

    // TODO(https://fxbug.dev/89651): Support Netstack3. Currently, an
    // interface name cannot be specified when adding an interface via
    // fuchsia.net.stack.Stack. As a result, the Network Test Realm
    // currently does not support Netstack3.
    let id: u32 = connector
        .connect_to_protocol::<fnetstack::NetstackMarker>()?
        .add_ethernet_device(
            DEFAULT_INTERFACE_TOPOLOGICAL_PATH,
            &mut fnetstack::InterfaceConfig {
                name: name.to_string(),
                filepath: DEFAULT_INTERFACE_FILE_PATH.to_string(),
                metric: DEFAULT_METRIC,
            },
            device_client_end,
        )
        .await
        .map_err(|e| {
            error!("add_ethernet_device failed: {:?}", e);
            fntr::Error::Internal
        })?
        .map_err(|e| match zx::Status::from_raw(e) {
            zx::Status::ALREADY_EXISTS => fntr::Error::AlreadyExists,
            status => {
                error!("add_ethernet_device error: {:?}", status);
                fntr::Error::Internal
            }
        })?;

    // Enable the interface that was newly added to the hermetic Netstack.
    // It is not enabled by default.
    enable_interface(id.into(), connector).await
}

/// Attempts to install an ethernet device on the hermetic Netstack.
///
/// If a device was installed, then true is returned. An error may be returned
/// if the `expected_mac_address` matches an ethernet device, but installation
/// of the device on the hermetic Netstack failed.
async fn try_install_eth_device(
    name: &str,
    expected_mac_address: fnet_ext::MacAddress,
    connector: &HermeticNetworkConnector,
) -> Result<bool, fntr::Error> {
    const ETHERNET_DIRECTORY_PATH: &'static str = "/dev/class/ethernet";

    let results = file_proxies::<fethernet::DeviceMarker>(ETHERNET_DIRECTORY_PATH)
        .await?
        .filter_map(|device_proxy| async move {
            // Note that errors are logged, but not propagated. In the event of
            // an error, this ensures that other devices can be searched for the
            // `expected_mac_address`.

            device_proxy.get_info().await.ok_or_log_err("get_info failed").and_then(|info| {
                (info.mac.octets == expected_mac_address.octets).then(|| device_proxy)
            })
        });
    futures::pin_mut!(results);

    match results.next().await {
        Some(device_proxy) => {
            install_eth_device(name, device_proxy, connector).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Adds an interface with the provided `name` to the hermetic Netstack.
///
/// If a matching interface cannot be found in devfs, then an
/// `fntr::Error::InterfaceNotFound` error is returned. Errors installing the
/// interface may also be propagated.
async fn install_interface(
    name: &str,
    mac_address: fnet_ext::MacAddress,
    connector: &HermeticNetworkConnector,
) -> Result<(), fntr::Error> {
    // TODO(https://fxbug.dev/89648): Replace this with fuchsia.net.debug, which
    // should ideally provide direct access to the matching interface. As an
    // intermediate solution, interfaces are read directly from devfs (for
    // ethernet and netdevice).
    (try_install_eth_device(name, mac_address, connector).await?
        || try_install_netdevice(name, mac_address, connector).await?)
        .then(|| ())
        .ok_or(fntr::Error::InterfaceNotFound)
}

/// Returns the id for the enabled interface that matches `mac_address`.
///
/// If an interface matching `mac_address` is not found, then an error is
/// returned. If a matching interface is found, but it is not enabled, then
/// `Option::None` is returned.
async fn find_enabled_interface_id(
    expected_mac_address: fnet_ext::MacAddress,
) -> Result<Option<u64>, fntr::Error> {
    let state_proxy = SystemConnector.connect_to_protocol::<fnet_interfaces::StateMarker>()?;
    let stream = fnet_interfaces_ext::event_stream_from_state(&state_proxy).map_err(|e| {
        error!("failed to read interface stream: {:?}", e);
        fntr::Error::Internal
    })?;

    let interfaces = fnet_interfaces_ext::existing(stream, HashMap::new()).await.map_err(|e| {
        error!("failed to read existing interfaces: {:?}", e);
        fntr::Error::Internal
    })?;

    let debug_interfaces_proxy =
        SystemConnector.connect_to_protocol::<fnet_debug::InterfacesMarker>()?;

    let interfaces_stream = futures::stream::iter(interfaces.into_values());

    let results =
        interfaces_stream.filter_map(|fnet_interfaces_ext::Properties { id, online, .. }| {
            let debug_interfaces_proxy = &debug_interfaces_proxy;
            async move {
                match debug_interfaces_proxy.get_mac(id).await {
                    Err(e) => {
                        let _: fidl::Error = e;
                        warn!("get_mac failure: {:?}", e);
                        None
                    }
                    Ok(result) => match result {
                        Err(fnet_debug::InterfacesGetMacError::NotFound) => {
                            warn!("get_mac interface not found for ID: {}", id);
                            None
                        }
                        Ok(mac_address) => mac_address.and_then(|mac_address| {
                            (mac_address.octets == expected_mac_address.octets)
                                .then(move || (id, online))
                        }),
                    },
                }
            }
        });

    futures::pin_mut!(results);

    results.next().await.ok_or(fntr::Error::InterfaceNotFound).map(|(id, online)| {
        if online {
            Some(id)
        } else {
            None
        }
    })
}

/// Returns a `fnet_interfaces_admin::ControlProxy` that can be used to
/// manipulate the interface that has the provided `id`.
async fn connect_to_interface_admin_control(
    id: u64,
    connector: &impl Connector,
) -> Result<fnet_interfaces_ext::admin::Control, fntr::Error> {
    let debug_interfaces_proxy = connector.connect_to_protocol::<fnet_debug::InterfacesMarker>()?;
    let (control, server) =
        fnet_interfaces_ext::admin::Control::create_endpoints().map_err(|e| {
            error!("create_proxy failure: {:?}", e);
            fntr::Error::Internal
        })?;
    debug_interfaces_proxy.get_admin(id, server).map_err(|e| {
        error!("get_admin failure: {:?}", e);
        fntr::Error::Internal
    })?;
    Ok(control)
}

/// Enables the interface with `id` using the provided `debug_interfaces_proxy`.
async fn enable_interface(id: u64, connector: &impl Connector) -> Result<(), fntr::Error> {
    let control_proxy = connect_to_interface_admin_control(id, connector).await?;
    let _did_enable: bool = control_proxy
        .enable()
        .await
        .map_err(|e| {
            error!("enable interface id: {} failure: {:?}", id, e);
            fntr::Error::Internal
        })?
        .map_err(|e| {
            error!("enable interface id: {} error: {:?}", id, e);
            fntr::Error::Internal
        })?;
    Ok(())
}

/// Disables the interface with `id` using the provided
/// `debug_interfaces_proxy`.
async fn disable_interface(id: u64, connector: &impl Connector) -> Result<(), fntr::Error> {
    let control_proxy = connect_to_interface_admin_control(id, connector).await?;
    let _did_disable: bool = control_proxy
        .disable()
        .await
        .map_err(|e| {
            error!("disable interface id: {} failure: {:?}", id, e);
            fntr::Error::Internal
        })?
        .map_err(|e| {
            error!("disable interface id: {} error: {:?}", id, e);
            fntr::Error::Internal
        })?;
    Ok(())
}

fn create_child_decl(child_name: &str, url: &str) -> fdecl::Child {
    fdecl::Child {
        name: Some(child_name.to_string()),
        url: Some(url.to_string()),
        // TODO(https://fxbug.dev/90085): Remove the startup field when the
        // child is being created in a single_run collection. In such a case,
        // this field is currently required to be set to
        // `fdecl::StartupMode::Lazy` even though it is a no-op.
        startup: Some(fdecl::StartupMode::Lazy),
        ..fdecl::Child::EMPTY
    }
}

/// Creates a child component named `child_name` within the provided
/// `collection_name`.
///
/// The `url` corresponds to the URL of the component to add. The `connector`
/// connects to the desired realm.
async fn create_child(
    mut collection_ref: fdecl::CollectionRef,
    child: fdecl::Child,
    connector: &impl Connector,
) -> Result<(), fntr::Error> {
    let realm_proxy = connector.connect_to_protocol::<fcomponent::RealmMarker>()?;

    realm_proxy
        .create_child(&mut collection_ref, child, fcomponent::CreateChildArgs::EMPTY)
        .await
        .map_err(|e| {
            error!("create_child failed: {:?}", e);
            fntr::Error::Internal
        })?
        .map_err(|e| {
            match e {
                // Variants that may be returned by the `CreateChild` method.
                fcomponent::Error::InstanceCannotResolve => fntr::Error::ComponentNotFound,
                fcomponent::Error::InstanceCannotUnresolve => fntr::Error::ComponentNotFound,
                fcomponent::Error::InvalidArguments => fntr::Error::InvalidArguments,
                fcomponent::Error::CollectionNotFound
                | fcomponent::Error::InstanceAlreadyExists
                | fcomponent::Error::InstanceDied
                | fcomponent::Error::ResourceUnavailable
                // Variants that are not returned by the `CreateChild` method.
                | fcomponent::Error::AccessDenied
                | fcomponent::Error::InstanceCannotStart
                | fcomponent::Error::InstanceNotFound
                | fcomponent::Error::Internal
                | fcomponent::Error::ResourceNotFound
                | fcomponent::Error::Unsupported => {
                    error!("create_child error: {:?}", e);
                    fntr::Error::Internal
                }
            }
        })
}

#[derive(thiserror::Error, Debug)]
enum DestroyChildError {
    #[error("Internal error")]
    Internal,
    #[error("Component not running")]
    NotRunning,
}

/// Destroys the child component that corresponds to `child_ref`.
///
/// The `connector` connects to the desired realm. A `not_running_error` will be
/// returned if the provided `child_ref` does not exist.
async fn destroy_child(
    mut child_ref: fdecl::ChildRef,
    connector: &impl Connector,
) -> Result<(), DestroyChildError> {
    let realm_proxy = connector
        .connect_to_protocol::<fcomponent::RealmMarker>()
        .map_err(|_e| DestroyChildError::Internal)?;

    realm_proxy
        .destroy_child(&mut child_ref)
        .await
        .map_err(|e| {
            error!("destroy_child failed: {:?}", e);
            DestroyChildError::Internal
        })?
        .map_err(|e| {
            match e {
            // Variants that may be returned by the `DestroyChild`
            // method. `CollectionNotFound` and `InstanceNotFound`
            // mean that the hermetic network realm does not exist. All
            // other errors are propagated as internal errors.
            fcomponent::Error::CollectionNotFound
            | fcomponent::Error::InstanceNotFound =>
                DestroyChildError::NotRunning,
            fcomponent::Error::InstanceDied
            | fcomponent::Error::InvalidArguments
            // Variants that are not returned by the `DestroyChild`
            // method.
            | fcomponent::Error::AccessDenied
            | fcomponent::Error::InstanceAlreadyExists
            | fcomponent::Error::InstanceCannotResolve
            | fcomponent::Error::InstanceCannotUnresolve
            | fcomponent::Error::InstanceCannotStart
            | fcomponent::Error::Internal
            | fcomponent::Error::ResourceNotFound
            | fcomponent::Error::ResourceUnavailable
            | fcomponent::Error::Unsupported => {
                error!("destroy_child error: {:?}", e);
                DestroyChildError::Internal
            }
    }
        })
}

async fn has_stub(connector: &impl Connector) -> Result<bool, fntr::Error> {
    let realm_proxy = connector.connect_to_protocol::<fcomponent::RealmMarker>()?;
    network_test_realm::has_stub(&realm_proxy).await.map_err(|e| {
        error!("failed to check for hermetic network realm: {:?}", e);
        fntr::Error::Internal
    })
}

/// A type that can connect to a FIDL protocol within a particular realm.
trait Connector {
    fn connect_to_protocol<P: fidl::endpoints::DiscoverableProtocolMarker>(
        &self,
    ) -> Result<P::Proxy, fntr::Error>;
}

/// Connects to protocols that are exposed to the Network Test Realm.
struct SystemConnector;

impl Connector for SystemConnector {
    fn connect_to_protocol<P: fidl::endpoints::DiscoverableProtocolMarker>(
        &self,
    ) -> Result<P::Proxy, fntr::Error> {
        fuchsia_component::client::connect_to_protocol::<P>().map_err(|e| {
            error!("failed to connect to {} with error: {:?}", P::PROTOCOL_NAME, e);
            fntr::Error::Internal
        })
    }
}

/// Connects to protocols within the hermetic-network realm.
struct HermeticNetworkConnector {
    child_directory: fio::DirectoryProxy,
}

impl HermeticNetworkConnector {
    async fn new() -> Result<Self, fntr::Error> {
        Ok(Self {
            child_directory: fuchsia_component::client::open_childs_exposed_directory(
                network_test_realm::HERMETIC_NETWORK_REALM_NAME.to_string(),
                Some(network_test_realm::HERMETIC_NETWORK_COLLECTION_NAME.to_string()),
            )
            .await
            .map_err(|e| {
                error!("open_childs_exposed_directory failed: {:?}", e);
                fntr::Error::Internal
            })?,
        })
    }
}

impl Connector for HermeticNetworkConnector {
    fn connect_to_protocol<P: fidl::endpoints::DiscoverableProtocolMarker>(
        &self,
    ) -> Result<P::Proxy, fntr::Error> {
        fuchsia_component::client::connect_to_protocol_at_dir_root::<P>(&self.child_directory)
            .map_err(|e| {
                error!("failed to connect to {} with error: {:?}", P::PROTOCOL_NAME, e);
                fntr::Error::Internal
            })
    }
}

async fn create_socket(
    domain: fposix_socket::Domain,
    protocol: fposix_socket::DatagramSocketProtocol,
    connector: &HermeticNetworkConnector,
) -> Result<socket2::Socket, fntr::Error> {
    let socket_provider = connector.connect_to_protocol::<fposix_socket::ProviderMarker>()?;
    let sock = socket_provider
        .datagram_socket(domain, protocol)
        .await
        .map_err(|e| {
            error!("datagram_socket failed: {:?}", e);
            fntr::Error::Internal
        })?
        .map_err(|e| {
            error!("datagram_socket error: {:?}", e);
            fntr::Error::Internal
        })?;

    fdio::create_fd(sock.into()).map_err(|e| {
        error!("create_fd from socket failed: {:?}", e);
        fntr::Error::Internal
    })
}

async fn create_icmp_socket(
    domain: fposix_socket::Domain,
    connector: &HermeticNetworkConnector,
) -> Result<fasync::net::DatagramSocket, fntr::Error> {
    Ok(fasync::net::DatagramSocket::new_from_socket(
        create_socket(domain, fposix_socket::DatagramSocketProtocol::IcmpEcho, connector).await?,
    )
    .map_err(|e| {
        error!("new_from_socket failed: {:?}", e);
        fntr::Error::Internal
    })?)
}

async fn bind_udp_socket(
    domain: fposix_socket::Domain,
    connector: &HermeticNetworkConnector,
) -> Result<fasync::net::UdpSocket, fntr::Error> {
    let socket =
        create_socket(domain, fposix_socket::DatagramSocketProtocol::Udp, connector).await?;
    let address: std::net::SocketAddr = (
        match domain {
            fposix_socket::Domain::Ipv4 => std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            fposix_socket::Domain::Ipv6 => std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        },
        0,
    )
        .into();
    let () = socket.bind(&address.into()).map_err(|e| {
        error!("error binding socket to address {:?}: {:?}", address, e);
        fntr::Error::Internal
    })?;
    Ok(fasync::net::UdpSocket::from_socket(socket.into()).map_err(|e| {
        error!("error converting socket to fuchsia_async::net::UdpSocket: {:?}", e);
        fntr::Error::Internal
    })?)
}

/// Returns the scope ID needed for the provided `address`.
///
/// See https://tools.ietf.org/html/rfc2553#section-3.3 for more information.
async fn get_interface_scope_id(
    interface_name: &Option<String>,
    address: &std::net::Ipv6Addr,
    connector: &impl Connector,
) -> Result<u32, fntr::Error> {
    const DEFAULT_SCOPE_ID: u32 = 0;
    let is_link_local_address =
        net_types::ip::Ipv6Addr::from_bytes(address.octets()).is_unicast_link_local();

    match (interface_name, is_link_local_address) {
        // If a link-local address is specified, then an interface name
        // must be provided.
        (None, true) => Err(fntr::Error::InvalidArguments),
        // The default scope ID should be used for any non link-local
        // address.
        (Some(_), false) | (None, false) => Ok(DEFAULT_SCOPE_ID),
        (Some(interface_name), true) => network_test_realm::get_interface_id(
            &interface_name,
            &connector.connect_to_protocol::<fnet_interfaces::StateMarker>()?,
        )
        .await
        .map_err(|e| {
            error!(
                "failed to obtain interface ID for interface named: {} with error: {:?}",
                interface_name, e
            );
            fntr::Error::Internal
        })?
        .ok_or(fntr::Error::InterfaceNotFound)
        .and_then(|id| {
            u32::try_from(id).map_err(|e| {
                error!("failed to convert interface ID to u32, {:?}", e);
                fntr::Error::Internal
            })
        }),
    }
}

/// Sends a single ICMP echo request to `address`.
async fn ping_once<Ip: ping::IpExt>(
    address: Ip::Addr,
    payload_length: usize,
    interface_name: Option<String>,
    timeout: zx::Duration,
    connector: &HermeticNetworkConnector,
) -> Result<(), fntr::Error> {
    let socket = create_icmp_socket(Ip::DOMAIN_FIDL, connector).await?;

    if let Some(interface_name) = interface_name {
        socket.bind_device(Some(interface_name.as_bytes())).map_err(|e| match e.kind() {
            std::io::ErrorKind::InvalidInput => fntr::Error::InterfaceNotFound,
            _kind => {
                error!("bind_device for interface: {} failed: {:?}", interface_name, e);
                fntr::Error::Internal
            }
        })?;
    }

    // The body of the packet is filled with `payload_length` 0 bytes.
    let payload: Vec<u8> = std::iter::repeat(0).take(payload_length).collect();

    let (mut sink, mut stream) = ping::new_unicast_sink_and_stream::<Ip, _, { u16::MAX as usize }>(
        &socket, &address, &payload,
    );

    const SEQ: u16 = 1;
    sink.send(SEQ).await.map_err(|e| {
        warn!("failed to send ping: {:?}", e);
        match e {
            ping::PingError::Send(error) => match error.kind() {
                // `InvalidInput` corresponds to an oversized `payload_length`.
                std::io::ErrorKind::InvalidInput => fntr::Error::InvalidArguments,
                // TODO(https://github.com/rust-lang/rust/issues/86442): Consider
                // defining more granular error codes once the relevant
                // `std::io::Error` variants are stable (e.g. `HostUnreachable`,
                // `NetworkUnreachable`, etc.).
                _kind => fntr::Error::PingFailed,
            },
            ping::PingError::Body { .. }
            | ping::PingError::Parse
            | ping::PingError::Recv(_)
            | ping::PingError::ReplyCode(_)
            | ping::PingError::ReplyType { .. }
            | ping::PingError::SendLength { .. } => fntr::Error::PingFailed,
        }
    })?;

    if timeout.into_nanos() <= 0 {
        return Ok(());
    }

    match stream.try_next().map(Some).on_timeout(timeout, || None).await {
        None => Err(fntr::Error::TimeoutExceeded),
        Some(Err(e)) => {
            warn!("failed to receive ping response: {:?}", e);
            Err(fntr::Error::PingFailed)
        }
        Some(Ok(None)) => {
            error!("ping reply stream ended unexpectedly");
            Err(fntr::Error::Internal)
        }
        Some(Ok(Some(got))) if got == SEQ => Ok(()),
        Some(Ok(Some(got))) => {
            error!("received unexpected ping sequence number; got: {}, want: {}", got, SEQ);
            Err(fntr::Error::PingFailed)
        }
    }
}

/// Creates a socket if `socket` is None. Otherwise, returns a reference to the
/// `socket` value.
///
/// If a socket is created, then the value of `socket` is replaced with the
/// newly created socket.
async fn get_or_insert_socket<'a>(
    socket: &'a mut Option<socket2::Socket>,
    domain: fposix_socket::Domain,
    connector: &'a HermeticNetworkConnector,
) -> Result<&'a socket2::Socket, fntr::Error> {
    match socket {
        None => Ok(socket.insert(
            create_socket(domain, fposix_socket::DatagramSocketProtocol::Udp, connector).await?,
        )),
        Some(value) => Ok(value),
    }
}

/// A controller for creating and manipulating the Network Test Realm.
///
/// The Network Test Realm corresponds to a hermetic network realm with a
/// Netstack under test. The `Controller` is responsible for configuring
/// this realm, the Netstack under test, and the system's Netstack. Once
/// configured, the controller is expected to yield the following component
/// topology:
///
/// ```
///            network-test-realm (this controller component)
///                    |
///             enclosed-network (a collection)
///                    |
///             hermetic-network
///            /       |        \
///      netstack     ...      stubs (a collection)
///                              |
///                          test-stub (a configurable component)
/// ```
///
/// This topology enables the `Controller` to interact with the system's
/// Netstack and the Netstack in the "hermetic-network" realm. Additionally, it
/// enables capabilities to be routed from the hermetic Netstack to siblings
/// (via the "hermetic-network" parent) while remaining isolated from the rest
/// of the system.
struct Controller {
    /// Interface IDs that have been mutated on the system's Netstack.
    mutated_interface_ids: Vec<u64>,

    /// Connector to access protocols within the hermetic-network realm. If the
    /// hermetic-network realm does not exist, then this will be `None`.
    hermetic_network_connector: Option<HermeticNetworkConnector>,

    /// Socket for joining and leaving Ipv4 multicast groups. This field is
    /// lazily instantiated the first time an Ipv4 multicast group is joined or
    /// left. Note that the lifetime of Ipv4 multicast memberships (those added
    /// via the `join_multicast_group` method) are tied to this field.
    multicast_v4_socket: Option<socket2::Socket>,

    /// Socket for joining and leaving Ipv6 multicast groups. This field is
    /// lazily instantiated the first time an Ipv6 multicast group is joined or
    /// left. Note that the lifetime of Ipv6 multicast memberships (those added
    /// via the `join_multicast_group` method) are tied to this field.
    multicast_v6_socket: Option<socket2::Socket>,
}

impl Controller {
    fn new() -> Self {
        Self {
            mutated_interface_ids: Vec::<u64>::new(),
            hermetic_network_connector: None,
            multicast_v4_socket: None,
            multicast_v6_socket: None,
        }
    }

    async fn handle_request(
        &mut self,
        request: fntr::ControllerRequest,
    ) -> Result<(), fidl::Error> {
        match request {
            fntr::ControllerRequest::StartHermeticNetworkRealm { netstack, responder } => {
                let mut result = self.start_hermetic_network_realm(netstack).await;
                responder.send(&mut result)?;
            }
            fntr::ControllerRequest::StopHermeticNetworkRealm { responder } => {
                let mut result = self.stop_hermetic_network_realm().await;
                responder.send(&mut result)?;
            }
            fntr::ControllerRequest::AddInterface { mac_address, name, responder } => {
                let mac_address = fnet_ext::MacAddress::from(mac_address);
                let mut result = self.add_interface(mac_address, &name).await;
                responder.send(&mut result)?;
            }
            fntr::ControllerRequest::StartStub { component_url, responder } => {
                let mut result = self.start_stub(&component_url).await;
                responder.send(&mut result)?;
            }
            fntr::ControllerRequest::StopStub { responder } => {
                let mut result = self.stop_stub().await;
                responder.send(&mut result)?;
            }
            fntr::ControllerRequest::PollUdp {
                target,
                payload,
                timeout,
                num_retries,
                responder,
            } => {
                let mut rx_buffer = vec![0; fntr::MAX_UDP_POLL_LENGTH.into()];
                let fnet_ext::SocketAddress(target) = target.into();
                let result = self
                    .poll_udp(
                        target,
                        &payload,
                        zx::Duration::from_nanos(timeout),
                        num_retries,
                        &mut rx_buffer,
                    )
                    .await;
                let mut result = result.map(|num_bytes| {
                    rx_buffer.truncate(num_bytes);
                    rx_buffer
                });
                responder.send(&mut result)?;
            }
            fntr::ControllerRequest::Ping {
                target,
                payload_length,
                interface_name,
                timeout,
                responder,
            } => {
                let mut result = self
                    .ping(target, payload_length, interface_name, zx::Duration::from_nanos(timeout))
                    .await;
                responder.send(&mut result)?;
            }
            fntr::ControllerRequest::JoinMulticastGroup { address, interface_id, responder } => {
                let mut result = self.join_multicast_group(address, interface_id).await;
                responder.send(&mut result)?;
            }
            fntr::ControllerRequest::LeaveMulticastGroup { address, interface_id, responder } => {
                let mut result = self.leave_multicast_group(address, interface_id).await;
                responder.send(&mut result)?;
            }
        }
        Ok(())
    }

    /// Returns the `socket2::Socket` that should be used join or leave
    /// multicast groups for `address`.
    ///
    /// Creates a socket on the first invocation for the relevant IP version.
    /// Subsequent invocations return the existing socket.
    async fn get_or_create_multicast_socket(
        &mut self,
        address: std::net::IpAddr,
    ) -> Result<&socket2::Socket, fntr::Error> {
        let hermetic_network_connector = self
            .hermetic_network_connector
            .as_ref()
            .ok_or(fntr::Error::HermeticNetworkRealmNotRunning)?;

        match address {
            std::net::IpAddr::V4(_) => {
                get_or_insert_socket(
                    &mut self.multicast_v4_socket,
                    fposix_socket::Domain::Ipv4,
                    hermetic_network_connector,
                )
                .await
            }
            std::net::IpAddr::V6(_) => {
                get_or_insert_socket(
                    &mut self.multicast_v6_socket,
                    fposix_socket::Domain::Ipv6,
                    hermetic_network_connector,
                )
                .await
            }
        }
    }

    /// Leaves the multicast group `address` using the provided `interface_id`.
    async fn leave_multicast_group(
        &mut self,
        address: fnet::IpAddress,
        interface_id: u64,
    ) -> Result<(), fntr::Error> {
        let fnet_ext::IpAddress(address) = address.into();
        let interface_id = u32::try_from(interface_id).map_err(|e| {
            error!("failed to convert interface ID to u32, {:?}", e);
            fntr::Error::Internal
        })?;
        let socket = self.get_or_create_multicast_socket(address).await?;

        match address {
            std::net::IpAddr::V4(addr) => socket.leave_multicast_v4_n(
                &addr,
                &socket2::InterfaceIndexOrAddress::Index(interface_id),
            ),
            std::net::IpAddr::V6(addr) => socket.leave_multicast_v6(&addr, interface_id),
        }
        .map_err(|e| match e.kind() {
            // The group `address` was not previously joined.
            std::io::ErrorKind::AddrNotAvailable => fntr::Error::AddressNotAvailable,
            // The specified `interface_id` does not exist or the `address`
            // does not correspond to a valid multicast address.
            std::io::ErrorKind::InvalidInput => fntr::Error::InvalidArguments,
            _kind => {
                error!("leave_multicast_group failed: {:?}", e);
                fntr::Error::Internal
            }
        })
    }

    /// Joins the multicast group `address` using the provided `interface_id`.
    async fn join_multicast_group(
        &mut self,
        address: fnet::IpAddress,
        interface_id: u64,
    ) -> Result<(), fntr::Error> {
        let fnet_ext::IpAddress(address) = address.into();
        let interface_id = u32::try_from(interface_id).map_err(|e| {
            error!("failed to convert interface ID to u32, {:?}", e);
            fntr::Error::Internal
        })?;
        let socket = self.get_or_create_multicast_socket(address).await?;

        match address {
            std::net::IpAddr::V4(addr) => socket
                .join_multicast_v4_n(&addr, &socket2::InterfaceIndexOrAddress::Index(interface_id)),
            std::net::IpAddr::V6(addr) => socket.join_multicast_v6(&addr, interface_id),
        }
        .map_err(|e| match e.kind() {
            // The group `address` was already joined.
            std::io::ErrorKind::AddrInUse => fntr::Error::AddressInUse,
            // The specified `interface_id` does not exist or the `address`
            // does not correspond to a valid multicast address.
            std::io::ErrorKind::InvalidInput => fntr::Error::InvalidArguments,
            _kind => {
                error!("join_multicast_group failed: {:?}", e);
                fntr::Error::Internal
            }
        })
    }

    /// Starts a test stub within the hermetic-network realm.
    async fn start_stub(&self, component_url: &str) -> Result<(), fntr::Error> {
        // Stubs exist only within the hermetic-network realm. Therefore,
        // the hermetic-network realm must exist.
        let hermetic_network_connector = self
            .hermetic_network_connector
            .as_ref()
            .ok_or(fntr::Error::HermeticNetworkRealmNotRunning)?;

        if has_stub(hermetic_network_connector).await? {
            // The `Controller` only configures one test stub at a time. As a
            // result, any existing stub must be stopped before a new one is
            // started.
            self.stop_stub().await.map_err(|e| match e {
                fntr::Error::StubNotRunning => {
                    error!("attempted to stop stub that was not running");
                    fntr::Error::Internal
                }
                fntr::Error::AddressInUse
                | fntr::Error::AddressUnreachable
                | fntr::Error::AddressNotAvailable
                | fntr::Error::AlreadyExists
                | fntr::Error::ComponentNotFound
                | fntr::Error::HermeticNetworkRealmNotRunning
                | fntr::Error::Internal
                | fntr::Error::InterfaceNotFound
                | fntr::Error::InvalidArguments
                | fntr::Error::PingFailed
                | fntr::Error::TimeoutExceeded => e,
            })?;
        }

        create_child(
            fdecl::CollectionRef { name: network_test_realm::STUB_COLLECTION_NAME.to_string() },
            create_child_decl(network_test_realm::STUB_COMPONENT_NAME, component_url),
            hermetic_network_connector,
        )
        .await
    }

    /// Stops the test stub within the hermetic-network realm.
    async fn stop_stub(&self) -> Result<(), fntr::Error> {
        // Stubs exist only within the hermetic-network realm. Therefore,
        // the hermetic-network realm must exist.
        let hermetic_network_connector = self
            .hermetic_network_connector
            .as_ref()
            .ok_or(fntr::Error::HermeticNetworkRealmNotRunning)?;

        destroy_child(network_test_realm::create_stub_child_ref(), hermetic_network_connector)
            .await
            .map_err(|e| match e {
                DestroyChildError::Internal => fntr::Error::Internal,
                DestroyChildError::NotRunning => fntr::Error::StubNotRunning,
            })
    }

    async fn poll_udp(
        &self,
        target: std::net::SocketAddr,
        payload: &[u8],
        timeout: zx::Duration,
        num_retries: u16,
        rx_buffer: &mut [u8],
    ) -> Result<usize, fntr::Error> {
        let hermetic_network_connector = self
            .hermetic_network_connector
            .as_ref()
            .ok_or(fntr::Error::HermeticNetworkRealmNotRunning)?;

        let socket = bind_udp_socket(
            match &target {
                std::net::SocketAddr::V4(_) => fposix_socket::Domain::Ipv4,
                std::net::SocketAddr::V6(_) => fposix_socket::Domain::Ipv6,
            },
            hermetic_network_connector,
        )
        .await?;

        let socket = &socket;

        let fold_result = async_utils::fold::try_fold_while(
            futures::stream::iter(0..num_retries)
                .then(|_| socket.send_to(payload, target))
                .map_err(|e| {
                    // TODO(https://github.com/rust-lang/rust/issues/86442): once
                    // std::io::ErrorKind::HostUnreachable is stable, we should use that instead.
                    match e.raw_os_error() {
                        Some(libc::EHOSTUNREACH) => fntr::Error::AddressUnreachable,
                        Some(_) | None => {
                            error!("error while sending udp datagram to {:?}: {:?}", target, e);
                            fntr::Error::Internal
                        }
                    }
                }),
            rx_buffer,
            |rx_buffer, num_bytes_sent| async move {
                if num_bytes_sent < payload.len() {
                    error!(
                        "expected to send full payload length {}, sent {} bytes instead",
                        payload.len(),
                        num_bytes_sent
                    );
                    return Err(fntr::Error::Internal);
                }

                let timelimited_socket_receive = socket
                    .recv_from(rx_buffer)
                    .map(Ok)
                    .or(fasync::Timer::new(timeout).map(Err))
                    .await;

                match timelimited_socket_receive
                {
                    Ok(received_result) => {
                        let (received, from_addr) = received_result.map_err(|e| {
                            error!("error while receiving udp datagram: {:?}", e);
                            fntr::Error::Internal
                        })?;
                        if from_addr != target {
                            warn!(
                                "received udp datagram from {:?} while listening for datagrams\
                                 from {:?}",
                                from_addr,
                                target,
                            );
                            return Ok(async_utils::fold::FoldWhile::Continue(rx_buffer));
                        }
                        Ok(async_utils::fold::FoldWhile::Done(received))
                    }
                    Err((/* timed out */)) => {
                        Ok(async_utils::fold::FoldWhile::Continue(rx_buffer))
                    }
                }
            },
        )
        .await?;

        match fold_result {
            async_utils::fold::FoldResult::StreamEnded(_rx_buffer) => {
                Err(fntr::Error::TimeoutExceeded)
            }
            async_utils::fold::FoldResult::ShortCircuited(num_bytes_received) => {
                Ok(num_bytes_received)
            }
        }
    }

    /// Pings the `target` using a socket created on the hermetic Netstack.
    async fn ping(
        &self,
        target: fnet::IpAddress,
        payload_length: u16,
        interface_name: Option<String>,
        timeout: zx::Duration,
    ) -> Result<(), fntr::Error> {
        let hermetic_network_connector = self
            .hermetic_network_connector
            .as_ref()
            .ok_or(fntr::Error::HermeticNetworkRealmNotRunning)?;

        let fnet_ext::IpAddress(target) = target.into();
        const UNSPECIFIED_PORT: u16 = 0;
        match target {
            std::net::IpAddr::V4(addr) => {
                ping_once::<ping::Ipv4>(
                    std::net::SocketAddrV4::new(addr, UNSPECIFIED_PORT),
                    payload_length.into(),
                    interface_name,
                    timeout,
                    hermetic_network_connector,
                )
                .await
            }
            std::net::IpAddr::V6(addr) => {
                const DEFAULT_FLOW_INFO: u32 = 0;
                let scope_id =
                    get_interface_scope_id(&interface_name, &addr, hermetic_network_connector)
                        .await?;
                ping_once::<ping::Ipv6>(
                    std::net::SocketAddrV6::new(
                        addr,
                        UNSPECIFIED_PORT,
                        DEFAULT_FLOW_INFO,
                        scope_id,
                    ),
                    payload_length.into(),
                    interface_name,
                    timeout,
                    hermetic_network_connector,
                )
                .await
            }
        }
    }

    /// Adds an interface to the hermetic Netstack.
    ///
    /// An interface will only be added if the system has an interface with a
    /// matching `mac_address`. The added interface will have the provided
    /// `name`. Additionally, the matching interface will be disabled on the
    /// system's Netstack.
    async fn add_interface(
        &mut self,
        mac_address: fnet_ext::MacAddress,
        name: &str,
    ) -> Result<(), fntr::Error> {
        // A hermetic Netstack must be running for an interface to be
        // added.
        let hermetic_network_connector = self
            .hermetic_network_connector
            .as_ref()
            .ok_or(fntr::Error::HermeticNetworkRealmNotRunning)?;

        let interface_id_to_disable = find_enabled_interface_id(mac_address).await?;
        install_interface(name, mac_address, hermetic_network_connector).await?;

        if let Some(interface_id_to_disable) = interface_id_to_disable {
            // Disable the matching interface on the system's Netstack.
            disable_interface(interface_id_to_disable, &SystemConnector).await?;
            self.mutated_interface_ids.push(interface_id_to_disable);
        }
        Ok(())
    }

    /// Tears down the "hermetic-network" realm.
    ///
    /// Any interfaces that were previously disabled by the controller on the
    /// system's Netstack will be re-enabled. Returns an error if there is not
    /// a running "hermetic-network" realm.
    async fn stop_hermetic_network_realm(&mut self) -> Result<(), fntr::Error> {
        destroy_child(
            network_test_realm::create_hermetic_network_realm_child_ref(),
            &SystemConnector,
        )
        .await
        .map_err(|e| match e {
            DestroyChildError::NotRunning => {
                self.hermetic_network_connector = None;
                fntr::Error::HermeticNetworkRealmNotRunning
            }
            DestroyChildError::Internal => fntr::Error::Internal,
        })?;

        // Attempt to re-enable all previously disabled interfaces on the
        // system's Netstack. If the controller fails to re-enable any of them,
        // then an error is logged but not returned. Re-enabling interfaces is
        // done on a best-effort basis.
        futures::stream::iter(self.mutated_interface_ids.drain(..))
            .for_each_concurrent(None, |id| async move {
                enable_interface(id, &SystemConnector).await.unwrap_or_else(|e| {
                    warn!("failed to re-enable interface id: {} with erorr: {:?}", id, e)
                })
            })
            .await;
        self.hermetic_network_connector = None;
        self.multicast_v4_socket = None;
        self.multicast_v6_socket = None;
        Ok(())
    }

    /// Starts the "hermetic-network" realm with the provided `netstack`.
    ///
    /// Adds the "hermetic-network" component to the "enclosed-network"
    /// collection.
    async fn start_hermetic_network_realm(
        &mut self,
        netstack: fntr::Netstack,
    ) -> Result<(), fntr::Error> {
        if let Some(_hermetic_network_connector) = &self.hermetic_network_connector {
            // The `Controller` only configures one hermetic network realm
            // at a time. As a result, any existing realm must be stopped before
            // a new one is started.
            self.stop_hermetic_network_realm().await.map_err(|e| match e {
                fntr::Error::HermeticNetworkRealmNotRunning => {
                    panic!("attempted to stop hermetic network realm that was not running")
                }
                fntr::Error::AddressInUse
                | fntr::Error::AddressNotAvailable
                | fntr::Error::AddressUnreachable
                | fntr::Error::AlreadyExists
                | fntr::Error::ComponentNotFound
                | fntr::Error::Internal
                | fntr::Error::InterfaceNotFound
                | fntr::Error::InvalidArguments
                | fntr::Error::PingFailed
                | fntr::Error::StubNotRunning
                | fntr::Error::TimeoutExceeded => e,
            })?;
        }

        let url = match netstack {
            fntr::Netstack::V2 => HERMETIC_NETWORK_V2_URL,
        };

        create_child(
            fdecl::CollectionRef {
                name: network_test_realm::HERMETIC_NETWORK_COLLECTION_NAME.to_string(),
            },
            create_child_decl(network_test_realm::HERMETIC_NETWORK_REALM_NAME, url),
            &SystemConnector,
        )
        .await?;

        self.hermetic_network_connector = Some(HermeticNetworkConnector::new().await?);
        Ok(())
    }
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let mut fs = fuchsia_component::server::ServiceFs::new_local();
    let _: &mut fuchsia_component::server::ServiceFsDir<'_, _> =
        fs.dir("svc").add_fidl_service(|s: fntr::ControllerRequestStream| s);

    let _: &mut fuchsia_component::server::ServiceFs<_> =
        fs.take_and_serve_directory_handle().context("failed to serve ServiceFs directory")?;

    let mut controller = Controller::new();

    let mut requests = fs.fuse().flatten_unordered();

    while let Some(controller_request) = requests.next().await {
        futures::future::ready(controller_request)
            .and_then(|req| controller.handle_request(req))
            .await
            .unwrap_or_else(|e| {
                if !fidl::Error::is_closed(&e) {
                    error!("handle_request failed: {:?}", e);
                } else {
                    warn!("handle_request closed: {:?}", e);
                }
            });
    }

    unreachable!("Stopped serving requests");
}
