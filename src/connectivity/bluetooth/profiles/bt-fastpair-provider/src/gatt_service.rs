// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use core::{
    pin::Pin,
    task::{Context, Poll},
};
use fidl::endpoints::{create_request_stream, ControlHandle, RequestStream};
use fidl_fuchsia_bluetooth_gatt2::{
    self as gatt, AttributePermissions, Characteristic, CharacteristicPropertyBits, Handle,
    LocalServiceMarker, LocalServiceReadValueResponder, LocalServiceRequest,
    LocalServiceRequestStream, LocalServiceWriteValueResponder, SecurityRequirements,
    Server_Marker, Server_Proxy, ServiceInfo, WriteValueParameters,
};
use fuchsia_bluetooth::types::{PeerId, Uuid};
use fuchsia_component::client::connect_to_protocol;
use futures::ready;
use futures::stream::{FusedStream, Stream, StreamExt};
use std::str::FromStr;
use tracing::{trace, warn};

use crate::config::Config;
use crate::error::Error;

/// The UUID of the Fast Pair Service.
pub const FAST_PAIR_SERVICE_UUID: u16 = 0xFE2C;

/// Custom characteristic - Model ID.
const MODEL_ID_CHARACTERISTIC_UUID: &str = "FE2C1233-8366-4814-8EB0-01DE32100BEA";
/// Fixed Handle assigned to the Model ID characteristic.
const MODEL_ID_CHARACTERISTIC_HANDLE: Handle = Handle { value: 1 };

/// Custom characteristic - Key-based pairing.
const KEY_BASED_PAIRING_CHARACTERISTIC_UUID: &str = "FE2C1234-8366-4814-8EB0-01DE32100BEA";
/// Fixed Handle assigned to the Model ID characteristic.
const KEY_BASED_PAIRING_CHARACTERISTIC_HANDLE: Handle = Handle { value: 2 };

/// Custom characteristic - Passkey.
const PASSKEY_CHARACTERISTIC_UUID: &str = "FE2C1235-8366-4814-8EB0-01DE32100BEA";
/// Fixed Handle assigned to the Model ID characteristic.
const PASSKEY_CHARACTERISTIC_HANDLE: Handle = Handle { value: 3 };

/// Custom characteristic - Account Key.
const ACCOUNT_KEY_CHARACTERISTIC_UUID: &str = "FE2C1236-8366-4814-8EB0-01DE32100BEA";
/// Fixed Handle assigned to the Model ID characteristic.
const ACCOUNT_KEY_CHARACTERISTIC_HANDLE: Handle = Handle { value: 4 };

/// Standard characteristic - the firmware revision of the device.
const FIRMWARE_REVISION_CHARACTERISTIC_UUID: u16 = 0x2A26;
/// Fixed Handle assigned to the Model ID characteristic.
const FIRMWARE_REVISION_CHARACTERISTIC_HANDLE: Handle = Handle { value: 5 };

pub enum GattRequest {
    /// A request to initiate the key-based pairing procedure.
    KeyBasedPairing {
        /// The ID of the remote peer that initiated the key-based pairing request.
        peer_id: PeerId,
        /// The encrypted payload containing the details of the request.
        encrypted_request: Vec<u8>,
        /// A responder used to acknowledge the handling of the pairing request.
        response: Box<dyn FnOnce(Result<(), gatt::Error>)>,
    },

    /// A request to write an Account Key to the local device.
    WriteAccountKey {
        /// The ID of the remote peer that initiated the key-based pairing request.
        peer_id: PeerId,
        /// The encrypted payload containing the Account Key to be written.
        encrypted_account_key: Vec<u8>,
        /// A responder used to acknowledge the handling of the write request.
        response: Box<dyn FnOnce(Result<(), gatt::Error>)>,
    },

    /// A request to compare the remote peer's passkey with that of the local device.
    VerifyPasskey {
        /// The ID of the remote peer that wants to verify the pairing Passkey.
        peer_id: PeerId,
        /// The encrypted payload containing the peer's (Seeker) Passkey.
        encrypted_passkey: Vec<u8>,
        /// A responder used to acknowledge the handling of the verification request.
        response: Box<dyn FnOnce(Result<(), gatt::Error>)>,
    },
}

impl std::fmt::Debug for GattRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let output = match &self {
            Self::KeyBasedPairing { peer_id, .. } => {
                format!("KeyBasedPairing({:?})", peer_id)
            }
            Self::WriteAccountKey { peer_id, .. } => {
                format!("WriteAccountKey({:?})", peer_id)
            }
            Self::VerifyPasskey { peer_id, .. } => {
                format!("VerifyPasskey({:?})", peer_id)
            }
        };
        write!(f, "{}", output)
    }
}

/// Represents a published Fast Pair Provider GATT service.
pub struct GattService {
    /// The connection to the `fuchsia.bluetooth.gatt2.Server` capability.
    server_svc: Server_Proxy,
    /// The stream associated with the published Fast Pair GATT Service. Receives GATT requests
    /// initiated by the remote peer.
    local_service_server: LocalServiceRequestStream,
    /// The configuration of the local Fast Pair component.
    config: Config,
}

impl std::fmt::Debug for GattService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GattService").field("server_svc", &self.server_svc).finish()
    }
}

impl GattService {
    /// Builds and returns a published Fast Pair Provider GATT service.
    pub async fn new(config: Config) -> Result<Self, Error> {
        let gatt_server_proxy =
            connect_to_protocol::<Server_Marker>().context("Can't connect to gatt2.Server")?;
        Self::from_proxy(gatt_server_proxy, config).await
    }

    pub async fn from_proxy(proxy: Server_Proxy, config: Config) -> Result<Self, Error> {
        let local_service = Self::publish_service(&proxy).await?;
        Ok(Self { server_svc: proxy, local_service_server: local_service, config })
    }

    // Builds and returns the characteristics associated with the Fast Pair Provider GATT service.
    fn characteristics() -> Vec<Characteristic> {
        // There are 5 characteristics that are published in this service.

        // 0. Model ID - This only supports read with no specific security requirements.
        let model_id = Characteristic {
            handle: Some(MODEL_ID_CHARACTERISTIC_HANDLE),
            type_: Uuid::from_str(MODEL_ID_CHARACTERISTIC_UUID).ok().map(Into::into),
            properties: Some(CharacteristicPropertyBits::READ.bits().into()),
            permissions: Some(AttributePermissions {
                read: Some(SecurityRequirements::EMPTY),
                ..AttributePermissions::EMPTY
            }),
            ..Characteristic::EMPTY
        };

        // 1. Key-based Pairing - This supports write & notify with no specific security
        //    requirements.
        let key_based_pairing = Characteristic {
            handle: Some(KEY_BASED_PAIRING_CHARACTERISTIC_HANDLE),
            type_: Uuid::from_str(KEY_BASED_PAIRING_CHARACTERISTIC_UUID).ok().map(Into::into),
            properties: Some(
                (CharacteristicPropertyBits::WRITE | CharacteristicPropertyBits::NOTIFY)
                    .bits()
                    .into(),
            ),
            permissions: Some(AttributePermissions {
                write: Some(SecurityRequirements::EMPTY),
                update: Some(SecurityRequirements::EMPTY),
                ..AttributePermissions::EMPTY
            }),
            ..Characteristic::EMPTY
        };

        // 2. Passkey - This supports write & notify with no specific security requirements.
        let passkey = Characteristic {
            handle: Some(PASSKEY_CHARACTERISTIC_HANDLE),
            type_: Uuid::from_str(PASSKEY_CHARACTERISTIC_UUID).ok().map(Into::into),
            properties: Some(
                (CharacteristicPropertyBits::WRITE | CharacteristicPropertyBits::NOTIFY)
                    .bits()
                    .into(),
            ),
            permissions: Some(AttributePermissions {
                write: Some(SecurityRequirements::EMPTY),
                update: Some(SecurityRequirements::EMPTY),
                ..AttributePermissions::EMPTY
            }),
            ..Characteristic::EMPTY
        };

        // 3. Account Key - This only supports write with no specific security requirements.
        let account_key = Characteristic {
            handle: Some(ACCOUNT_KEY_CHARACTERISTIC_HANDLE),
            type_: Uuid::from_str(ACCOUNT_KEY_CHARACTERISTIC_UUID).ok().map(Into::into),
            properties: Some(CharacteristicPropertyBits::WRITE.bits().into()),
            permissions: Some(AttributePermissions {
                write: Some(SecurityRequirements::EMPTY),
                ..AttributePermissions::EMPTY
            }),
            ..Characteristic::EMPTY
        };

        // 4. Firmware Revision - This only supports read with no specific security requirements.
        let firmware_revision = Characteristic {
            handle: Some(FIRMWARE_REVISION_CHARACTERISTIC_HANDLE),
            type_: Some(Uuid::new16(FIRMWARE_REVISION_CHARACTERISTIC_UUID).into()),
            properties: Some(CharacteristicPropertyBits::READ.bits().into()),
            permissions: Some(AttributePermissions {
                read: Some(SecurityRequirements::EMPTY),
                ..AttributePermissions::EMPTY
            }),
            ..Characteristic::EMPTY
        };

        vec![model_id, key_based_pairing, passkey, account_key, firmware_revision]
    }

    // Returns the GATT service definition for the Fast Pair Provider role.
    fn service_info() -> ServiceInfo {
        ServiceInfo {
            primary: Some(true),
            type_: Some(Uuid::new16(FAST_PAIR_SERVICE_UUID).into()),
            characteristics: Some(Self::characteristics()),
            ..ServiceInfo::EMPTY
        }
    }

    /// Publishes a Fast Pair Provider GATT service using the provided `server_svc`.
    ///
    /// Returns the server end of the published service.
    async fn publish_service(
        server_svc: &Server_Proxy,
    ) -> Result<LocalServiceRequestStream, Error> {
        // Build the GATT service.
        let info = Self::service_info();
        let (service_client, service_stream) = create_request_stream::<LocalServiceMarker>()
            .context("Can't create LocalService endpoints")?;

        if let Err(e) = server_svc.publish_service(info, service_client).await? {
            warn!("Couldn't set up Fast Pair GATT Service: {:?}", e);
            return Err(e.into());
        }

        Ok(service_stream)
    }

    /// Handle an incoming GATT read request for the local GATT characteristic at `handle`.
    fn handle_read_request(&self, handle: Handle, responder: LocalServiceReadValueResponder) {
        match handle {
            MODEL_ID_CHARACTERISTIC_HANDLE => {
                let model_id_bytes: [u8; 3] = self.config.model_id.into();
                let _ = responder.send(&mut Ok(model_id_bytes.to_vec()));
            }
            FIRMWARE_REVISION_CHARACTERISTIC_HANDLE => {
                let firmware_revision_bytes = self.config.firmware_revision.clone().into_bytes();
                let _ = responder.send(&mut Ok(firmware_revision_bytes));
            }
            h => {
                warn!("Received unsupported read request for handle: {:?}", h);
                let _ = responder.send(&mut Err(gatt::Error::InvalidHandle));
            }
        }
    }

    fn handle_write_request(
        &self,
        params: WriteValueParameters,
        responder: LocalServiceWriteValueResponder,
    ) -> Option<GattRequest> {
        // All the fields of the `WriteValueParameters` must be provided.
        let (peer_id, handle, value) = if let WriteValueParameters {
            peer_id: Some(peer_id),
            handle: Some(handle),
            offset: Some(0),
            value: Some(value),
            ..
        } = params
        {
            (peer_id.into(), handle, value)
        } else {
            let _ = responder.send(&mut Err(gatt::Error::InvalidParameters));
            return None;
        };

        let response: Box<dyn FnOnce(Result<(), gatt::Error>)> = Box::new(|mut result| {
            let _ = responder.send(&mut result);
        });
        match handle {
            KEY_BASED_PAIRING_CHARACTERISTIC_HANDLE => {
                Some(GattRequest::KeyBasedPairing { peer_id, encrypted_request: value, response })
            }
            PASSKEY_CHARACTERISTIC_HANDLE => {
                Some(GattRequest::VerifyPasskey { peer_id, encrypted_passkey: value, response })
            }
            ACCOUNT_KEY_CHARACTERISTIC_HANDLE => Some(GattRequest::WriteAccountKey {
                peer_id,
                encrypted_account_key: value,
                response,
            }),
            h => {
                warn!("Received unsupported write request for handle: {:?}", h);
                response(Err(gatt::Error::InvalidHandle));
                None
            }
        }
    }

    /// Handle an incoming FIDL request for the local GATT service.
    fn handle_service_request(&mut self, request: LocalServiceRequest) -> Option<GattRequest> {
        // TODO(fxbug.dev/95542): Handle each GATT request type.
        match request {
            LocalServiceRequest::ReadValue { handle, responder, .. } => {
                self.handle_read_request(handle, responder);
            }
            LocalServiceRequest::WriteValue { params, responder, .. } => {
                return self.handle_write_request(params, responder);
            }
            LocalServiceRequest::CharacteristicConfiguration { responder, .. } => {
                let _ = responder.send();
            }
            LocalServiceRequest::ValueChangedCredit { .. } => {}
        }
        None
    }
}

impl Stream for GattService {
    type Item = GattRequest;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Keep polling the request stream until it produces a request that should be returned or it
        // produces Poll::Pending.
        loop {
            let result = ready!(self.local_service_server.poll_next_unpin(cx));

            let result = match result {
                Some(Ok(request)) => match self.handle_service_request(request) {
                    None => continue,
                    request => request,
                },
                Some(Err(e)) => {
                    warn!("Error in LocalService FIDL client request: {}.", e);
                    None
                }
                None => None,
            };
            // Either the `LocalService` stream is exhausted, or there is an error in the channel.
            if result.is_none() {
                trace!("Closing LocalService connection");
                self.local_service_server.control_handle().shutdown();
            }

            return Poll::Ready(result);
        }
    }
}

impl FusedStream for GattService {
    fn is_terminated(&self) -> bool {
        self.local_service_server.is_terminated()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use anyhow::format_err;
    use assert_matches::assert_matches;
    use async_test_helpers::run_while;
    use async_utils::PollExt;
    use fidl_fuchsia_bluetooth_gatt2::LocalServiceProxy;
    use fuchsia_async as fasync;
    use fuchsia_bluetooth::types::PeerId;
    use futures::{future::Either, pin_mut, stream::StreamExt};

    #[fuchsia::test]
    fn gatt_service_is_received_by_upstream_server() {
        let mut exec = fasync::TestExecutor::new().unwrap();

        let (gatt_client, mut gatt_server) =
            fidl::endpoints::create_proxy_and_stream::<Server_Marker>().unwrap();
        let gatt_server_fut = gatt_server.next();
        pin_mut!(gatt_server_fut);
        let _ = exec.run_until_stalled(&mut gatt_server_fut).expect_pending("Upstream still ok");

        let publish_fut = GattService::from_proxy(gatt_client, Config::example_config());
        pin_mut!(publish_fut);
        let _ =
            exec.run_until_stalled(&mut publish_fut).expect_pending("Waiting for publish response");

        // Expect the upstream server to receive the publish request.
        let (_info, _service_client, responder) =
            match exec.run_until_stalled(&mut gatt_server_fut).expect("stream is ready") {
                Some(Ok(req)) => req.into_publish_service().expect("only possible request"),
                x => panic!("Expected ready request but got: {:?}", x),
            };
        // Upstream server responds positively.
        let _ = responder.send(&mut Ok({}));

        // Publish service request should resolve successfully.
        let publish_result =
            exec.run_until_stalled(&mut publish_fut).expect("publish response received");
        assert_matches!(publish_result, Ok(_));
    }

    /// Builds the `GattService` by publishing the Fast Pair Provider service definition.
    /// Returns the `GattService` and the connection to the published local service.
    pub(crate) async fn setup_gatt_service() -> (GattService, LocalServiceProxy) {
        let (gatt_server_client, mut gatt_server) =
            fidl::endpoints::create_proxy_and_stream::<Server_Marker>().unwrap();

        let publish_fut = GattService::from_proxy(gatt_server_client, Config::example_config());
        let gatt_server_fut = gatt_server.select_next_some();
        pin_mut!(publish_fut);
        pin_mut!(gatt_server_fut);

        match futures::future::select(publish_fut, gatt_server_fut).await {
            Either::Left(_) => panic!("Publish future resolved before GATT server responded"),
            Either::Right((server_result, publish_fut)) => {
                let (_info, local_service_client, responder) = server_result
                    .expect("valid FIDL request")
                    .into_publish_service()
                    .expect("only possible request");
                // Respond positively.
                let _ = responder.send(&mut Ok({}));
                // Publish service request should resolve.
                let gatt_service = publish_fut.await.expect("should resolve ok");
                (gatt_service, local_service_client.into_proxy().unwrap())
            }
        }
    }

    #[fuchsia::test]
    fn gatt_service_stream_impl_terminates() {
        let mut exec = fasync::TestExecutor::new().unwrap();
        let (gatt_service, _upstream_service_client) =
            exec.run_singlethreaded(setup_gatt_service());
        pin_mut!(gatt_service);

        assert!(!gatt_service.is_terminated());
        let _ =
            exec.run_until_stalled(&mut gatt_service.next()).expect_pending("stream still active");

        // Upstream terminates the connection for the local service.
        drop(_upstream_service_client);

        // The stream should be exhausted.
        let result =
            exec.run_until_stalled(&mut gatt_service.next()).expect("stream item should resolve");
        assert_matches!(result, None);
        assert!(gatt_service.is_terminated());
    }

    #[fuchsia::test]
    fn read_model_id_characteristic_success() {
        let mut exec = fasync::TestExecutor::new().unwrap();
        let (mut gatt_service, upstream_service_client) =
            exec.run_singlethreaded(setup_gatt_service());

        // Simulate a peer's read request for the Model ID.
        let mut handle = MODEL_ID_CHARACTERISTIC_HANDLE.clone();
        let read_request_fut = upstream_service_client.read_value(
            &mut PeerId(123).into(),
            &mut handle,
            /* offset */ 0,
        );
        pin_mut!(read_request_fut);
        let _ = exec
            .run_until_stalled(&mut read_request_fut)
            .expect_pending("waiting for FIDL response");

        // Read request should be received by the GATT Server. No additional information is needed
        // so no stream items.
        let _ = exec
            .run_until_stalled(&mut gatt_service.next())
            .expect_pending("stream active with no items");

        // A 24-bit Model ID of 1 is used in `setup_gatt_service` (see Config::example_config).
        let expected_model_id_bytes = vec![0x00, 0x00, 0x01];
        let read_result = exec
            .run_until_stalled(&mut read_request_fut)
            .expect("response is ready")
            .expect("fidl result is Ok");
        assert_eq!(read_result, Ok(expected_model_id_bytes));
    }

    #[fuchsia::test]
    fn read_firmware_revision_characteristic_success() {
        let mut exec = fasync::TestExecutor::new().unwrap();
        let (mut gatt_service, upstream_service_client) =
            exec.run_singlethreaded(setup_gatt_service());

        // Simulate a peer's read request for the Model ID.
        let mut handle = FIRMWARE_REVISION_CHARACTERISTIC_HANDLE.clone();
        let read_request_fut = upstream_service_client.read_value(
            &mut PeerId(123).into(),
            &mut handle,
            /* offset */ 0,
        );
        pin_mut!(read_request_fut);
        let _ = exec
            .run_until_stalled(&mut read_request_fut)
            .expect_pending("waiting for FIDL response");

        // Read request should be received by the GATT Server. No additional information is needed
        // so no stream items.
        let _ = exec
            .run_until_stalled(&mut gatt_service.next())
            .expect_pending("stream active with no items");

        // A version of "1.0.0" is specified in `Config::example_config`.
        let expected_version_bytes = vec![0x31, 0x2E, 0x30, 0x2E, 0x30];
        let read_result = exec
            .run_until_stalled(&mut read_request_fut)
            .expect("response is ready")
            .expect("fidl result is Ok");
        assert_eq!(read_result, Ok(expected_version_bytes));
    }

    #[fuchsia::test]
    fn read_invalid_characteristic_returns_error() {
        let mut exec = fasync::TestExecutor::new().unwrap();
        let (mut gatt_service, upstream_service_client) =
            exec.run_singlethreaded(setup_gatt_service());

        // Simulate a peer's read request for a random, unsupported, characteristic.
        let read_request_fut = upstream_service_client.read_value(
            &mut PeerId(123).into(),
            &mut Handle { value: 999 },
            /* offset */ 0,
        );
        pin_mut!(read_request_fut);
        let _ = exec
            .run_until_stalled(&mut read_request_fut)
            .expect_pending("waiting for FIDL response");

        // Read request should be received by the GATT Server. Should be handled with no further
        // action.
        let _ = exec
            .run_until_stalled(&mut gatt_service.next())
            .expect_pending("stream active with no items");

        // Client end of GATT connection should receive the error.
        let read_result = exec
            .run_until_stalled(&mut read_request_fut)
            .expect("response is ready")
            .expect("fidl result is Ok");
        assert_matches!(read_result, Err(gatt::Error::InvalidHandle));
    }

    #[fuchsia::test]
    fn write_requests_with_invalid_handle_is_handled_gracefully() {
        let mut exec = fasync::TestExecutor::new().unwrap();
        let (mut gatt_service, upstream_service_client) =
            exec.run_singlethreaded(setup_gatt_service());
        let gatt_service_fut = gatt_service.next();
        pin_mut!(gatt_service_fut);

        // Model ID is a valid characteristic, but writes are not supported.
        let params = WriteValueParameters {
            peer_id: Some(PeerId(123).into()),
            handle: Some(MODEL_ID_CHARACTERISTIC_HANDLE),
            offset: Some(0),
            value: Some(vec![0x00, 0x01, 0x02]),
            ..WriteValueParameters::EMPTY
        };
        let write_request_fut = upstream_service_client.write_value(params);
        pin_mut!(write_request_fut);
        // We expect an Error to be returned to the FIDL client. Additionally, no `GattService`
        // stream items should be produced. This is verified indirectly via `run_while` which will
        // panic if the `BackgroundFut` (`gatt_service_fut`) finishes.
        let (write_result, gatt_service_fut) =
            run_while(&mut exec, gatt_service_fut, write_request_fut);
        assert_matches!(write_result, Ok(Err(gatt::Error::InvalidHandle)));

        // Random characteristic handle that is not supported by this GATT server.
        let params = WriteValueParameters {
            peer_id: Some(PeerId(123).into()),
            handle: Some(Handle { value: 999 }),
            offset: Some(0),
            value: Some(vec![0x00, 0x01, 0x02]),
            ..WriteValueParameters::EMPTY
        };
        let write_request_fut = upstream_service_client.write_value(params);
        pin_mut!(write_request_fut);
        let (write_result, _gatt_service_fut) =
            run_while(&mut exec, gatt_service_fut, write_request_fut);
        assert_matches!(write_result, Ok(Err(gatt::Error::InvalidHandle)));
    }

    type Responder = Box<dyn FnOnce(Result<(), gatt::Error>)>;
    type GattRequestMatcher = fn(GattRequest) -> Result<Responder, anyhow::Error>;
    fn characteristic_write_results_in_expected_stream_item(
        characteristic_handle: Handle,
        matcher: GattRequestMatcher,
    ) {
        let mut exec = fasync::TestExecutor::new().unwrap();
        let (mut gatt_service, upstream_service_client) =
            exec.run_singlethreaded(setup_gatt_service());

        let params = WriteValueParameters {
            peer_id: Some(PeerId(123).into()),
            handle: Some(characteristic_handle),
            offset: Some(0),
            value: Some(vec![0x00, 0x01, 0x02]),
            ..WriteValueParameters::EMPTY
        };
        let write_request_fut = upstream_service_client.write_value(params);
        pin_mut!(write_request_fut);
        let _ = exec
            .run_until_stalled(&mut write_request_fut)
            .expect_pending("waiting for FIDL response");

        // The write request should be received by the GATT Server and converted into the expected
        // GattRequest variant.
        let request = exec
            .run_until_stalled(&mut gatt_service.next())
            .expect("stream should yield item")
            .expect("stream item should not be none");
        let responder = matcher(request).expect("wrong GATT request");

        // FIDL request only resolves when the stream item is handled (e.g responded to).
        responder(Ok(()));
        let write_result = exec
            .run_until_stalled(&mut write_request_fut)
            .expect("response is ready")
            .expect("FIDL result is Ok");
        assert_eq!(write_result, Ok(()));
    }

    #[fuchsia::test]
    fn key_based_pairing_write_results_in_stream_item() {
        characteristic_write_results_in_expected_stream_item(
            KEY_BASED_PAIRING_CHARACTERISTIC_HANDLE,
            |gatt_req| match gatt_req {
                GattRequest::KeyBasedPairing { response, .. } => Ok(response),
                req => Err(format_err!("Expected key-based pairing write, got: {:?}", req)),
            },
        );
    }

    #[fuchsia::test]
    fn account_key_write_results_in_stream_item() {
        characteristic_write_results_in_expected_stream_item(
            ACCOUNT_KEY_CHARACTERISTIC_HANDLE,
            |gatt_req| match gatt_req {
                GattRequest::WriteAccountKey { response, .. } => Ok(response),
                req => Err(format_err!("Expected account key write, got: {:?}", req)),
            },
        );
    }

    #[fuchsia::test]
    fn verify_passkey_write_results_in_stream_item() {
        characteristic_write_results_in_expected_stream_item(
            PASSKEY_CHARACTERISTIC_HANDLE,
            |gatt_req| match gatt_req {
                GattRequest::VerifyPasskey { response, .. } => Ok(response),
                req => Err(format_err!("Expected passkey write, got: {:?}", req)),
            },
        );
    }

    #[fuchsia::test]
    fn gatt_write_with_nonzero_offset_returns_error() {
        let mut exec = fasync::TestExecutor::new().unwrap();
        let (mut gatt_service, upstream_service_client) =
            exec.run_singlethreaded(setup_gatt_service());

        let params = WriteValueParameters {
            peer_id: Some(PeerId(123).into()),
            handle: Some(PASSKEY_CHARACTERISTIC_HANDLE),
            offset: Some(10), // Nonzero is not supported.
            value: Some(vec![0x00, 0x01, 0x02]),
            ..WriteValueParameters::EMPTY
        };
        let write_request_fut = upstream_service_client.write_value(params);
        pin_mut!(write_request_fut);
        let _ = exec
            .run_until_stalled(&mut write_request_fut)
            .expect_pending("waiting for FIDL response");

        // Write request should be received by the GATT Server - request to verify passkey but with
        // an invalid parameter. Should be rejected and no GATT stream item.
        let _ = exec.run_until_stalled(&mut gatt_service.next()).expect_pending("No stream item");

        let write_result = exec
            .run_until_stalled(&mut write_request_fut)
            .expect("response is ready")
            .expect("FIDL result is Ok");
        assert_eq!(write_result, Err(gatt::Error::InvalidParameters));
    }
}
