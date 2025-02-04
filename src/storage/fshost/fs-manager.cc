// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "fs-manager.h"

#include <fcntl.h>
#include <fidl/fuchsia.feedback/cpp/wire.h>
#include <fidl/fuchsia.fshost/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/wait.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/io.h>
#include <lib/inspect/service/cpp/service.h>
#include <lib/service/llcpp/service.h>
#include <lib/syslog/cpp/macros.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <threads.h>
#include <zircon/errors.h>
#include <zircon/processargs.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

#include <iterator>
#include <memory>
#include <string>
#include <string_view>
#include <utility>

#include <fbl/algorithm.h>
#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>
#include <fbl/ref_ptr.h>
#include <fbl/string.h>

#include "admin-server.h"
#include "block-watcher.h"
#include "fidl/fuchsia.ldsvc/cpp/wire.h"
#include "fshost-boot-args.h"
#include "lib/async/cpp/task.h"
#include "lifecycle.h"
#include "src/lib/storage/vfs/cpp/remote_dir.h"
#include "src/lib/storage/vfs/cpp/vfs.h"
#include "src/lib/storage/vfs/cpp/vfs_types.h"

namespace fshost {

namespace {

fbl::String ReportReasonString(fs_management::DiskFormat format, FsManager::ReportReason reason) {
  switch (reason) {
    case FsManager::ReportReason::kFsckFailure:
      return fbl::String::Concat(
          {"fuchsia-", fs_management::DiskFormatString(format), "-corruption"});
  }
}

zx::status<uint64_t> GetFsId(fidl::UnownedClientEnd<fuchsia_io::Directory> root) {
  auto result = fidl::WireCall(root)->QueryFilesystem();
  if (!result.ok())
    return zx::error(result.status());
  if (result.value_NEW().s != ZX_OK)
    return zx::error(result.value_NEW().s);
  return zx::ok(result.value_NEW().info->fs_id);
}

}  // namespace

FsManager::MountedFilesystem::~MountedFilesystem() {
  if (auto status = fs_management::Shutdown(export_root_); status.is_error())
    FX_LOGS(WARNING) << "Unmount error: " << status.status_string();
}

FsManager::FsManager(std::shared_ptr<FshostBootArgs> boot_args)
    : global_loop_(new async::Loop(&kAsyncLoopConfigNoAttachToCurrentThread)),
      vfs_(fs::ManagedVfs(global_loop_->dispatcher())),
      boot_args_(std::move(boot_args)) {}

// In the event that we haven't been explicitly signalled, tear ourself down.
FsManager::~FsManager() {
  if (!shutdown_called_) {
    Shutdown([](zx_status_t status) {
      if (status != ZX_OK) {
        FX_LOGS(ERROR) << "filesystem shutdown failed: " << zx_status_get_string(status);
        return;
      }
      FX_LOGS(INFO) << "filesystem shutdown complete";
    });
  }
  sync_completion_wait(&shutdown_, ZX_TIME_INFINITE);
  global_loop_->Shutdown();
}

zx_status_t FsManager::Initialize(
    fidl::ServerEnd<fuchsia_io::Directory> dir_request,
    fidl::ServerEnd<fuchsia_process_lifecycle::Lifecycle> lifecycle_request,
    const fshost_config::Config& config, BlockWatcher& watcher) {
  global_loop_->StartThread("root-dispatcher");

  auto outgoing_dir = fbl::MakeRefCounted<fs::PseudoDir>();

  // Add services to the vfs
  svc_dir_ = fbl::MakeRefCounted<fs::PseudoDir>();
  svc_dir_->AddEntry(fidl::DiscoverableProtocolName<fuchsia_fshost::Admin>,
                     AdminServer::Create(this, global_loop_->dispatcher()));
  svc_dir_->AddEntry(fidl::DiscoverableProtocolName<fuchsia_fshost::BlockWatcher>,
                     BlockWatcherServer::Create(global_loop_->dispatcher(), watcher));
  outgoing_dir->AddEntry("svc", svc_dir_);

  fs_dir_ = fbl::MakeRefCounted<fs::PseudoDir>();

  // Construct the list of mount points we will be serving. Durable and Factory are somewhat special
  // cases - they rarely exist as partitions on the device, but they are always exported as
  // directory capabilities. If we aren't configured to find these partitions, don't queue requests
  // for them, and instead point them at an empty, read-only folder in the fs dir, so the directory
  // capability can be successfully routed.
  std::vector<MountPoint> mount_points;
  mount_points.push_back(MountPoint::kData);
  if (config.durable()) {
    mount_points.push_back(MountPoint::kDurable);
  } else {
    fs_dir_->AddEntry(MountPointPath(MountPoint::kDurable), fbl::MakeRefCounted<fs::PseudoDir>());
  }
  if (config.factory()) {
    mount_points.push_back(MountPoint::kFactory);
  } else {
    fs_dir_->AddEntry(MountPointPath(MountPoint::kFactory), fbl::MakeRefCounted<fs::PseudoDir>());
  }

  for (const auto& point : mount_points) {
    if (config.data_filesystem_format() == "fxfs") {
      // Fxfs launches as a component so doesn't have a mount node.
      continue;
    }

    zx::status endpoints_or = fidl::CreateEndpoints<fuchsia_io::Directory>();
    if (endpoints_or.is_error()) {
      return endpoints_or.status_value();
    }

    // FsRootHandle issues an Open call on the export root. These open calls are asynchronous -
    // they are queued into the channel pair and serviced when the filesystem is started.
    // Similarly, calls on the pair created by FsRootHandle, of which root_or is the client end,
    // are also queued.
    zx::status root_or = fs_management::FsRootHandle(endpoints_or->client);
    if (root_or.is_error()) {
      return root_or.status_value();
    }

    zx_status_t status = fs_dir_->AddEntry(MountPointPath(point),
                                           fbl::MakeRefCounted<fs::RemoteDir>(std::move(*root_or)));
    if (status != ZX_OK) {
      FX_PLOGS(ERROR, status) << "failed to add " << MountPointPath(point) << " to /fs directory";
    }

    auto [it, inserted] =
        mount_nodes_.try_emplace(point, FsManager::MountNode{
                                            .export_root = std::move(endpoints_or->client),
                                            .server_end = std::move(endpoints_or->server),
                                        });
    if (!inserted) {
      FX_LOGS(ERROR) << "Channel pair for mount point " << MountPointPath(point)
                     << " already exists";
    }
  }
  outgoing_dir->AddEntry("fs", fs_dir_);

  diagnostics_dir_ = inspect_manager_.Initialize(global_loop_->dispatcher());
  outgoing_dir->AddEntry("diagnostics", diagnostics_dir_);

  fbl::RefPtr<memfs::VnodeDir> tmp_vnode;
  zx_status_t status = memfs::Memfs::Create(global_loop_->dispatcher(), "<tmp>", &tmp_, &tmp_vnode);
  if (status != ZX_OK) {
    return status;
  }
  outgoing_dir->AddEntry("tmp", tmp_vnode);

  mnt_dir_ = fbl::MakeRefCounted<fs::PseudoDir>();
  outgoing_dir->AddEntry("mnt", mnt_dir_);

  if (dir_request.is_valid()) {
    // Run the outgoing directory.
    vfs_.ServeDirectory(outgoing_dir, std::move(dir_request));
  }
  if (lifecycle_request.is_valid()) {
    zx_status_t status =
        LifecycleServer::Create(global_loop_->dispatcher(), this, std::move(lifecycle_request));
    if (status != ZX_OK) {
      return status;
    }
  }
  return ZX_OK;
}

zx::status<fidl::ClientEnd<fuchsia_io::Directory>> FsManager::GetFsDir() {
  zx::status endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
  if (endpoints.is_error()) {
    return endpoints.take_error();
  }
  zx_status_t status = vfs_.ServeDirectory(fs_dir_, std::move(endpoints->server));
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(endpoints->client));
}

std::optional<FsManager::MountPointEndpoints> FsManager::TakeMountPointServerEnd(MountPoint point) {
  // Hold the shutdown lock for the entire duration of the install to avoid racing with shutdown on
  // adding/removing the remote mount.
  std::lock_guard guard(shutdown_lock_);
  if (shutdown_called_) {
    FX_LOGS(INFO) << "Not installing " << MountPointPath(point) << " after shutdown";
    return std::nullopt;
  }

  auto node = mount_nodes_.find(point);
  if (node == mount_nodes_.end()) {
    // The map should have been fully initialized.
    return std::nullopt;
  }
  if (!node->second.server_end.has_value()) {
    // The server end for this mount point was already taken, or the map was not fully initialized.
    return std::nullopt;
  }
  fidl::ServerEnd<fuchsia_io::Directory> server_end =
      std::exchange(node->second.server_end, std::nullopt).value();

  return FsManager::MountPointEndpoints{
      .export_root = node->second.export_root,
      .server_end = std::move(server_end),
  };
}

void FsManager::RegisterDevicePath(MountPoint point, std::string_view device_path) {
  // Retrieving the device path and setting it for a particular filesystem is best-effort, so any
  // failures are logged but otherwise ignored.
  if (device_path.empty())
    return;

  std::lock_guard guard(shutdown_lock_);
  if (shutdown_called_) {
    FX_LOGS(INFO) << "Not registering device path for " << MountPointPath(point)
                  << " after shutdown";
    return;
  }

  zx::status root_or = GetRoot(point);
  if (root_or.is_error()) {
    FX_PLOGS(WARNING, root_or.status_value()) << "Failed to get root handle for mount point";
    return;
  }
  if (auto result = fidl::WireCall(*root_or)->QueryFilesystem(); !result.ok()) {
    FX_PLOGS(WARNING, result.status()) << "QueryFilesystem call failed (fidl error)";
    return;
  } else if (result->s != ZX_OK) {
    FX_PLOGS(WARNING, result->s) << "QueryFilesystem call failed";
    return;
  } else {
    std::lock_guard guard(device_paths_lock_);
    if (!device_paths_.try_emplace(result->info->fs_id, device_path).second) {
      FX_LOGS(WARNING) << "Device path entry for fs id " << result->info->fs_id
                       << " already exists; not inserting " << device_path;
    }
  }
}

void FsManager::Shutdown(fit::function<void(zx_status_t)> callback) {
  std::lock_guard guard(shutdown_lock_);
  if (shutdown_called_) {
    FX_LOGS(ERROR) << "shutdown called more than once";
    callback(ZX_ERR_INTERNAL);
    return;
  }
  shutdown_called_ = true;

  FX_LOGS(INFO) << "filesystem shutdown initiated";
  // Shutting down fshost involves sending asynchronous shutdown signals to several different
  // systems in order with continuation passing.
  // 0. Before fshost is told to shut down, almost everything that is running out of the
  //    filesystems is shut down by component manager. Also before this, blobfs is told to shut
  //    down by component manager. Blobfs, as part of it's shutdown, notifies driver manager that
  //    drivers running out of /system should be shut down.
  // 1. Shut down any filesystems which were started, synchronously calling shutdown on each one in
  //    no particular order.
  // 2. Shut down the memfs which hosts /tmp.
  // 3. Shut down the vfs. This hosts the fshost outgoing directory.
  // 4. Call the shutdown callback provided when the shutdown function was called.
  // 5. Signal the shutdown completion that shutdown is complete. After this point, the FsManager
  //    class can be destroyed, and fshost can exit.
  // If at any point we hit an error, we log loudly, but continue with the shutdown procedure. At
  // the end, we send the callback whatever the first error value we encountered was.
  std::vector<std::pair<MountPoint, fidl::ClientEnd<fuchsia_io::Directory>>>
      filesystems_to_shut_down;
  for (auto& [point, node] : mount_nodes_) {
    if (!node.server_end.has_value()) {
      filesystems_to_shut_down.emplace_back(point, std::move(node.export_root));
    }
  }

  // fs_management::Shutdown is synchronous, so we spawn a thread to shut down
  // the mounted filesystems.
  std::thread shutdown_thread([this, callback = std::move(callback),
                               filesystems_to_shutdown =
                                   std::move(filesystems_to_shut_down)]() mutable {
    // Ensure that we are ready for shutdown.
    sync_completion_wait(&ready_for_shutdown_, ZX_TIME_INFINITE);
    auto merge_status = [first_status = ZX_OK](zx_status_t status) mutable {
      if (first_status == ZX_OK)
        first_status = status;
      return first_status;
    };

    for (auto& [point, fs] : filesystems_to_shutdown) {
      FX_LOGS(INFO) << "Shutting down " << MountPointPath(point);
      if (auto result = fs_management::Shutdown({fs.borrow()}); result.is_error()) {
        FX_LOGS(WARNING) << "Failed to shut down " << MountPointPath(point) << ": "
                         << result.status_string();
        merge_status(result.error_value());
      }
    }

    // Continue on the async loop...
    zx_status_t status = async::PostTask(
        global_loop_->dispatcher(),
        [this, callback = std::move(callback), merge_status = std::move(merge_status)]() mutable {
          tmp_->Shutdown([this, callback = std::move(callback),
                          merge_status = std::move(merge_status)](zx_status_t status) mutable {
            if (status != ZX_OK) {
              FX_LOGS(ERROR) << "tmp shutdown failed: " << zx_status_get_string(status);
              merge_status(status);
            }
            vfs_.Shutdown([this, callback = std::move(callback),
                           merge_status = std::move(merge_status)](zx_status_t status) mutable {
              if (status != ZX_OK) {
                FX_LOGS(ERROR) << "vfs shutdown failed: " << zx_status_get_string(status);
                merge_status(status);
              }
              callback(merge_status(ZX_OK));
              sync_completion_signal(&shutdown_);
              // after this signal, FsManager can be destroyed.
            });
          });
        });
    if (status != ZX_OK) {
      FX_LOGS(ERROR) << "Unable to finish shut down: " << zx_status_get_string(status);
      // We can't call the callback here because we moved it, but we don't expect posting the task
      // to fail, so let's not worry about it.
    }
  });

  shutdown_thread.detach();
}

bool FsManager::IsShutdown() { return sync_completion_signaled(&shutdown_); }

void FsManager::WaitForShutdown() { sync_completion_wait(&shutdown_, ZX_TIME_INFINITE); }

void FsManager::ReadyForShutdown() { sync_completion_signal(&ready_for_shutdown_); }

const char* FsManager::MountPointPath(FsManager::MountPoint point) {
  switch (point) {
    case MountPoint::kData:
      return "data";
    case MountPoint::kFactory:
      return "factory";
    case MountPoint::kDurable:
      return "durable";
  }
}

zx_status_t FsManager::ForwardFsDiagnosticsDirectory(MountPoint point,
                                                     std::string_view diagnostics_dir_name) {
  // The diagnostics directory may not be initialized in tests.
  if (diagnostics_dir_ == nullptr) {
    return ZX_ERR_INTERNAL;
  }
  if (!mount_nodes_[point].export_root) {
    FX_LOGS(ERROR) << "Can't forward diagnostics dir for " << MountPointPath(point)
                   << ", export root directory was not set";
    return ZX_ERR_BAD_STATE;
  }

  auto inspect_node = fbl::MakeRefCounted<fs::Service>([this, point](zx::channel request) {
    std::string name = std::string("diagnostics/") + fuchsia::inspect::Tree::Name_;
    return fdio_service_connect_at(mount_nodes_[point].export_root.channel().get(), name.c_str(),
                                   request.release());
  });
  auto fs_diagnostics_dir = fbl::MakeRefCounted<fs::PseudoDir>();
  zx_status_t status = fs_diagnostics_dir->AddEntry(fuchsia::inspect::Tree::Name_, inspect_node);
  if (status != ZX_OK) {
    return status;
  }
  return diagnostics_dir_->AddEntry(diagnostics_dir_name, fs_diagnostics_dir);
}

zx_status_t FsManager::ForwardFsService(MountPoint point, const char* service_name) {
  // The outgoing service directory may not be initialized in tests.
  if (svc_dir_ == nullptr) {
    return ZX_ERR_INTERNAL;
  }
  if (!mount_nodes_[point].export_root) {
    FX_LOGS(ERROR) << "Can't forward service for " << MountPointPath(point)
                   << ", export root directory was not set";
    return ZX_ERR_BAD_STATE;
  }

  auto service_node =
      fbl::MakeRefCounted<fs::Service>([this, point, service_name](zx::channel request) {
        std::string name = std::string("svc/") + service_name;
        return fdio_service_connect_at(mount_nodes_[point].export_root.channel().get(),
                                       name.c_str(), request.release());
      });
  return svc_dir_->AddEntry(service_name, std::move(service_node));
}

void FsManager::FileReport(fs_management::DiskFormat format, ReportReason reason) const {
  fbl::String report_reason = ReportReasonString(format, reason);
  FX_LOGS(INFO) << "Filing crash report, reason: " << report_reason.c_str();
  if (!file_crash_report_) {
    FX_LOGS(INFO) << "Report filing disabled, ignoring crash report.";
    return;
  }
  // This thread accesses no state in the SyntheticCrashReporter, so is thread-safe even if the
  // reporter is destroyed.
  std::thread t([format, report_reason = std::move(report_reason)]() {
    auto client_end = service::Connect<fuchsia_feedback::CrashReporter>();
    if (client_end.is_error()) {
      FX_LOGS(WARNING) << "Unable to connect to crash reporting service: "
                       << client_end.status_string();
      return;
    }
    auto client = fidl::BindSyncClient(std::move(*client_end));

    fidl::Arena allocator;
    fuchsia_feedback::wire::CrashReport report(allocator);
    report.set_program_name(allocator, allocator, fs_management::DiskFormatString(format));
    report.set_crash_signature(allocator, allocator, report_reason);
    report.set_is_fatal(false);

    auto res = client->File(report);
    if (!res.ok()) {
      FX_LOGS(WARNING) << "Unable to send crash report (fidl error): " << res.status_string();
    } else if (res.Unwrap_NEW()->is_error()) {
      FX_LOGS(WARNING) << "Failed to file crash report: "
                       << zx_status_get_string(res.Unwrap_NEW()->error_value());
    } else {
      FX_LOGS(INFO) << "Crash report successfully filed";
    }
  });
  t.detach();
}

zx_status_t FsManager::AttachMount(std::string_view device_path,
                                   fidl::ClientEnd<fuchsia_io::Directory> export_root,
                                   std::string_view name) {
  auto root_or = fs_management::FsRootHandle(export_root);
  if (root_or.is_error()) {
    FX_PLOGS(WARNING, root_or.status_value()) << "Failed to get root";
    zx::status result = fs_management::Shutdown(export_root);
    if (result.is_error()) {
      FX_PLOGS(WARNING, result.status_value()) << "Failed to shutdown after failure to get root";
    }
    return root_or.error_value();
  }

  uint64_t fs_id = GetFsId(*root_or).value_or(0);
  mnt_dir_->AddEntry(name, fbl::MakeRefCounted<fs::RemoteDir>(std::move(*root_or)));

  std::lock_guard guard(device_paths_lock_);
  mounted_filesystems_.emplace(
      std::make_unique<MountedFilesystem>(name, std::move(export_root), fs_id));
  if (!device_path.empty())
    device_paths_.emplace(fs_id, device_path);
  return ZX_OK;
}

zx_status_t FsManager::DetachMount(std::string_view name) {
  std::lock_guard guard(device_paths_lock_);
  if (auto iter = mounted_filesystems_.find(name); iter == mounted_filesystems_.end()) {
    return ZX_ERR_NOT_FOUND;
  } else {
    device_paths_.erase((*iter)->fs_id());
    mounted_filesystems_.erase(iter);
  }
  return mnt_dir_->RemoveEntry(name);
}

zx::status<std::string> FsManager::GetDevicePath(uint64_t fs_id) {
  std::lock_guard guard(device_paths_lock_);
  auto iter = device_paths_.find(fs_id);
  if (iter == device_paths_.end())
    return zx::error(ZX_ERR_NOT_FOUND);
  else
    return zx::ok(iter->second);
}

zx::status<fidl::ClientEnd<fuchsia_io::Directory>> FsManager::GetRoot(MountPoint point) const {
  auto node = mount_nodes_.find(point);
  if (node == mount_nodes_.cend()) {
    if (point == MountPoint::kData) {
      // The data mount has been mounted via a component in which case we can get to the service
      // root through our local namespace.
      return service::Connect<fuchsia_io::Directory>("/data_root");
    }
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  return fs_management::FsRootHandle(node->second.export_root.borrow());
}

}  // namespace fshost
