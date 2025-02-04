// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of an individual connection to a file.

use crate::{
    common::{
        inherit_rights_for_clone, rights_to_posix_mode_bits, send_on_open_with_error,
        GET_FLAGS_VISIBLE,
    },
    execution_scope::ExecutionScope,
    file::common::{get_buffer_validate_flags, new_connection_validate_flags, vmo_flags_to_rights},
    file::vmo::{
        asynchronous::{NewVmo, VmoFileState},
        connection::VmoFileInterface,
    },
};

use {
    anyhow::Error,
    fidl::endpoints::ServerEnd,
    fidl::prelude::*,
    fidl_fuchsia_io as fio,
    fidl_fuchsia_mem::Buffer,
    fuchsia_zircon::{
        self as zx,
        sys::{ZX_ERR_NOT_SUPPORTED, ZX_OK},
        AsHandleRef, HandleBased,
    },
    futures::{lock::MutexGuard, stream::StreamExt},
    static_assertions::assert_eq_size,
    std::{convert::TryInto, sync::Arc},
};

macro_rules! update_initialized_state {
    (match $status:expr;
     error: $method_name:expr => $uninitialized_result:expr ;
     { $( $vars:tt ),* $(,)* } => $body:stmt $(;)*) => {
        match $status {
            VmoFileState::Uninitialized => {
                let name = $method_name;
                debug_assert!(false, "`{}` called for a file with no connections", name);
                $uninitialized_result
            }
            VmoFileState::Initialized { $( $vars ),* } => loop { break { $body } },
        }
    }
}

/// Represents a FIDL connection to a file.
pub struct VmoFileConnection {
    /// Execution scope this connection and any async operations and connections it creates will
    /// use.
    scope: ExecutionScope,

    /// File this connection is associated with.
    file: Arc<dyn VmoFileInterface>,

    /// Wraps a FIDL connection, providing messages coming from the client.
    requests: fio::FileRequestStream,

    /// Either the "flags" value passed into [`DirectoryEntry::open()`], or the "flags" value
    /// received with [`FileRequest::Clone()`].
    flags: fio::OpenFlags,

    /// Seek position. Next byte to be read or written within the buffer. This might be beyond the
    /// current size of buffer, matching POSIX:
    ///
    ///     http://pubs.opengroup.org/onlinepubs/9699919799/functions/lseek.html
    ///
    /// It will cause the buffer to be extended with zeroes (if necessary) when write() is called.
    // While the content in the buffer vector uses usize for the size, it is easier to use u64 to
    // match the FIDL bindings API. Pseudo files are not expected to cross the 2^64 bytes size
    // limit. And all the code is much simpler when we just assume that usize is the same as u64.
    // Should we need to port to a 128 bit platform, there are static assertions in the code that
    // would fail.
    seek: u64,
}

/// Return type for [`handle_request()`] functions.
enum ConnectionState {
    /// Connection is still alive.
    Alive,
    /// Connection have received Node::Close message and the [`handle_close`] method has been
    /// already called for this connection.
    Closed,
    /// Connection has been dropped by the peer or an error has occurred.  [`handle_close`] still
    /// need to be called (though it would not be able to report the status to the peer).
    Dropped,
}

impl VmoFileConnection {
    /// Initialized a file connection, which will be running in the context of the specified
    /// execution `scope`.  This function will also check the flags and will send the `OnOpen`
    /// event if necessary.
    ///
    /// Per connection buffer is initialized using the `init_vmo` closure, as part of the
    /// connection initialization.
    pub(in crate::file::vmo) fn create_connection(
        scope: ExecutionScope,
        file: Arc<dyn VmoFileInterface>,
        flags: fio::OpenFlags,
        server_end: ServerEnd<fio::NodeMarker>,
    ) {
        let task = Self::create_connection_task(scope.clone(), file, flags, server_end);
        // If we failed to send the task to the executor, it is probably shut down or is in the
        // process of shutting down (this is the only error state currently).  So there is nothing
        // for us to do, but to ignore the open.  `server_end` will be closed when the object will
        // be dropped - there seems to be no error to report there.
        let _ = scope.spawn(Box::pin(task));
    }

    async fn create_connection_task(
        scope: ExecutionScope,
        file: Arc<dyn VmoFileInterface>,
        flags: fio::OpenFlags,
        server_end: ServerEnd<fio::NodeMarker>,
    ) {
        let flags = match new_connection_validate_flags(
            flags,
            file.is_readable(),
            file.is_writable(),
            file.is_executable(),
            /*append_allowed=*/ false,
        ) {
            Ok(updated) => updated,
            Err(status) => {
                send_on_open_with_error(flags, server_end, status);
                return;
            }
        };

        let server_end = {
            let (mut state, server_end) =
                match Self::ensure_vmo(file.clone(), file.state().await, server_end).await {
                    Ok(res) => res,
                    Err((status, server_end)) => {
                        send_on_open_with_error(flags, server_end, status);
                        return;
                    }
                };

            if flags.intersects(fio::OpenFlags::TRUNCATE) {
                let mut seek = 0;
                if let Err(status) = Self::truncate_vmo(&mut *state, 0, &mut seek) {
                    send_on_open_with_error(flags, server_end, status);
                    return;
                }
                debug_assert!(seek == 0);
            }

            match &mut *state {
                VmoFileState::Uninitialized => {
                    debug_assert!(false, "`ensure_vmo` did not initialize the state.");
                    send_on_open_with_error(flags, server_end, zx::Status::INTERNAL);
                    return;
                }
                VmoFileState::Initialized { connection_count, .. } => {
                    *connection_count += 1;
                }
            }

            server_end
        };

        let (requests, control_handle) = match server_end.into_stream_and_control_handle() {
            Ok((requests, control_handle)) => (requests.cast_stream(), control_handle),
            Err(_) => {
                // As we report all errors on `server_end`, if we failed to send an error over this
                // connection, there is nowhere to send the error to.
                return;
            }
        };

        let mut connection =
            VmoFileConnection { scope: scope.clone(), file, requests, flags, seek: 0 };

        if flags.intersects(fio::OpenFlags::DESCRIBE) {
            match connection.get_node_info().await {
                Ok(mut info) => {
                    let send_result =
                        control_handle.send_on_open_(zx::Status::OK.into_raw(), Some(&mut info));
                    if send_result.is_err() {
                        return;
                    }
                }
                Err(status) => {
                    debug_assert!(status != zx::Status::OK);
                    control_handle.shutdown_with_epitaph(status);
                    return;
                }
            }
        }

        connection.handle_requests().await;
    }

    async fn ensure_vmo<'state_guard>(
        file: Arc<dyn VmoFileInterface>,
        mut state: MutexGuard<'state_guard, VmoFileState>,
        server_end: ServerEnd<fio::NodeMarker>,
    ) -> Result<
        (MutexGuard<'state_guard, VmoFileState>, ServerEnd<fio::NodeMarker>),
        (zx::Status, ServerEnd<fio::NodeMarker>),
    > {
        if let VmoFileState::Initialized { .. } = *state {
            return Ok((state, server_end));
        }

        let NewVmo { vmo, mut size, capacity } = match file.init_vmo().await {
            Ok(res) => res,
            Err(status) => return Err((status, server_end)),
        };
        let mut vmo_size = match vmo.get_size() {
            Ok(size) => size,
            Err(status) => return Err((status, server_end)),
        };

        if cfg!(debug_assertions) {
            // Debug build will just enforce the constraints.
            assert!(
                vmo_size >= size,
                "`init_vmo` returned a VMO that is smaller than the declared size.\n\
                 VMO size: {}\n\
                 Declared size: {}",
                vmo_size,
                size
            );
        } else if vmo_size < size {
            // Release build will try to recover.
            match vmo.set_size(size) {
                Ok(()) => {
                    // Actual VMO size might be different from the requested one due to rounding,
                    // so we have to ask for it.
                    vmo_size = match vmo.get_size() {
                        Ok(size) => size,
                        Err(status) => return Err((status, server_end)),
                    };
                }
                Err(zx::Status::UNAVAILABLE) => {
                    // VMO is not resizable.  Try to use what we got.
                    size = vmo_size;
                }
                Err(status) => return Err((status, server_end)),
            }
        }

        *state = VmoFileState::Initialized {
            vmo,
            vmo_size,
            size,
            capacity,
            // We are going to increment the connection count later, so it needs to
            // start at 0.
            connection_count: 0,
        };

        Ok((state, server_end))
    }

    fn truncate_vmo(
        state: &mut VmoFileState,
        new_size: u64,
        seek: &mut u64,
    ) -> Result<(), zx::Status> {
        update_initialized_state! {
            match state;
            error: "truncate_vmo" => Err(zx::Status::INTERNAL);
            { vmo, vmo_size, size, capacity, .. } => {
                let effective_capacity = core::cmp::max(*size, *capacity);

                if new_size > effective_capacity {
                    break Err(zx::Status::OUT_OF_RANGE);
                }

                assert_eq_size!(usize, u64);

                vmo.set_size(new_size)?;
                // Actual VMO size might be different from the requested one due to rounding,
                // so we have to ask for it.
                *vmo_size = match vmo.get_size() {
                    Ok(size) => size,
                    Err(status) => break Err(status),
                };

                    if let Err(status) = vmo.set_content_size(&new_size) {
                        break Err(status);
                    }

                *size = new_size;

                // We are not supposed to touch the seek position during truncation, but the
                // effective_capacity might be smaller now - in which case we do need to move the
                // seek position.
                let new_effective_capacity = core::cmp::max(new_size, *capacity);
                *seek = core::cmp::min(*seek, new_effective_capacity);

                Ok(())
            }
        }
    }

    async fn handle_requests(mut self) {
        while let Some(request_or_err) = self.requests.next().await {
            let state = match request_or_err {
                Err(_) => {
                    // FIDL level error, such as invalid message format and alike.  Close the
                    // connection on any unexpected error.
                    // TODO: Send an epitaph.
                    ConnectionState::Dropped
                }
                Ok(request) => {
                    self.handle_request(request)
                        .await
                        // Protocol level error.  Close the connection on any unexpected error.
                        // TODO: Send an epitaph.
                        .unwrap_or(ConnectionState::Dropped)
                }
            };

            match state {
                ConnectionState::Alive => (),
                ConnectionState::Closed => {
                    // We have already called `handle_close`, do not call it again.
                    return;
                }
                ConnectionState::Dropped => break,
            }
        }

        // If the connection has been closed by the peer or due some error we still need to call
        // the `updated` callback, unless the `Close` message have been used.
        // `ConnectionState::Closed` is handled above.
        let _: Result<(), zx::Status> = self.handle_close().await;
    }

    /// Returns `NodeInfo` for the VMO file.
    async fn get_node_info(&mut self) -> Result<fio::NodeInfo, zx::Status> {
        // The current fuchsia.io specification for Vmofile node types specify that the node is
        // immutable, thus if the file is writable, we report it as a regular file instead.
        // If this changes in the future, we need to handle size changes in the backing VMO.
        if self.flags.intersects(fio::OpenFlags::NODE_REFERENCE)
            || self.flags.intersects(fio::OpenFlags::RIGHT_WRITABLE)
        {
            Ok(fio::NodeInfo::File(fio::FileObject { event: None, stream: None }))
        } else {
            let vmofile = update_initialized_state! {
                match &*self.file.state().await;
                error: "get_node_info" => Err(zx::Status::INTERNAL);
                { vmo, size, .. } => {
                    // Since the VMO rights may exceed those of the connection, we need to ensure
                    // the duplicated handle's rights are not greater than those of the connection.
                    let mut new_rights = vmo.basic_info().unwrap().rights;
                    // We already checked above that the connection is not writable. We also remove
                    // SET_PROPERTY as this would also allow size changes.
                    new_rights.remove(zx::Rights::WRITE | zx::Rights::SET_PROPERTY);
                    if !self.flags.intersects(fio::OpenFlags::RIGHT_EXECUTABLE) {
                        new_rights.remove(zx::Rights::EXECUTE);
                    }
                    let vmo = vmo.duplicate_handle(new_rights).unwrap();

                    Ok(fio::Vmofile {vmo, offset: 0, length: *size})
                }
            }?;
            Ok(fio::NodeInfo::Vmofile(vmofile))
        }
    }

    /// Handle a [`FileRequest`]. This function is responsible for handing all the file operations
    /// that operate on the connection-specific buffer.
    async fn handle_request(&mut self, req: fio::FileRequest) -> Result<ConnectionState, Error> {
        match req {
            fio::FileRequest::Clone { flags, object, control_handle: _ } => {
                self.handle_clone(self.flags, flags, object);
            }
            fio::FileRequest::Reopen { options, object_request, control_handle: _ } => {
                let _ = object_request;
                todo!("https://fxbug.dev/77623: options={:?}", options);
            }
            fio::FileRequest::Close { responder } => {
                // We are going to close the connection anyways, so there is no way to handle this
                // error.
                let result = self.handle_close().await;
                responder.send(&mut result.map_err(zx::Status::into_raw))?;
                return Ok(ConnectionState::Closed);
            }
            fio::FileRequest::Describe { responder } => match self.get_node_info().await {
                Ok(mut info) => responder.send(&mut info)?,
                Err(status) => {
                    debug_assert!(status != zx::Status::OK);
                    responder.control_handle().shutdown_with_epitaph(status);
                }
            },
            fio::FileRequest::Describe2 { query, responder } => {
                let _ = responder;
                todo!("https://fxbug.dev/77623: query={:?}", query);
            }
            fio::FileRequest::Sync { responder } => {
                // VMOs are always in sync.
                responder.send(&mut Ok(()))?;
            }
            fio::FileRequest::GetAttr { responder } => {
                let (status, mut attrs) = self.handle_get_attr().await;
                responder.send(status.into_raw(), &mut attrs)?;
            }
            fio::FileRequest::SetAttr { flags: _, attributes: _, responder } => {
                // According to https://fuchsia.googlesource.com/fuchsia/+/HEAD/sdk/fidl/fuchsia.io/
                // the only flag that might be modified through this call is OPEN_FLAG_APPEND, and
                // it is not supported at the moment.
                responder.send(ZX_ERR_NOT_SUPPORTED)?;
            }
            fio::FileRequest::GetAttributes { query, responder } => {
                let _ = responder;
                todo!("https://fxbug.dev/77623: query={:?}", query);
            }
            fio::FileRequest::UpdateAttributes { attributes, responder } => {
                let _ = responder;
                todo!("https://fxbug.dev/77623: attributes={:?}", attributes);
            }
            fio::FileRequest::Read { count, responder } => {
                let result = self.handle_read(count).await;
                responder.send(&mut result.map_err(zx::Status::into_raw))?;
            }
            fio::FileRequest::ReadAt { count, offset, responder } => {
                let result = self.handle_read_at(offset, count).await;
                responder.send(&mut result.map_err(zx::Status::into_raw))?;
            }
            fio::FileRequest::WriteDeprecated { data, responder } => {
                let (status, count) = match self.handle_write(&data).await {
                    Ok(count) => (zx::Status::OK, count),
                    Err(status) => (status, 0),
                };
                responder.send(status.into_raw(), count)?;
            }
            fio::FileRequest::Write { data, responder } => {
                let result = self.handle_write(&data).await;
                responder.send(&mut result.map_err(zx::Status::into_raw))?;
            }
            fio::FileRequest::WriteAt { offset, data, responder } => {
                let result = self.handle_write_at(offset, &data).await;
                responder.send(&mut result.map_err(zx::Status::into_raw))?;
            }
            fio::FileRequest::Seek { origin, offset, responder } => {
                let result = self.handle_seek(offset, origin).await;
                responder.send(&mut result.map_err(zx::Status::into_raw))?;
            }
            fio::FileRequest::Resize { length, responder } => {
                let result = self.handle_truncate(length).await;
                responder.send(&mut result.map_err(zx::Status::into_raw))?;
            }
            fio::FileRequest::GetFlags { responder } => {
                responder.send(ZX_OK, self.flags & GET_FLAGS_VISIBLE)?;
            }
            fio::FileRequest::SetFlags { flags: _, responder } => {
                // TODO: Support OPEN_FLAG_APPEND?  It is the only flag that is allowed to be set
                // via this call according to fuchsia.io. It would be nice to have that explicitly
                // encoded in the API instead, I guess.
                responder.send(ZX_ERR_NOT_SUPPORTED)?;
            }
            fio::FileRequest::GetBackingMemory { flags, responder } => {
                let result = self.handle_get_buffer(flags).await.map(|Buffer { vmo, size: _ }| vmo);
                responder.send(&mut result.map_err(zx::Status::into_raw))?;
            }
            fio::FileRequest::AdvisoryLock { request: _, responder } => {
                responder.send(&mut Err(ZX_ERR_NOT_SUPPORTED))?;
            }
            fio::FileRequest::QueryFilesystem { responder } => {
                responder.send(ZX_ERR_NOT_SUPPORTED, None)?;
            }
        }
        Ok(ConnectionState::Alive)
    }

    fn handle_clone(
        &mut self,
        parent_flags: fio::OpenFlags,
        current_flags: fio::OpenFlags,
        server_end: ServerEnd<fio::NodeMarker>,
    ) {
        let flags = match inherit_rights_for_clone(parent_flags, current_flags) {
            Ok(updated) => updated,
            Err(status) => {
                send_on_open_with_error(current_flags, server_end, status);
                return;
            }
        };

        Self::create_connection(self.scope.clone(), self.file.clone(), flags, server_end);
    }

    async fn handle_close(&mut self) -> Result<(), zx::Status> {
        let state = &mut *self.file.state().await;
        match state {
            VmoFileState::Uninitialized => {
                debug_assert!(false, "`handle_close` called for a file with no connections");
                Err(zx::Status::INTERNAL)
            }
            VmoFileState::Initialized { connection_count, .. } => {
                *connection_count -= 1;

                Ok(())
            }
        }
    }

    async fn handle_get_attr(&mut self) -> (zx::Status, fio::NodeAttributes) {
        let result = update_initialized_state! {
            match *self.file.state().await;
            error: "handle_get_attr" => Err(zx::Status::INTERNAL);
            { size, capacity, .. } => Ok((size, capacity))
        };

        let (status, size, capacity) = match result {
            Ok((size, capacity)) => (zx::Status::OK, size, capacity),
            Err(status) => (status, 0, 0),
        };

        (
            status,
            fio::NodeAttributes {
                mode: fio::MODE_TYPE_FILE
                    | rights_to_posix_mode_bits(
                        self.file.is_readable(),
                        self.file.is_writable(),
                        self.file.is_executable(),
                    ),
                id: self.file.get_inode(),
                content_size: size,
                storage_size: capacity,
                link_count: 1,
                creation_time: 0,
                modification_time: 0,
            },
        )
    }

    async fn handle_read(&mut self, count: u64) -> Result<Vec<u8>, zx::Status> {
        let bytes = self.handle_read_at(self.seek, count).await?;
        let count = bytes.len().try_into().unwrap();
        self.seek = self.seek.checked_add(count).unwrap();
        Ok(bytes)
    }

    async fn handle_read_at(&mut self, offset: u64, count: u64) -> Result<Vec<u8>, zx::Status> {
        if !self.flags.intersects(fio::OpenFlags::RIGHT_READABLE) {
            return Err(zx::Status::BAD_HANDLE);
        }

        update_initialized_state! {
            match &*self.file.state().await;
            error: "handle_read_at" => return Err(zx::Status::INTERNAL);
            { vmo, size, .. } => {
                match size.checked_sub(offset) {
                    None => Ok(Vec::new()),
                    Some(rem) => {
                        let count = core::cmp::min(count, rem);

                        assert_eq_size!(usize, u64);
                        let count = count.try_into().unwrap();

                        let mut buffer = vec![0; count];
                        vmo.read(&mut buffer, offset)?;
                        Ok(buffer)
                    }
                }
            }
        }
    }

    async fn handle_write(&mut self, content: &[u8]) -> Result<u64, zx::Status> {
        let actual = self.handle_write_at(self.seek, content).await?;
        self.seek += actual;
        Ok(actual)
    }

    async fn handle_write_at(
        &mut self,
        offset: u64,
        mut content: &[u8],
    ) -> Result<u64, zx::Status> {
        if !self.flags.intersects(fio::OpenFlags::RIGHT_WRITABLE) {
            return Err(zx::Status::BAD_HANDLE);
        }

        update_initialized_state! {
            match &mut *self.file.state().await;
            error: "handle_write_at" => return Err(zx::Status::INTERNAL);
            { vmo, vmo_size, size, capacity, .. } => {
                let capacity = core::cmp::max(*size, *capacity);
                match capacity.checked_sub(offset) {
                    None => return Err(zx::Status::OUT_OF_RANGE),
                    Some(capacity) => {
                        assert_eq_size!(usize, u64);
                        let capacity = capacity.try_into().unwrap();

                        if content.len() > capacity {
                            content = &content[..capacity];
                        }

                        let len = content.len().try_into().unwrap();
                        let end = offset + len;
                        if end > *size {
                            if end > *vmo_size {
                                vmo.set_size(end)?;
                                // As VMO sizes are rounded, we do not really know the current size
                                // of the VMO after the `set_size` call.  We need an additional
                                // `get_size`, if we want to be aware of the exact size.  We can
                                // probably do our own rounding, but it seems more fragile.
                                // Hopefully, this extra syscall will be invisible, as it should not
                                // happen too frequently.  It will be at least offset by 4 more
                                // syscalls that happen for every `write_at` FIDL call.
                                *vmo_size = vmo.get_size()?;
                            }
                            vmo.set_content_size(&end)?;
                            *size = end;
                        }
                        vmo.write(content, offset)?;
                        Ok(len)
                    }
                }
            }
        }
    }

    /// Move seek position to byte `offset` relative to the origin specified by `start.  Calls
    /// `responder` with an updated seek position, on success.
    async fn handle_seek(
        &mut self,
        offset: i64,
        origin: fio::SeekOrigin,
    ) -> Result<u64, zx::Status> {
        if self.flags.intersects(fio::OpenFlags::NODE_REFERENCE) {
            return Err(zx::Status::BAD_HANDLE);
        }

        update_initialized_state! {
            match *self.file.state().await;
            error: "handle_seek" => return Err(zx::Status::INTERNAL);
            { size, .. } => {
                // There is an undocumented constraint that the seek offset can never exceed 63
                // bits. See https://fxbug.dev/100754.
                let origin: i64 = match origin {
                    fio::SeekOrigin::Start => 0,
                    fio::SeekOrigin::Current => self.seek,
                    fio::SeekOrigin::End => size,
                }.try_into().unwrap();
                match origin.checked_add(offset) {
                    None => Err(zx::Status::OUT_OF_RANGE),
                    Some(offset) => {
                        let offset = offset.try_into().map_err(|std::num::TryFromIntError { .. }| zx::Status::OUT_OF_RANGE)?;
                        self.seek = offset;
                        Ok(offset)
                    }
                }
            }
        }
    }

    async fn handle_truncate(&mut self, length: u64) -> Result<(), zx::Status> {
        if !self.flags.intersects(fio::OpenFlags::RIGHT_WRITABLE) {
            return Err(zx::Status::BAD_HANDLE);
        }

        Self::truncate_vmo(&mut *self.file.state().await, length, &mut self.seek)
    }

    async fn handle_get_buffer(&mut self, flags: fio::VmoFlags) -> Result<Buffer, zx::Status> {
        let () = get_buffer_validate_flags(flags, self.flags)?;

        // The only sharing mode we support that disallows the VMO size to change currently
        // is VMO_FLAG_PRIVATE (`get_as_private`), so we require that to be set explicitly.
        if flags.contains(fio::VmoFlags::WRITE) && !flags.contains(fio::VmoFlags::PRIVATE_CLONE) {
            return Err(zx::Status::NOT_SUPPORTED);
        }

        // Disallow opening as both writable and executable. In addition to improving W^X
        // enforcement, this also eliminates any inconstiencies related to clones that use
        // SNAPSHOT_AT_LEAST_ON_WRITE since in that case, we cannot satisfy both requirements.
        if flags.contains(fio::VmoFlags::EXECUTE) && flags.contains(fio::VmoFlags::WRITE) {
            return Err(zx::Status::NOT_SUPPORTED);
        }

        update_initialized_state! {
            match &*self.file.state().await;
            error: "handle_get_buffer" => Err(zx::Status::INTERNAL);
            { vmo, size, .. } => {
                let () = vmo.set_content_size(&size)?;

                // Logic here matches fuchsia.io requirements and matches what works for memfs.
                // Shared requests are satisfied by duplicating an handle, and private shares are
                // child VMOs.
                //
                // Minfs and blobfs may require customization.  In particular, they may want to
                // track not just number of connections to a file, but also the number of
                // outstanding child VMOs.  While it is possible with the `init_vmo`
                // model currently implemented, it is very likely that adding another customization
                // callback here will make the implementation of those files systems easier.
                let vmo_rights = vmo_flags_to_rights(flags);
                // Unless private sharing mode is specified, we always default to shared.
                let vmo = if flags.contains(fio::VmoFlags::PRIVATE_CLONE) {
                    Self::get_as_private(&vmo, vmo_rights, *size)
                }
                else {
                    Self::get_as_shared(&vmo, vmo_rights)
                }?;
                    Ok(Buffer{vmo, size: *size})
            }
        }
    }

    fn get_as_shared(vmo: &zx::Vmo, mut rights: zx::Rights) -> Result<zx::Vmo, zx::Status> {
        // Add set of basic rights to include in shared mode before duplicating the VMO handle.
        rights |= zx::Rights::BASIC | zx::Rights::MAP | zx::Rights::GET_PROPERTY;
        vmo.as_handle_ref().duplicate(rights).map(Into::into)
    }

    fn get_as_private(
        vmo: &zx::Vmo,
        mut rights: zx::Rights,
        size: u64,
    ) -> Result<zx::Vmo, zx::Status> {
        // Add set of basic rights to include in private mode, ensuring we provide SET_PROPERTY.
        rights |= zx::Rights::BASIC
            | zx::Rights::MAP
            | zx::Rights::GET_PROPERTY
            | zx::Rights::SET_PROPERTY;

        // Ensure we give out a copy-on-write clone.
        let mut child_options = zx::VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE;
        // If we don't need a writable clone, we need to add CHILD_NO_WRITE since
        // SNAPSHOT_AT_LEAST_ON_WRITE removes ZX_RIGHT_EXECUTE even if the parent VMO has it, but
        // adding CHILD_NO_WRITE will ensure EXECUTE is maintained.
        if !rights.contains(zx::Rights::WRITE) {
            child_options |= zx::VmoChildOptions::NO_WRITE;
        } else {
            // If we need a writable clone, ensure it can be resized.
            child_options |= zx::VmoChildOptions::RESIZABLE;
        }

        let new_vmo = vmo.create_child(child_options, 0, size)?;
        new_vmo.into_handle().replace_handle(rights).map(Into::into)
    }
}
