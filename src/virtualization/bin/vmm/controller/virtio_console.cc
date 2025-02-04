// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/virtualization/bin/vmm/controller/virtio_console.h"

#include <lib/sys/cpp/service_directory.h>

#include "src/virtualization/bin/vmm/controller/realm_utils.h"

VirtioConsole::VirtioConsole(const PhysMem& phys_mem)
    : VirtioComponentDevice("Virtio Console", phys_mem, 0 /* device_features */,
                            fit::bind_member(this, &VirtioConsole::ConfigureQueue),
                            fit::bind_member(this, &VirtioConsole::Ready)) {
  config_.max_nr_ports = kVirtioConsoleMaxNumPorts;
}

zx_status_t VirtioConsole::Start(const zx::guest& guest, zx::socket socket,
                                 fuchsia::sys::LauncherPtr& launcher,
                                 fuchsia::component::RealmSyncPtr& realm,
                                 async_dispatcher_t* dispatcher) {
  if (launcher) {
    constexpr auto kComponentUrl =
        "fuchsia-pkg://fuchsia.com/virtio_console#meta/virtio_console.cmx";

    fuchsia::sys::LaunchInfo launch_info;
    launch_info.url = kComponentUrl;
    auto services = sys::ServiceDirectory::CreateWithRequest(&launch_info.directory_request);
    launcher->CreateComponent(std::move(launch_info), controller_.NewRequest());
    services->Connect(console_.NewRequest());
  } else {
    constexpr auto kComponentName = "virtio_console";
    constexpr auto kComponentCollectionName = "virtio_console_devices";
    constexpr auto kComponentUrl =
        "fuchsia-pkg://fuchsia.com/virtio_console#meta/virtio_console.cm";

    zx_status_t status = CreateDynamicComponent(
        realm, kComponentCollectionName, kComponentName, kComponentUrl,
        [console = console_.NewRequest()](std::shared_ptr<sys::ServiceDirectory> services) mutable {
          return services->Connect(std::move(console));
        });
    if (status != ZX_OK) {
      return status;
    }
  }

  fuchsia::virtualization::hardware::StartInfo start_info;
  zx_status_t status = PrepStart(guest, dispatcher, &start_info);
  if (status != ZX_OK) {
    return status;
  }
  return console_->Start(std::move(start_info), std::move(socket));
}

zx_status_t VirtioConsole::ConfigureQueue(uint16_t queue, uint16_t size, zx_gpaddr_t desc,
                                          zx_gpaddr_t avail, zx_gpaddr_t used) {
  return console_->ConfigureQueue(queue, size, desc, avail, used);
}

zx_status_t VirtioConsole::Ready(uint32_t negotiated_features) {
  return console_->Ready(negotiated_features);
}
