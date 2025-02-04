// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{self, Context};
use fidl_fidl_examples_routing_echo::{EchoRequest, EchoRequestStream};
use fuchsia_component::server::ServiceFs;
use futures::prelude::*;

// Wrap protocol requests being served.
enum IncomingRequest {
    Echo(EchoRequestStream),
}

#[fuchsia::main(logging = false)]
async fn main() -> Result<(), anyhow::Error> {
    let mut service_fs = ServiceFs::new_local();

    // Serve the Echo protocol
    service_fs.dir("svc").add_fidl_service(IncomingRequest::Echo);
    service_fs.take_and_serve_directory_handle().context("failed to serve outgoing namespace")?;

    // Attach request handler for incoming requests
    service_fs
        .for_each_concurrent(None, |request: IncomingRequest| async move {
            match request {
                IncomingRequest::Echo(stream) => handle_echo_request(stream).await,
            }
        })
        .await;

    Ok(())
}

// Handler for incoming service requests
async fn handle_echo_request(mut stream: EchoRequestStream) {
    let default_reply = match std::env::args().skip(1).next() {
        Some(user_specified_reply) => user_specified_reply,
        None => "UNSET".to_string(),
    };

    while let Some(event) = stream.try_next().await.expect("failed to serve echo service") {
        let EchoRequest::EchoString { value, responder } = event;
        responder
            .send(Some(value.unwrap_or(default_reply.to_string())).as_ref().map(|s| &**s))
            .expect("failed to send echo response");
    }
}
