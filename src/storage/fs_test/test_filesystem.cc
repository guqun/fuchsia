// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/fs_test/test_filesystem.h"

#include <lib/fdio/cpp/caller.h>
#include <lib/fidl/llcpp/channel.h>

#include "sdk/lib/syslog/cpp/macros.h"
#include "src/storage/fs_test/crypt_service.h"

namespace fs_test {

fs_management::MountOptions TestFilesystem::DefaultMountOptions() const {
  fs_management::MountOptions options;
  if (options_.blob_compression_algorithm) {
    options.write_compression_algorithm =
        blobfs::CompressionAlgorithmToString(*options_.blob_compression_algorithm);
  }
  if (GetTraits().uses_crypt)
    options.crypt_client = [] { return *GetCryptService(); };
  return options;
}

zx::status<TestFilesystem> TestFilesystem::FromInstance(
    const TestFilesystemOptions& options, std::unique_ptr<FilesystemInstance> instance) {
  static uint32_t mount_index;
  TestFilesystem filesystem(options, std::move(instance),
                            std::string("/fs_test." + std::to_string(mount_index++) + "/"));
  auto status = filesystem.Mount();
  if (status.is_error()) {
    return status.take_error();
  }
  return zx::ok(std::move(filesystem));
}

zx::status<TestFilesystem> TestFilesystem::Create(const TestFilesystemOptions& options) {
  auto instance_or = options.filesystem->Make(options);
  if (instance_or.is_error()) {
    return instance_or.take_error();
  }
  return FromInstance(options, std::move(instance_or).value());
}

zx::status<TestFilesystem> TestFilesystem::Open(const TestFilesystemOptions& options) {
  auto instance_or = options.filesystem->Open(options);
  if (instance_or.is_error()) {
    return instance_or.take_error();
  }
  return FromInstance(options, std::move(instance_or).value());
}

TestFilesystem::~TestFilesystem() {
  if (filesystem_) {
    if (mounted_) {
      auto status = Unmount();
      if (status.is_error()) {
        FX_LOGS(WARNING) << "Failed to unmount: " << status.status_string() << std::endl;
      }
    }
    rmdir(mount_path_.c_str());
  }
}

zx::status<> TestFilesystem::Mount(const fs_management::MountOptions& options) {
  auto status = filesystem_->Mount(mount_path_, options);
  if (status.is_ok()) {
    mounted_ = true;
  }
  return status;
}

zx::status<> TestFilesystem::Unmount() {
  if (!filesystem_) {
    return zx::ok();
  }
  auto status = filesystem_->Unmount(mount_path_);
  if (status.is_ok()) {
    mounted_ = false;
  }
  return status;
}

zx::status<> TestFilesystem::Fsck() { return filesystem_->Fsck(); }

zx::status<std::string> TestFilesystem::DevicePath() const { return filesystem_->DevicePath(); }

zx::status<fuchsia_io::wire::FilesystemInfo> TestFilesystem::GetFsInfo() const {
  fbl::unique_fd root_fd = fbl::unique_fd(open(mount_path().c_str(), O_RDONLY | O_DIRECTORY));
  fdio_cpp::UnownedFdioCaller root_connection(root_fd);
  const auto& result = fidl::WireCall(fidl::UnownedClientEnd<fuchsia_io::Directory>(
                                          zx::unowned_channel(root_connection.borrow_channel())))
                           ->QueryFilesystem();
  if (!result.ok()) {
    return zx::error(result.status());
  }
  if (result.value_NEW().s != ZX_OK) {
    return zx::error(result.value_NEW().s);
  }
  return zx::ok(*result.value_NEW().info);
}

}  // namespace fs_test
