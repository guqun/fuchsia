// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fidl_examples_routing_echo::EchoMarker;
use fidl_fuchsia_sys2::{InstanceState, RealmExplorerMarker, RealmQueryMarker};
use fuchsia_component::client::*;

#[fuchsia::test]
pub async fn query_self() {
    let query = connect_to_protocol::<RealmQueryMarker>().unwrap();

    let (info, resolved) = query.get_instance_info("./").await.unwrap().unwrap();
    assert!(info.url.starts_with("fuchsia-pkg://fuchsia.com/realm_integration_test"));
    assert!(info.url.ends_with("#meta/realm_integration_test.cm"));
    assert_eq!(info.state, InstanceState::Started);

    let resolved = resolved.unwrap();
    assert_eq!(resolved.uses.len(), 4);
    assert_eq!(resolved.exposes.len(), 1);
    let started = resolved.started.unwrap();
    started.out_dir.unwrap();

    // Test runners are not providing the runtime directory for test components
    assert!(started.runtime_dir.is_none());
}

#[fuchsia::test]
pub async fn query_echo_server_child() {
    let query = connect_to_protocol::<RealmQueryMarker>().unwrap();

    let (info, resolved) = query.get_instance_info("./echo_server").await.unwrap().unwrap();
    assert_eq!(info.url, "#meta/echo_server.cm");
    assert_eq!(info.state, InstanceState::Unresolved);
    assert!(resolved.is_none());

    // Now connect to the Echo protocol, thus starting the echo_server
    let echo = connect_to_protocol::<EchoMarker>().unwrap();
    let reply = echo.echo_string(Some("test")).await.unwrap();
    assert_eq!(reply.unwrap(), "test");

    let (info, resolved) = query.get_instance_info("./echo_server").await.unwrap().unwrap();
    assert_eq!(info.url, "#meta/echo_server.cm");
    assert_eq!(info.state, InstanceState::Started);

    let resolved = resolved.unwrap();
    assert_eq!(resolved.uses.len(), 1);
    assert_eq!(resolved.exposes.len(), 3);

    let pkg_dir = resolved.pkg_dir.unwrap();
    let pkg_dir = pkg_dir.into_proxy().unwrap();
    let entries = files_async::readdir(&pkg_dir).await.unwrap();
    assert_eq!(
        entries,
        vec![
            files_async::DirEntry {
                name: "bin".to_string(),
                kind: files_async::DirentKind::Directory,
            },
            files_async::DirEntry {
                name: "lib".to_string(),
                kind: files_async::DirentKind::Directory,
            },
            files_async::DirEntry {
                name: "meta".to_string(),
                kind: files_async::DirentKind::Directory,
            }
        ]
    );

    let started = resolved.started.unwrap();

    let out_dir = started.out_dir.unwrap();
    let out_dir = out_dir.into_proxy().unwrap();
    let echo = connect_to_protocol_at_dir_svc::<EchoMarker>(&out_dir).unwrap();
    let reply = echo.echo_string(Some("test")).await.unwrap();
    assert_eq!(reply.unwrap(), "test");

    let runtime_dir = started.runtime_dir.unwrap();
    let runtime_dir = runtime_dir.into_proxy().unwrap();
    let elf_dir =
        io_util::directory::open_directory(&runtime_dir, "elf", io_util::OpenFlags::RIGHT_READABLE)
            .await
            .unwrap();
    let mut entries = files_async::readdir(&elf_dir).await.unwrap();

    // TODO(http://fxbug.dev/99823): The existence of "process_start_time_utc_estimate" is flaky.
    if let Some(position) = entries.iter().position(|e| e.name == "process_start_time_utc_estimate")
    {
        entries.remove(position);
    }

    assert_eq!(
        entries,
        vec![
            files_async::DirEntry {
                name: "job_id".to_string(),
                kind: files_async::DirentKind::File,
            },
            files_async::DirEntry {
                name: "process_id".to_string(),
                kind: files_async::DirentKind::File,
            },
            files_async::DirEntry {
                name: "process_start_time".to_string(),
                kind: files_async::DirentKind::File,
            },
        ]
    );
}

#[fuchsia::test]
pub async fn query_will_not_resolve_child() {
    let query = connect_to_protocol::<RealmQueryMarker>().unwrap();

    let (info, resolved) = query.get_instance_info("./will_not_resolve").await.unwrap().unwrap();
    assert_eq!(info.url, "fuchsia-pkg://fake.com");
    assert_eq!(info.state, InstanceState::Unresolved);
    assert!(resolved.is_none());
}

#[fuchsia::test]
pub async fn explorer_get_instances() {
    let explorer = connect_to_protocol::<RealmExplorerMarker>().unwrap();
    let iterator = explorer.get_all_instance_infos().await.unwrap().unwrap();
    let iterator = iterator.into_proxy().unwrap();
    let mut instances = vec![];

    loop {
        let mut batch = iterator.next().await.unwrap();
        if batch.is_empty() {
            break;
        }
        instances.append(&mut batch);
    }

    // This component and the two children
    assert_eq!(instances.len(), 3);

    for instance in instances {
        if instance.url.ends_with("#meta/realm_integration_test.cm") {
            // This component is definitely resolved and started
            assert_eq!(instance.moniker, ".");
            assert_eq!(instance.state, InstanceState::Started);
        } else if instance.url == "#meta/echo_server.cm" {
            // The other test case may start this component so its state is not stable
            assert_eq!(instance.moniker, "./echo_server");
        } else if instance.url == "fuchsia-pkg://fake.com" {
            // This component can never be resolved or started
            assert_eq!(instance.moniker, "./will_not_resolve");
            assert_eq!(instance.state, InstanceState::Unresolved);
        } else {
            panic!("Unknown instance: {}", instance.url);
        }
    }
}
