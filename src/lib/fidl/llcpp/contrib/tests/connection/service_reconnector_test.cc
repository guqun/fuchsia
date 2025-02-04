// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/fidl/llcpp/contrib/connection/service_reconnector.h"

#include <fidl/test.protocol/cpp/fidl.h>
#include <lib/gtest/test_loop_fixture.h>
#include <lib/service/llcpp/service.h>

#include <functional>
#include <optional>
#include <queue>
#include <type_traits>

#include <sdk/lib/sys/component/llcpp/outgoing_directory.h>

#include "fidl/test.protocol/cpp/natural_types.h"
#include "lib/async/cpp/task.h"
#include "lib/fidl/llcpp/channel.h"
#include "src/lib/storage/vfs/cpp/pseudo_dir.h"
#include "src/lib/storage/vfs/cpp/service.h"
#include "src/lib/storage/vfs/cpp/synchronous_vfs.h"

namespace {

using fidl::contrib::ServiceReconnector;
using test_protocol::Error;
using test_protocol::SimpleProtocol;

class SimpleProtocolImpl : public fidl::Server<SimpleProtocol> {
 public:
  SimpleProtocolImpl() = default;

  void DoAction(DoActionRequest& request, DoActionCompleter::Sync& completer) override {
    actions_attempted_ += 1;
    fitx::result<Error> result = fitx::ok();
    if (auto error = next_error()) {
      result = fitx::as_error(*error);
    } else {
      actions_successful_ += 1;
    }

    completer.Reply(result);
  }

  size_t ActionsAttempted() const { return actions_attempted_; }
  size_t ActionsSuccessful() const { return actions_successful_; }
  void QueueError(Error error) { queued_errors_.push(error); }

 private:
  std::optional<Error> next_error() {
    if (queued_errors_.empty()) {
      return std::nullopt;
    }
    Error retval = queued_errors_.front();
    queued_errors_.pop();
    return retval;
  }

  size_t actions_attempted_ = 0;
  size_t actions_successful_ = 0;
  std::queue<Error> queued_errors_;
};

}  // namespace

class ServiceReconnectorTest : public gtest::TestLoopFixture {
 public:
  ServiceReconnectorTest() = default;
  ~ServiceReconnectorTest() override = default;

  // Disallow copy and assign.
  ServiceReconnectorTest(const ServiceReconnectorTest&) = delete;
  ServiceReconnectorTest& operator=(const ServiceReconnectorTest&) = delete;
  ServiceReconnectorTest(ServiceReconnectorTest&&) = delete;
  ServiceReconnectorTest& operator=(ServiceReconnectorTest&&) = delete;

  SimpleProtocolImpl& protocol() { return *protocol_impl_; }
  std::shared_ptr<ServiceReconnector<SimpleProtocol>> reconnector() { return reconnector_; }

  void ReplaceProtocol() {
    // Close all existing connections.
    if (!server_bindings_.empty()) {
      std::vector<fidl::ServerBindingRef<test_protocol::SimpleProtocol>> old_bindings;
      old_bindings.swap(server_bindings_);

      for (auto binding : old_bindings) {
        binding.Close(ZX_ERR_PEER_CLOSED);
      }
      // Wait until all the closes happen.
      RunLoopUntilIdle();
    }

    // Create new factory impl.
    protocol_impl_ = std::make_unique<SimpleProtocolImpl>();
  }

  fidl::UnownedClientEnd<fuchsia_io::Directory> svc() const { return svc_dir_; }

  std::shared_ptr<ServiceReconnector<SimpleProtocol>> MakeReconnector() {
    return ServiceReconnector<SimpleProtocol>::Create(
        dispatcher(), "SimpleProtocol",
        [this](ServiceReconnector<SimpleProtocol>::ConnectResolver resolver) {
          auto connection = service::ConnectAt<SimpleProtocol>(svc());
          if (connection.is_error()) {
            resolver.resolve(std::nullopt);
          } else {
            resolver.resolve(std::move(connection.value()));
          }
        });
  }

  void DoAction() {
    reconnector()->Do([](fidl::Client<SimpleProtocol>& client) {
      client->DoAction().Then([](fidl::Result<test_protocol::SimpleProtocol::DoAction>& resp) {});
    });
  }

 private:
  void SetUp() override {
    ReplaceProtocol();

    // Serve ProtocolFactory
    outgoing_directory_ = std::make_unique<component::OutgoingDirectory>(
        component::OutgoingDirectory::Create(dispatcher()));
    ASSERT_EQ(ZX_OK, outgoing_directory_
                         ->AddProtocol<test_protocol::SimpleProtocol>(
                             [this](fidl::ServerEnd<test_protocol::SimpleProtocol> request) {
                               server_bindings_.push_back(fidl::BindServer(
                                   dispatcher(), std::move(request), protocol_impl_.get()));
                             })
                         .status_value());

    // Connect to /svc endpoint
    auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
    ASSERT_EQ(ZX_OK, outgoing_directory_->Serve(std::move(endpoints->server)).status_value());
    root_dir_ = std::move(endpoints->client);

    auto svc_dir = service::ConnectAt<fuchsia_io::Directory>(root_dir_, "svc");
    ASSERT_EQ(ZX_OK, svc_dir.status_value());
    svc_dir_ = std::move(svc_dir.value());

    reconnector_ = MakeReconnector();

    RunLoopUntilIdle();
  }

  std::unique_ptr<component::OutgoingDirectory> outgoing_directory_ = nullptr;
  fidl::ClientEnd<fuchsia_io::Directory> root_dir_;
  fidl::ClientEnd<fuchsia_io::Directory> svc_dir_;
  std::unique_ptr<SimpleProtocolImpl> protocol_impl_;
  std::vector<fidl::ServerBindingRef<test_protocol::SimpleProtocol>> server_bindings_;
  std::shared_ptr<ServiceReconnector<SimpleProtocol>> reconnector_;
};

TEST_F(ServiceReconnectorTest, CallMethodAfterInitialization) {
  DoAction();
  RunLoopFor(zx::min(100));
  ASSERT_EQ(protocol().ActionsSuccessful(), 1U);
}

TEST_F(ServiceReconnectorTest, HandlesServiceFailure) {
  constexpr size_t num_actions = 10u;

  for (size_t i = 0; i < num_actions; i++) {
    DoAction();
  }
  RunLoopUntilIdle();
  ASSERT_EQ(protocol().ActionsSuccessful(), num_actions);

  // Kill the protocol factory
  ReplaceProtocol();

  for (size_t i = 0; i < num_actions; i++) {
    DoAction();
  }

  // RunLoop for 10 minutes to ensure that reconnect will be tried.
  RunLoopFor(zx::min(10));

  ASSERT_EQ(protocol().ActionsSuccessful(), num_actions);
}

TEST_F(ServiceReconnectorTest, HandlesErrors) {
  DoAction();
  RunLoopUntilIdle();
  ASSERT_EQ(protocol().ActionsSuccessful(), 1U);

  protocol().QueueError(Error::kPermanent);
  protocol().QueueError(Error::kTransient);
  DoAction();
  DoAction();
  DoAction();
  RunLoopFor(zx::min(10));

  ASSERT_EQ(protocol().ActionsAttempted(), 4U);
  ASSERT_EQ(protocol().ActionsSuccessful(), 2U);
}

TEST_F(ServiceReconnectorTest, SupportsCallsFromMultipleThreads) {
  auto thread_1 = std::thread([=]() { DoAction(); });
  auto thread_2 = std::thread([=]() { DoAction(); });
  thread_1.join();
  thread_2.join();

  RunLoopUntilIdle();
  ASSERT_EQ(protocol().ActionsAttempted(), 2U);
  ASSERT_EQ(protocol().ActionsSuccessful(), 2U);
}
