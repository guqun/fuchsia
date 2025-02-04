// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "fdio.h"

#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/fdio.h>
#include <lib/fdio/io.h>
#include <lib/fdio/spawn.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/channel.h>
#include <lib/zx/debuglog.h>
#include <lib/zx/job.h>
#include <lib/zx/process.h>
#include <lib/zx/resource.h>
#include <lib/zx/vmo.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <zircon/processargs.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/log.h>

#include <iterator>
#include <utility>

#include <fbl/algorithm.h>
#include <fbl/array.h>
#include <fbl/vector.h>

namespace fshost {

#define CHILD_JOB_RIGHTS (ZX_RIGHTS_BASIC | ZX_RIGHT_MANAGE_JOB | ZX_RIGHT_MANAGE_PROCESS)

zx_status_t Launch(const zx::job& job, const char* name, const char* const* argv,
                   const char** initial_envp, int stdiofd, const zx::resource& root_resource,
                   const zx_handle_t* handles, const uint32_t* types, size_t hcount,
                   zx::process* out_proc) {
  zx::job job_copy;
  zx_status_t status = job.duplicate(CHILD_JOB_RIGHTS, &job_copy);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "launch failed " << zx_status_get_string(status);
    return status;
  }

  zx::debuglog debuglog;
  if (stdiofd < 0) {
    if ((status = zx::debuglog::create(root_resource, 0, &debuglog) != ZX_OK)) {
      return status;
    }
  }

  uint32_t spawn_flags = FDIO_SPAWN_CLONE_JOB | FDIO_SPAWN_CLONE_UTC_CLOCK;

  // Set up the environ for the new process
  fbl::Vector<const char*> env;
  if (getenv(LDSO_TRACE_CMDLINE)) {
    env.push_back(LDSO_TRACE_ENV);
  }
  while (initial_envp && initial_envp[0]) {
    env.push_back(*initial_envp++);
  }
  env.push_back(nullptr);

  fbl::Vector<fdio_spawn_action_t> actions;
  actions.reserve(4 + hcount);

  actions.push_back((fdio_spawn_action_t){
      .action = FDIO_SPAWN_ACTION_SET_NAME,
      .name = {.data = name},
  });

  spawn_flags |= FDIO_SPAWN_DEFAULT_LDSVC;

  actions.push_back((fdio_spawn_action_t){
      .action = FDIO_SPAWN_ACTION_CLONE_DIR,
      .dir = {.prefix = "/svc"},
  });

  if (debuglog.is_valid()) {
    actions.push_back((fdio_spawn_action_t){
        .action = FDIO_SPAWN_ACTION_ADD_HANDLE,
        .h = {.id = PA_HND(PA_FD, FDIO_FLAG_USE_FOR_STDIO | 0), .handle = debuglog.release()},
    });
  } else {
    actions.push_back((fdio_spawn_action_t){
        .action = FDIO_SPAWN_ACTION_TRANSFER_FD,
        .fd = {.local_fd = stdiofd, .target_fd = FDIO_FLAG_USE_FOR_STDIO | 0},
    });
  }

  for (size_t i = 0; i < hcount; ++i) {
    actions.push_back((fdio_spawn_action_t){
        .action = FDIO_SPAWN_ACTION_ADD_HANDLE,
        .h = {.id = types[i], .handle = handles[i]},
    });
  }

  zx::process proc;
  char err_msg[FDIO_SPAWN_ERR_MSG_MAX_LENGTH];
  status = fdio_spawn_etc(job_copy.get(), spawn_flags, argv[0], argv, env.data(), actions.size(),
                          actions.data(), proc.reset_and_get_address(), err_msg);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "spawn " << argv[0] << " (" << name << ") failed: " << err_msg << ": "
                   << status;
    return status;
  }
  FX_LOGS(INFO) << "launch " << argv[0] << " (" << name << ") OK";
  if (out_proc != nullptr) {
    *out_proc = std::move(proc);
  }
  return ZX_OK;
}

ArgumentVector ArgumentVector::FromCmdline(const char* cmdline) {
  ArgumentVector argv;
  const size_t cmdline_len = strlen(cmdline) + 1;
  argv.raw_bytes_.reset(new char[cmdline_len]);
  memcpy(argv.raw_bytes_.get(), cmdline, cmdline_len);

  // Get the full commandline by splitting on '+'.
  size_t argc = 0;
  char* token;
  char* rest = argv.raw_bytes_.get();
  while (argc < std::size(argv.argv_) && (token = strtok_r(rest, "+", &rest))) {
    argv.argv_[argc++] = token;
  }
  argv.argv_[argc] = nullptr;
  return argv;
}

std::ostream& operator<<(std::ostream& stream, const ArgumentVector& arguments) {
  const char* const* argv = arguments.argv();
  const char* prefix = "'";
  for (const char* arg = *argv; arg != nullptr; ++argv, arg = *argv) {
    stream << prefix << *argv << "'";
    prefix = " '";
  }
  return stream;
}

}  // namespace fshost
