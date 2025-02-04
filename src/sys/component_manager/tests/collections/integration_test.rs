// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{Context as _, Error},
    fidl::endpoints::{self, Proxy},
    fidl::{AsHandleRef, HandleBased},
    fidl_fidl_test_components as ftest, fidl_fuchsia_component as fcomponent,
    fidl_fuchsia_component_decl as fdecl, fidl_fuchsia_io as fio, fidl_fuchsia_process as fprocess,
    fuchsia_async as fasync,
    fuchsia_component::client,
    fuchsia_runtime::{HandleInfo, HandleType},
    fuchsia_zircon as zx,
    io_util::{self, OpenFlags},
    std::ffi::CString,
    std::path::PathBuf,
};

#[fasync::run_singlethreaded(test)]
async fn collections() {
    let realm = client::connect_to_protocol::<fcomponent::RealmMarker>()
        .expect("could not connect to Realm service");

    // Create a couple child components.
    for name in vec!["a", "b"] {
        let mut collection_ref = fdecl::CollectionRef { name: "coll".to_string() };
        let child_decl = fdecl::Child {
            name: Some(name.to_string()),
            url: Some(format!(
                "fuchsia-pkg://fuchsia.com/collections_integration_test#meta/trigger_{}.cm",
                name
            )),
            startup: Some(fdecl::StartupMode::Lazy),
            environment: None,
            ..fdecl::Child::EMPTY
        };
        realm
            .create_child(&mut collection_ref, child_decl, fcomponent::CreateChildArgs::EMPTY)
            .await
            .expect(&format!("create_child {} failed", name))
            .expect(&format!("failed to create child {}", name));
    }

    let children = list_children(&realm).await.expect("failed to list children");
    assert_eq!("coll:a,coll:b", &children);

    // Start the children.
    for name in vec!["a", "b"] {
        let mut child_ref = new_child_ref(name, "coll");
        let (dir, server_end) = endpoints::create_proxy::<fio::DirectoryMarker>().unwrap();
        realm
            .open_exposed_dir(&mut child_ref, server_end)
            .await
            .expect(&format!("open_exposed_dir {} failed", name))
            .expect(&format!("failed to open exposed dir of child {}", name));
        let trigger = open_trigger_svc(&dir).expect("failed to open trigger service");
        let out = trigger.run().await.expect(&format!("trigger {} failed", name));
        assert_eq!(out, format!("Triggered {}", name));
    }

    // Destroy one.
    {
        let mut child_ref = new_child_ref("a", "coll");
        realm
            .destroy_child(&mut child_ref)
            .await
            .expect("destroy_child a failed")
            .expect("failed to destroy child");
    }

    // Binding to destroyed child should fail.
    {
        let (_, server_end) = endpoints::create_proxy::<fio::DirectoryMarker>().unwrap();
        let mut child_ref = new_child_ref("a", "coll");
        let res = realm
            .open_exposed_dir(&mut child_ref, server_end)
            .await
            .expect("second open_exposed_dir a failed");
        let err = res.expect_err("expected open_exposed_dir a to fail");
        assert_eq!(err, fcomponent::Error::InstanceNotFound);
    }

    let children = list_children(&realm).await.expect("failed to list children");
    assert_eq!("coll:b", &children);

    // Recreate child (with different URL), and start it. Should work.
    {
        let mut collection_ref = fdecl::CollectionRef { name: "coll".to_string() };
        let child_decl = fdecl::Child {
            name: Some("a".to_string()),
            url: Some(
                "fuchsia-pkg://fuchsia.com/collections_integration_test#meta/trigger_realm.cm"
                    .to_string(),
            ),
            startup: Some(fdecl::StartupMode::Lazy),
            environment: None,
            ..fdecl::Child::EMPTY
        };
        realm
            .create_child(&mut collection_ref, child_decl, fcomponent::CreateChildArgs::EMPTY)
            .await
            .expect("second create_child a failed")
            .expect("failed to create second child a");
    }
    {
        let (dir, server_end) = endpoints::create_proxy::<fio::DirectoryMarker>().unwrap();
        let mut child_ref = new_child_ref("a", "coll");
        realm
            .open_exposed_dir(&mut child_ref, server_end)
            .await
            .expect("open_exposed_dir a failed")
            .expect("failed to open exposed dir of child a");
        let trigger = open_trigger_svc(&dir).expect("failed to open trigger service");
        let out = trigger.run().await.expect("second trigger a failed");
        assert_eq!(&out, "Triggered a");
    }

    let children = list_children(&realm).await.expect("failed to list children");
    assert_eq!("coll:a,coll:b", &children);
}

#[fasync::run_singlethreaded(test)]
async fn child_args() {
    let realm = client::connect_to_protocol::<fcomponent::RealmMarker>()
        .expect("could not connect to Realm service");

    // Providing numbered handles to a component that is not in a single run collection should fail.
    {
        let name = "a";
        let mut collection_ref = fdecl::CollectionRef { name: "not_single_run".to_string() };
        let child_decl = fdecl::Child {
            name: Some(name.to_string()),
            url: Some(format!(
                "fuchsia-pkg://fuchsia.com/collections_integration_test#meta/trigger_{}.cm",
                name
            )),
            startup: Some(fdecl::StartupMode::Lazy),
            environment: None,
            ..fdecl::Child::EMPTY
        };
        let (_, socket) =
            zx::Socket::create(zx::SocketOpts::STREAM).expect("Couldn't create socket");
        let numbered_handles = vec![fprocess::HandleInfo { handle: socket.into_handle(), id: 0 }];
        let child_args = fcomponent::CreateChildArgs {
            numbered_handles: Some(numbered_handles),
            ..fcomponent::CreateChildArgs::EMPTY
        };
        let res = realm
            .create_child(&mut collection_ref, child_decl, child_args)
            .await
            .expect(&format!("create_child {} failed", name));
        let err = res.expect_err("expected create_child a to fail");
        assert_eq!(err, fcomponent::Error::Unsupported);
    }
    // Providing numbered handles to a component that is in a single run collection should succeed.
    {
        let mut collection_ref = fdecl::CollectionRef { name: "single_run".to_owned() };
        let child_decl = fdecl::Child {
            name: Some("write_startup_socket".to_owned()),
            url: Some(
                "fuchsia-pkg://fuchsia.com/collections_integration_test#meta/write_startup_socket.cm".to_owned(),
            ),
            startup: Some(fdecl::StartupMode::Lazy),
            environment: None,
            ..fdecl::Child::EMPTY
        };
        let (their_socket, our_socket) =
            zx::Socket::create(zx::SocketOpts::STREAM).expect("Couldn't create socket");
        let numbered_handles = vec![fprocess::HandleInfo {
            handle: their_socket.into_handle(),
            // Must correspond to the same Handle Id in write_startup_socket.rs.
            id: HandleInfo::new(HandleType::User0, 0).as_raw(),
        }];
        let child_args = fcomponent::CreateChildArgs {
            numbered_handles: Some(numbered_handles),
            ..fcomponent::CreateChildArgs::EMPTY
        };
        realm
            .create_child(&mut collection_ref, child_decl, child_args)
            .await
            .expect("fidl error in create_child")
            .expect("failed to create_child");

        // NOTE: Although we specified StartupMode::Lazy above, which is the only supported mode for
        // dynamic components. |create_child| will actually start the component immediately without
        // the need to manually bind it, because "the instances in a single run collection are
        // started when they are created" (see //docs/concepts/components/v2/realms.md).
        // fxbug.dev/90085 discusses the idea to remove the startup parameter.

        fasync::OnSignals::new(&our_socket, zx::Signals::SOCKET_READABLE)
            .await
            .expect("SOCKET_READABLE should be signaled after the child starts");
        let mut bytes = [0u8; 32];
        let length = our_socket.read(&mut bytes).expect("read should succeed");
        assert_eq!(bytes[..length], b"Hello, World!"[..]);

        // Checking OBJECT_PEER_CLOSED ensures that |their_socket| is not leaked.
        fasync::OnSignals::new(&our_socket, zx::Signals::OBJECT_PEER_CLOSED)
            .await
            .expect("OBJECT_PEER_CLOSED should be signaled after the child exits");
    }
    // Providing numbered handles with invalid id should fail.
    {
        let mut collection_ref = fdecl::CollectionRef { name: "single_run".to_owned() };
        let child_decl = fdecl::Child {
            name: Some("write_startup_socket_2".to_owned()),
            url: Some(
                "fuchsia-pkg://fuchsia.com/collections_integration_test#meta/write_startup_socket.cm".to_owned(),
            ),
            startup: Some(fdecl::StartupMode::Lazy),
            environment: None,
            ..fdecl::Child::EMPTY
        };
        let (their_socket, our_socket) =
            zx::Socket::create(zx::SocketOpts::STREAM).expect("Couldn't create socket");
        let job =
            fuchsia_runtime::job_default().create_child_job().expect("fail to create_child_job");
        job.set_name(&CString::new("new-job-new-name").unwrap()).expect("fail to set_name");
        let numbered_handles = vec![
            fprocess::HandleInfo {
                handle: job.into_handle(),
                // Only PA_FD and PA_USER* handles are valid arguments (//sdk/fidl/fuchsia.component/realm.fidl).
                id: HandleInfo::new(HandleType::DefaultJob, 0).as_raw(),
            },
            fprocess::HandleInfo {
                handle: their_socket.into_handle(),
                // Must correspond to the same Handle Id in write_startup_socket.rs.
                id: HandleInfo::new(HandleType::User0, 0).as_raw(),
            },
        ];
        let child_args = fcomponent::CreateChildArgs {
            numbered_handles: Some(numbered_handles),
            ..fcomponent::CreateChildArgs::EMPTY
        };
        let result = realm
            .create_child(&mut collection_ref, child_decl, child_args)
            .await
            .expect("fidl error in create_child");

        // TODO(fxbug.dev/98739): it should fail to create_child, but it succeeds instead. Also
        // the default_job received in write_startup_socket.rs is NOT the job we created above.
        result.unwrap();

        // Wait for the component to exit (to mute the log).
        fasync::OnSignals::new(&our_socket, zx::Signals::OBJECT_PEER_CLOSED)
            .await
            .expect("OBJECT_PEER_CLOSED should be signaled after the child exits");
    }
}

fn new_child_ref(name: &str, collection: &str) -> fdecl::ChildRef {
    fdecl::ChildRef { name: name.to_string(), collection: Some(collection.to_string()) }
}

async fn list_children(realm: &fcomponent::RealmProxy) -> Result<String, Error> {
    let (iterator_proxy, server_end) = endpoints::create_proxy().unwrap();
    let mut collection_ref = fdecl::CollectionRef { name: "coll".to_string() };
    realm
        .list_children(&mut collection_ref, server_end)
        .await
        .expect("list_children failed")
        .expect("failed to list children");
    let res = iterator_proxy.next().await;
    let children = res.expect("failed to iterate over children");
    let children: Vec<_> = children
        .iter()
        .map(|c| format!("{}:{}", c.collection.as_ref().expect("no collection"), &c.name))
        .collect();
    Ok(children.join(","))
}

fn open_trigger_svc(dir: &fio::DirectoryProxy) -> Result<ftest::TriggerProxy, Error> {
    let node_proxy = io_util::open_node(
        dir,
        &PathBuf::from("fidl.test.components.Trigger"),
        OpenFlags::RIGHT_READABLE,
        fio::MODE_TYPE_SERVICE,
    )
    .context("failed to open trigger service")?;
    Ok(ftest::TriggerProxy::new(node_proxy.into_channel().unwrap()))
}
