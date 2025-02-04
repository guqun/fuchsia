// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/virtualization/bin/host_vsock/guest_vsock_endpoint.h"

GuestVsockEndpoint::GuestVsockEndpoint(
    uint32_t cid, fuchsia::virtualization::GuestVsockEndpointPtr guest_endpoint,
    fuchsia::virtualization::HostVsockConnector* connector)
    : connector_binding_(connector), guest_endpoint_(std::move(guest_endpoint)) {
  guest_endpoint_->SetContextId(cid, connector_binding_.NewBinding(), acceptor_.NewRequest());
}

void GuestVsockEndpoint::Accept(uint32_t src_cid, uint32_t src_port, uint32_t port,
                                zx::socket socket, AcceptCallback callback) {
  acceptor_->Accept(src_cid, src_port, port, std::move(socket), std::move(callback));
}
