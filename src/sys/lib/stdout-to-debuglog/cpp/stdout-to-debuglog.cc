// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.boot/cpp/wire.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fdio.h>
#include <lib/zx/debuglog.h>

namespace StdoutToDebuglog {

zx_status_t Init() {
  zx::channel local, remote;
  zx_status_t status = zx::channel::create(0, &local, &remote);
  if (status != ZX_OK) {
    return status;
  }
  status = fdio_service_connect("/svc/fuchsia.boot.WriteOnlyLog", remote.release());
  if (status != ZX_OK) {
    return status;
  }
  fidl::WireSyncClient<fuchsia_boot::WriteOnlyLog> write_only_log(std::move(local));
  auto result = write_only_log->Get();
  if (result.status() != ZX_OK) {
    return result.status();
  }
  zx::debuglog log = std::move(result.Unwrap_NEW()->log);
  for (int fd = 1; fd <= 2; ++fd) {
    zx::debuglog dup;
    zx_status_t status = log.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup);
    if (status != ZX_OK) {
      return status;
    }
    fdio_t* logger = nullptr;
    status = fdio_create(dup.release(), &logger);
    if (status != ZX_OK) {
      return status;
    }
    const int out_fd = fdio_bind_to_fd(logger, fd, 0);
    if (out_fd != fd) {
      return ZX_ERR_BAD_STATE;
    }
  }
  return ZX_OK;
}

}  // namespace StdoutToDebuglog
