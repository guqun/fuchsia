// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.gpu.magma/cpp/wire.h>
#include <lib/zx/channel.h>

#include "magma_util/dlog.h"
#include "platform_connection_client.h"
#include "platform_device_client.h"

namespace magma {
class ZirconPlatformDeviceClient : public PlatformDeviceClient {
 public:
  ZirconPlatformDeviceClient(magma_handle_t handle) : channel_(handle) {}

  std::unique_ptr<PlatformConnectionClient> Connect() {
    uint64_t inflight_params = 0;

    if (!Query(MAGMA_QUERY_MAXIMUM_INFLIGHT_PARAMS, nullptr, &inflight_params))
      return DRETP(nullptr, "Query(MAGMA_QUERY_MAXIMUM_INFLIGHT_PARAMS) failed");

    auto endpoints = fidl::CreateEndpoints<fuchsia_gpu_magma::Primary>();
    if (!endpoints.is_ok())
      return DRETP(nullptr, "Failed to create primary endpoints");

    zx::channel client_notification_endpoint, server_notification_endpoint;
    zx_status_t status =
        zx::channel::create(0, &server_notification_endpoint, &client_notification_endpoint);
    if (status != ZX_OK)
      return DRETP(nullptr, "zx::channel::create failed");

    auto result = fidl::WireCall<fuchsia_gpu_magma::Device>(channel_.borrow())
                      ->Connect2(magma::PlatformThreadId().id(), std::move(endpoints->server),
                                 std::move(server_notification_endpoint));

    if (result.status() != ZX_OK)
      return DRETP(nullptr, "magma_DeviceConnect2 failed: %d", result.status());

    uint64_t max_inflight_messages = magma::upper_32_bits(inflight_params);
    uint64_t max_inflight_bytes = magma::lower_32_bits(inflight_params) * 1024 * 1024;

    return magma::PlatformConnectionClient::Create(endpoints->client.channel().release(),
                                                   client_notification_endpoint.release(),
                                                   max_inflight_messages, max_inflight_bytes);
  }

  magma::Status Query(uint64_t query_id, magma_handle_t* result_buffer_out, uint64_t* result_out) {
    auto result = fidl::WireCall<fuchsia_gpu_magma::Device>(channel_.borrow())
                      ->Query(fuchsia_gpu_magma::wire::QueryId(query_id));

    if (result.status() != ZX_OK)
      return DRET_MSG(result.status(), "magma_DeviceQuery failed");

    if (result.Unwrap_NEW()->is_error())
      return DRET_MSG(result.Unwrap_NEW()->error_value(), "Got error response");

    if (result.Unwrap_NEW()->value()->is_buffer_result()) {
      if (!result_buffer_out)
        return DRET_MSG(MAGMA_STATUS_INVALID_ARGS, "Can't return query result buffer");

      *result_buffer_out = result.Unwrap_NEW()->value()->buffer_result().release();

      return MAGMA_STATUS_OK;
    }

    if (result.Unwrap_NEW()->value()->is_simple_result()) {
      if (!result_out)
        return DRET_MSG(MAGMA_STATUS_INVALID_ARGS, "Can't return query simple result");

      *result_out = result.Unwrap_NEW()->value()->simple_result();

      if (result_buffer_out)
        *result_buffer_out = ZX_HANDLE_INVALID;

      return MAGMA_STATUS_OK;
    }

    return DRET_MSG(MAGMA_STATUS_INTERNAL_ERROR, "Unknown result type");
  }

 private:
  zx::channel channel_;
};

// static
std::unique_ptr<PlatformDeviceClient> PlatformDeviceClient::Create(uint32_t handle) {
  return std::make_unique<ZirconPlatformDeviceClient>(handle);
}
}  // namespace magma
