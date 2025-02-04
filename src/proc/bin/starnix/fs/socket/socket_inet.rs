// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_zircon as zx;

use super::*;

use crate::fs::buffers::*;
use crate::fs::*;
use crate::logging::not_implemented;
use crate::task::*;
use crate::types::*;

use std::sync::Arc;

// This is a stubbed version of AF_INET/AF_INET6
pub struct InetSocket {}

impl InetSocket {
    pub fn new(_socket_type: SocketType) -> InetSocket {
        InetSocket {}
    }
}

impl SocketOps for InetSocket {
    fn connect(
        &self,
        _socket: &SocketHandle,
        _peer: &SocketHandle,
        _credentials: ucred,
    ) -> Result<(), Errno> {
        error!(ENOSYS)
    }

    fn listen(&self, _socket: &Socket, _backlog: i32) -> Result<(), Errno> {
        not_implemented!("InetSocket::listen is stubbed");
        Ok(())
    }

    fn accept(&self, _socket: &Socket, _credentials: ucred) -> Result<SocketHandle, Errno> {
        not_implemented!("InetSocket::accept is stubbed");
        error!(EAGAIN)
    }

    fn remote_connection(&self, _socket: &Socket, _file: FileHandle) -> Result<(), Errno> {
        not_implemented!("InetSocket::remote_connection is stubbed");
        Ok(())
    }

    fn bind(&self, _socket: &Socket, _socket_address: SocketAddress) -> Result<(), Errno> {
        not_implemented!("InetSocket::bind is stubbed");
        Ok(())
    }

    fn read(
        &self,
        _socket: &Socket,
        _current_task: &CurrentTask,
        _user_buffers: &mut UserBufferIterator<'_>,
        _flags: SocketMessageFlags,
    ) -> Result<MessageReadInfo, Errno> {
        error!(ENOSYS)
    }

    fn write(
        &self,
        _socket: &Socket,
        _current_task: &CurrentTask,
        _user_buffers: &mut UserBufferIterator<'_>,
        _dest_address: &mut Option<SocketAddress>,
        _ancillary_data: &mut Vec<AncillaryData>,
    ) -> Result<usize, Errno> {
        error!(ENOSYS)
    }

    fn wait_async(
        &self,
        _socket: &Socket,
        _current_task: &CurrentTask,
        _waiter: &Arc<Waiter>,
        _events: FdEvents,
        _handler: EventHandler,
        _options: WaitAsyncOptions,
    ) -> WaitKey {
        not_implemented!("InetSocket::wait_async is stubbed");
        WaitKey::empty()
    }

    fn cancel_wait(
        &self,
        _socket: &Socket,
        _current_task: &CurrentTask,
        _waiter: &Arc<Waiter>,
        _key: WaitKey,
    ) {
    }

    fn query_events(&self, _socket: &Socket, _current_task: &CurrentTask) -> FdEvents {
        not_implemented!("InetSocket::query_events is stubbed");
        FdEvents::empty()
    }

    fn shutdown(&self, _socket: &Socket, _how: SocketShutdownFlags) -> Result<(), Errno> {
        Ok(())
    }

    fn close(&self, _socket: &Socket) {}

    fn getsockname(&self, _socket: &Socket) -> Vec<u8> {
        vec![]
    }

    fn getpeername(&self, _socket: &Socket) -> Result<Vec<u8>, Errno> {
        error!(ENOTCONN)
    }

    fn setsockopt(
        &self,
        _socket: &Socket,
        _task: &Task,
        _level: u32,
        _optname: u32,
        _user_opt: UserBuffer,
    ) -> Result<(), Errno> {
        error!(ENOPROTOOPT)
    }

    fn getsockopt(&self, _socket: &Socket, _level: u32, _optname: u32) -> Result<Vec<u8>, Errno> {
        error!(ENOPROTOOPT)
    }

    fn get_receive_timeout(&self, _socket: &Socket) -> Option<zx::Duration> {
        None
    }

    fn get_send_timeout(&self, _socket: &Socket) -> Option<zx::Duration> {
        None
    }
}
