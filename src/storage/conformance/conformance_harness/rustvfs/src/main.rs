// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! fuchsia io conformance testing harness for the rust psuedo-fs-mt library

use {
    anyhow::{anyhow, Context as _, Error},
    fidl_fuchsia_io as fio,
    fidl_fuchsia_io_test::{
        self as io_test, Io1Config, Io1HarnessRequest, Io1HarnessRequestStream,
    },
    fidl_fuchsia_mem, fuchsia_async as fasync,
    fuchsia_component::server::ServiceFs,
    fuchsia_syslog as syslog,
    fuchsia_zircon::{self as zx, HandleBased},
    futures::prelude::*,
    log::error,
    std::sync::Arc,
    vfs::{
        directory::{
            entry::DirectoryEntry,
            helper::DirectlyMutable,
            mutable::{connection::io1::MutableConnection, simple},
            simple::Simple,
        },
        execution_scope::ExecutionScope,
        file::vmo::asynchronous as vmo,
        path::Path,
        registry::token_registry,
        remote::remote_dir,
    },
};

struct Harness(Io1HarnessRequestStream);

const HARNESS_EXEC_PATH: &'static str = "/pkg/bin/io_conformance_harness_rustvfs";

/// Creates and returns a Rust VFS VmoFile-backed file using the contents of the given buffer.
///
/// The VMO backing the buffer is duplicated so that tests using VMO_FLAG_EXACT can ensure the
/// same VMO is returned by subsequent GetBuffer calls.
fn new_vmo_file(buffer: &fidl_fuchsia_mem::Range) -> Result<Arc<dyn DirectoryEntry>, Error> {
    let size = buffer.size;
    // Duplicate the VMO so we can move it into the init closure.
    let vmo: zx::Vmo = buffer.vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?.into();
    let init_vmo = move || {
        // Need to clone VMO again as this might be invoked multiple times.
        let vmo_clone: zx::Vmo =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Failed to duplicate VMO!").into();
        async move { Ok(vmo::NewVmo { vmo: vmo_clone, size, capacity: size }) }
    };
    Ok(vmo::read_write(init_vmo))
}

/// Creates and returns a Rust VFS VmoFile-backed executable file using the contents of the
/// conformance test harness binary itself.
fn new_executable_file() -> Result<Arc<dyn DirectoryEntry>, Error> {
    let init_vmo = || async {
        let file = fdio::open_fd(
            HARNESS_EXEC_PATH,
            fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_EXECUTABLE,
        )?;
        let vmo = fdio::get_vmo_exec_from_file(&file)?;
        let size = vmo.get_size()?;
        Ok(vmo::NewVmo { vmo, size, capacity: size })
    };
    Ok(vmo::read_exec(init_vmo))
}

fn add_entry(
    entry: io_test::DirectoryEntry,
    dest: &Arc<Simple<MutableConnection>>,
) -> Result<(), Error> {
    match entry {
        io_test::DirectoryEntry::Directory(dir) => {
            let name = dir.name.expect("Directory must have name");
            let new_dir = simple();
            if let Some(entries) = dir.entries {
                for entry in entries {
                    let entry = *entry.expect("Directory entries must not be null");
                    add_entry(entry, &new_dir)?;
                }
            }
            dest.add_entry(name, new_dir)?;
        }
        io_test::DirectoryEntry::RemoteDirectory(dir) => {
            let name = dir.name.expect("RemoteDirectory must have name");
            let dir_proxy = dir
                .remote_client
                .expect("RemoteDirectory must have a remote client")
                .into_proxy()?;
            dest.add_entry(name, remote_dir(dir_proxy))?;
        }
        io_test::DirectoryEntry::File(file) => {
            let name = file.name.as_ref().expect("File must have name");
            let contents = file.contents.as_ref().expect("File must have contents").clone();
            let new_file =
                vmo::read_write(vmo::simple_init_vmo_resizable_with_capacity(&contents, 100));
            dest.add_entry(name, new_file)?;
        }
        io_test::DirectoryEntry::VmoFile(vmo_file) => {
            let name = vmo_file.name.expect("VMO file must have a name");
            let buffer = vmo_file.buffer.expect("VMO file must have a buffer");
            dest.add_entry(name, new_vmo_file(&buffer)?)?;
        }
        io_test::DirectoryEntry::ExecutableFile(executable_file) => {
            let name = executable_file.name.expect("Exec file must have a name");
            dest.add_entry(name, new_executable_file()?)?;
        }
    }
    Ok(())
}

async fn run(mut stream: Io1HarnessRequestStream) -> Result<(), Error> {
    while let Some(request) = stream.try_next().await.context("error running harness server")? {
        let (dir, flags, directory_request) = match request {
            Io1HarnessRequest::GetConfig { responder } => {
                let config = Io1Config {
                    // Supported options:
                    mutable_file: Some(true),
                    supports_create: Some(true),
                    supports_executable_file: Some(true),
                    supports_vmo_file: Some(true),
                    supports_remote_dir: Some(true),
                    supports_get_buffer: Some(true),
                    supports_rename: Some(true),
                    supports_get_token: Some(true),
                    conformant_path_handling: Some(true),
                    supports_unlink: Some(true),

                    // Unsupported options:
                    supports_link: Some(false), // Link is not supported using a pseudo filesystem.
                    // TODO(fxbug.dev/72801): SetAttr should work, investigate why the test fails.
                    supports_set_attr: Some(false),

                    ..Io1Config::EMPTY
                };
                responder.send(config)?;
                continue;
            }
            Io1HarnessRequest::GetDirectory {
                root,
                flags,
                directory_request,
                control_handle: _,
            } => {
                let dir = simple();
                if let Some(entries) = root.entries {
                    for entry in entries {
                        let entry = entry.expect("Directory entries must not be null");
                        add_entry(*entry, &dir)?;
                    }
                }
                (dir, flags, directory_request)
            }
        };

        let token_registry = token_registry::Simple::new();
        let scope = ExecutionScope::build()
            .token_registry(token_registry)
            .entry_constructor(simple::tree_constructor(|_parent, _filename| {
                let entry = vmo::read_write(vmo::simple_init_vmo_resizable_with_capacity(&[], 100));
                Ok(entry)
            }))
            .new();

        dir.open(scope, flags, 0, Path::dot(), directory_request.into_channel().into());
    }

    Ok(())
}

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    syslog::init().unwrap();

    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_fidl_service(Harness);
    fs.take_and_serve_directory_handle()?;

    let fut = fs.for_each_concurrent(10_000, |Harness(stream)| {
        run(stream).unwrap_or_else(|e| error!("Error processing request: {:?}", anyhow!(e)))
    });

    fut.await;
    Ok(())
}
