// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/misc/drivers/compat/loader.h"

namespace fldsvc = fuchsia_ldsvc;

namespace compat {

Loader::Loader(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

async_dispatcher_t* Loader::dispatcher() { return dispatcher_; }

zx::status<> Loader::Bind(fidl::ClientEnd<fldsvc::Loader> client_end, zx::vmo driver_vmo) {
  if (client_) {
    return zx::error(ZX_ERR_ALREADY_BOUND);
  }
  client_.Bind(std::move(client_end), dispatcher_);
  driver_vmo_ = std::move(driver_vmo);
  return zx::ok();
}

void Loader::Done(DoneRequestView request, DoneCompleter::Sync& completer) {
  completer.Close(ZX_OK);
}

void Loader::LoadObject(LoadObjectRequestView request, LoadObjectCompleter::Sync& completer) {
  // When there is a request for the DFv1 driver library, return the
  // compatibility driver's VMO instead.
  if (request->object_name.get() == kLibDriverName) {
    if (driver_vmo_) {
      completer.Reply(ZX_OK, std::move(driver_vmo_));
    } else {
      // We have already provided and instance of driver VMO, or
      // `Loader::Bind()` has not been called.
      completer.Reply(ZX_ERR_NOT_FOUND, {});
    }
    return;
  }

  auto callback = [completer = completer.ToAsync()](
                      fidl::WireUnownedResult<fldsvc::Loader::LoadObject>& result) mutable {
    if (!result.ok()) {
      completer.Reply(result.status(), {});
      return;
    }
    completer.Reply(result.Unwrap_NEW()->rv, std::move(result.Unwrap_NEW()->object));
  };
  client_->LoadObject(request->object_name).ThenExactlyOnce(std::move(callback));
}

void Loader::Config(ConfigRequestView request, ConfigCompleter::Sync& completer) {
  auto callback = [completer = completer.ToAsync()](
                      fidl::WireUnownedResult<fldsvc::Loader::Config>& result) mutable {
    if (!result.ok()) {
      completer.Reply(result.status());
      return;
    }
    completer.Reply(result.Unwrap_NEW()->rv);
  };
  client_->Config(request->config).ThenExactlyOnce(std::move(callback));
}

void Loader::Clone(CloneRequestView request, CloneCompleter::Sync& completer) {
  fidl::BindServer(dispatcher_, std::move(request->loader), this);
  completer.Reply(ZX_OK);
}

}  // namespace compat
