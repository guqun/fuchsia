// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.test/cpp/wire.h>
#include <lib/service/llcpp/service.h>
#include <lib/syslog/global.h>

int main() {
  auto client_end = service::Connect<fuchsia_driver_test::Realm>();
  if (!client_end.is_ok()) {
    FX_LOGF(ERROR, "platform_driver_test_realm", "Failed to connect to Realm FIDL: %d",
            client_end.error_value());
    return 1;
  }
  auto client = fidl::BindSyncClient(std::move(*client_end));

  fidl::Arena arena;
  fuchsia_driver_test::wire::RealmArgs args(arena);
  args.set_root_driver(arena, fidl::StringView("fuchsia-boot:///#driver/platform-bus.so"));
  auto wire_result = client->Start(std::move(args));
  if (wire_result.status() != ZX_OK) {
    FX_LOGF(ERROR, "platform_driver_test_realm", "Failed to call to Realm:Start: %d",
            wire_result.status());
    return 1;
  }
  if (wire_result.value_NEW().is_error()) {
    FX_LOGF(ERROR, "platform_driver_test_realm", "Realm:Start failed: %d",
            wire_result.value_NEW().error_value());
    return 1;
  }

  return 0;
}
