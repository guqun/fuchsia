// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/fidl/cpp/internal/stub_controller.h"

#include <lib/fidl/cpp/message_buffer.h>
#include <lib/fidl/cpp/message_builder.h>
#include <lib/zx/channel.h>
#include <zircon/fidl.h>
#include <zircon/types.h>

#include <limits>

#include <zxtest/zxtest.h>

#include "lib/fidl/cpp/internal/proxy_controller.h"
#include "lib/fidl/cpp/internal/stub.h"
#include "lib/fidl/cpp/string.h"
#include "testing/fidl/async_loop_for_test.h"
#include "testing/fidl/fidl_types.h"

namespace fidl {
namespace internal {
namespace {

class CallbackStub : public Stub {
 public:
  fit::function<zx_status_t(HLCPPIncomingMessage, PendingResponse)> callback;

  zx_status_t Dispatch_(HLCPPIncomingMessage message, PendingResponse response) override {
    return callback(std::move(message), std::move(response));
  }
};

TEST(StubController, Trivial) { StubController controller; }

TEST(StubController, NoResponse) {
  zx::channel h1, h2;
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &h1, &h2));

  fidl::test::AsyncLoopForTest loop;

  StubController stub_ctrl;
  EXPECT_EQ(ZX_OK, stub_ctrl.reader().Bind(std::move(h1)));

  ProxyController proxy_ctrl;
  EXPECT_EQ(ZX_OK, proxy_ctrl.reader().Bind(std::move(h2)));

  CallbackStub stub;

  int callback_count = 0;
  stub.callback = [&callback_count](HLCPPIncomingMessage message, PendingResponse response) {
    ++callback_count;
    EXPECT_EQ(5u, message.ordinal());
    EXPECT_FALSE(response.needs_response());
    EXPECT_EQ(ZX_ERR_BAD_STATE,
              response.Send(&unbounded_nonnullable_string_message_type, HLCPPOutgoingMessage()));
    return ZX_OK;
  };

  stub_ctrl.set_stub(&stub);

  MessageEncoder encoder(5u, fidl::MessageDynamicFlags::kStrictMethod);
  StringPtr string("hello!");
  fidl::Encode(&encoder, &string, encoder.Alloc(sizeof(fidl_string_t)));

  zx_status_t status = std::numeric_limits<zx_status_t>::max();
  proxy_ctrl.reader().set_error_handler([&status](zx_status_t s) { status = s; });
  proxy_ctrl.Send(&unbounded_nonnullable_string_message_type, encoder.GetMessage(), nullptr);
  EXPECT_EQ(std::numeric_limits<zx_status_t>::max(), status);
  EXPECT_EQ(0, callback_count);
  loop.RunUntilIdle();
  EXPECT_EQ(1, callback_count);
}

TEST(StubController, Response) {
  zx::channel h1, h2;
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &h1, &h2));

  fidl::test::AsyncLoopForTest loop;

  StubController stub_ctrl;
  EXPECT_EQ(ZX_OK, stub_ctrl.reader().Bind(std::move(h1)));

  ProxyController proxy_ctrl;
  EXPECT_EQ(ZX_OK, proxy_ctrl.reader().Bind(std::move(h2)));

  CallbackStub stub;

  int callback_count = 0;
  stub.callback = [&callback_count](HLCPPIncomingMessage message, PendingResponse response) {
    ++callback_count;
    EXPECT_EQ(5u, message.ordinal());
    EXPECT_TRUE(response.needs_response());
    MessageEncoder encoder(42u, fidl::MessageDynamicFlags::kStrictMethod);
    StringPtr string("welcome!");
    fidl::Encode(&encoder, &string, encoder.Alloc(sizeof(fidl_string_t)));
    EXPECT_EQ(ZX_OK,
              response.Send(&unbounded_nonnullable_string_message_type, encoder.GetMessage()));
    return ZX_OK;
  };

  stub_ctrl.set_stub(&stub);

  MessageEncoder encoder(5u, fidl::MessageDynamicFlags::kStrictMethod);
  StringPtr string("hello!");
  fidl::Encode(&encoder, &string, encoder.Alloc(sizeof(fidl_string_t)));

  int response_count = 0;
  auto handler = std::make_unique<SingleUseMessageHandler>(
      [&response_count](HLCPPIncomingMessage&& message) {
        ++response_count;
        EXPECT_EQ(42u, message.ordinal());
        return ZX_OK;
      },
      &unbounded_nonnullable_string_message_type);

  zx_status_t status = std::numeric_limits<zx_status_t>::max();
  proxy_ctrl.reader().set_error_handler([&status](zx_status_t s) { status = s; });
  proxy_ctrl.Send(&unbounded_nonnullable_string_message_type, encoder.GetMessage(),
                  std::move(handler));
  EXPECT_EQ(std::numeric_limits<zx_status_t>::max(), status);
  EXPECT_EQ(0, callback_count);
  EXPECT_EQ(0, response_count);
  loop.RunUntilIdle();
  EXPECT_EQ(1, callback_count);
  EXPECT_EQ(1, response_count);
}

TEST(StubController, ResponseAfterUnbind) {
  zx::channel h1, h2;
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &h1, &h2));

  fidl::test::AsyncLoopForTest loop;

  StubController stub_ctrl;
  EXPECT_EQ(ZX_OK, stub_ctrl.reader().Bind(std::move(h1)));

  ProxyController proxy_ctrl;
  EXPECT_EQ(ZX_OK, proxy_ctrl.reader().Bind(std::move(h2)));

  CallbackStub stub;

  int callback_count = 0;
  stub.callback = [&callback_count, &stub_ctrl](HLCPPIncomingMessage message,
                                                PendingResponse response) {
    ++callback_count;

    stub_ctrl.reader().Unbind();

    EXPECT_EQ(5u, message.ordinal());
    EXPECT_TRUE(response.needs_response());
    MessageEncoder encoder(42u, fidl::MessageDynamicFlags::kStrictMethod);
    StringPtr string("welcome!");
    fidl::Encode(&encoder, &string, encoder.Alloc(sizeof(fidl_string_t)));
    EXPECT_EQ(ZX_ERR_BAD_STATE,
              response.Send(&unbounded_nonnullable_string_message_type, encoder.GetMessage()));
    return ZX_OK;
  };

  stub_ctrl.set_stub(&stub);

  MessageEncoder encoder(5u, fidl::MessageDynamicFlags::kStrictMethod);
  StringPtr string("hello!");
  fidl::Encode(&encoder, &string, encoder.Alloc(sizeof(fidl_string_t)));

  int response_count = 0;
  auto handler = std::make_unique<SingleUseMessageHandler>(
      [&response_count](HLCPPIncomingMessage&& message) {
        ++response_count;
        return ZX_OK;
      },
      &unbounded_nonnullable_string_message_type);

  zx_status_t status = std::numeric_limits<zx_status_t>::max();
  proxy_ctrl.reader().set_error_handler([&status](zx_status_t s) { status = s; });
  proxy_ctrl.Send(&unbounded_nonnullable_string_message_type, encoder.GetMessage(),
                  std::move(handler));
  EXPECT_EQ(std::numeric_limits<zx_status_t>::max(), status);
  EXPECT_EQ(0, callback_count);
  EXPECT_EQ(0, response_count);
  loop.RunUntilIdle();
  EXPECT_EQ(1, callback_count);
  EXPECT_EQ(0, response_count);
}

TEST(StubController, ResponseAfterDestroy) {
  zx::channel h1, h2;
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &h1, &h2));

  fidl::test::AsyncLoopForTest loop;

  auto stub_ctrl = std::make_unique<StubController>();
  EXPECT_EQ(ZX_OK, stub_ctrl->reader().Bind(std::move(h1)));

  ProxyController proxy_ctrl;
  EXPECT_EQ(ZX_OK, proxy_ctrl.reader().Bind(std::move(h2)));

  CallbackStub stub;

  int callback_count = 0;
  stub.callback = [&callback_count, &stub_ctrl](HLCPPIncomingMessage message,
                                                PendingResponse response) {
    ++callback_count;

    stub_ctrl.reset();

    EXPECT_EQ(5u, message.ordinal());
    EXPECT_TRUE(response.needs_response());
    MessageEncoder encoder(42u, fidl::MessageDynamicFlags::kStrictMethod);
    StringPtr string("welcome!");
    fidl::Encode(&encoder, &string, encoder.Alloc(sizeof(fidl_string_t)));
    EXPECT_EQ(ZX_ERR_BAD_STATE,
              response.Send(&unbounded_nonnullable_string_message_type, encoder.GetMessage()));
    return ZX_OK;
  };

  stub_ctrl->set_stub(&stub);

  MessageEncoder encoder(5u, fidl::MessageDynamicFlags::kStrictMethod);
  StringPtr string("hello!");
  fidl::Encode(&encoder, &string, encoder.Alloc(sizeof(fidl_string_t)));

  int response_count = 0;
  auto handler = std::make_unique<SingleUseMessageHandler>(
      [&response_count](HLCPPIncomingMessage&& message) {
        ++response_count;
        return ZX_OK;
      },
      &unbounded_nonnullable_string_message_type);

  zx_status_t status = std::numeric_limits<zx_status_t>::max();
  proxy_ctrl.reader().set_error_handler([&status](zx_status_t s) { status = s; });
  proxy_ctrl.Send(&unbounded_nonnullable_string_message_type, encoder.GetMessage(),
                  std::move(handler));
  EXPECT_EQ(std::numeric_limits<zx_status_t>::max(), status);
  EXPECT_EQ(0, callback_count);
  EXPECT_EQ(0, response_count);
  loop.RunUntilIdle();
  EXPECT_EQ(1, callback_count);
  EXPECT_EQ(0, response_count);
}

TEST(StubController, BadResponse) {
  zx::channel h1, h2;
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &h1, &h2));

  fidl::test::AsyncLoopForTest loop;

  StubController stub_ctrl;
  EXPECT_EQ(ZX_OK, stub_ctrl.reader().Bind(std::move(h1)));

  int error_count = 0;
  stub_ctrl.reader().set_error_handler([&error_count](zx_status_t status) {
    EXPECT_EQ(ZX_ERR_PEER_CLOSED, status);
    ++error_count;
  });

  ProxyController proxy_ctrl;
  EXPECT_EQ(ZX_OK, proxy_ctrl.reader().Bind(std::move(h2)));

  CallbackStub stub;

  int callback_count = 0;
  stub.callback = [&callback_count](HLCPPIncomingMessage message, PendingResponse response) {
    ++callback_count;
    EXPECT_EQ(5u, message.ordinal());
    EXPECT_TRUE(response.needs_response());
    MessageEncoder encoder(42u, fidl::MessageDynamicFlags::kStrictMethod);
    // Bad message format.
    EXPECT_EQ(ZX_ERR_BUFFER_TOO_SMALL,
              response.Send(&unbounded_nonnullable_string_message_type, encoder.GetMessage()));
    return ZX_OK;
  };

  stub_ctrl.set_stub(&stub);

  MessageEncoder encoder(5u, fidl::MessageDynamicFlags::kStrictMethod);
  StringPtr string("hello!");
  fidl::Encode(&encoder, &string, encoder.Alloc(sizeof(fidl_string_t)));

  int response_count = 0;
  auto handler = std::make_unique<SingleUseMessageHandler>(
      [&response_count](HLCPPIncomingMessage&& message) {
        ++response_count;
        return ZX_OK;
      },
      &unbounded_nonnullable_string_message_type);

  zx_status_t status = std::numeric_limits<zx_status_t>::max();
  proxy_ctrl.reader().set_error_handler([&status](zx_status_t s) { status = s; });
  proxy_ctrl.Send(&unbounded_nonnullable_string_message_type, encoder.GetMessage(),
                  std::move(handler));
  EXPECT_EQ(std::numeric_limits<zx_status_t>::max(), status);
  EXPECT_EQ(0, callback_count);
  EXPECT_EQ(0, response_count);
  EXPECT_EQ(0, error_count);
  loop.RunUntilIdle();
  EXPECT_EQ(1, callback_count);
  EXPECT_EQ(0, response_count);
  EXPECT_EQ(0, error_count);
}

TEST(StubController, BadMessage) {
  zx::channel h1, h2;
  EXPECT_EQ(ZX_OK, zx::channel::create(0, &h1, &h2));

  fidl::test::AsyncLoopForTest loop;

  StubController stub_ctrl;
  EXPECT_EQ(ZX_OK, stub_ctrl.reader().Bind(std::move(h1)));

  int error_count = 0;
  stub_ctrl.reader().set_error_handler([&error_count](zx_status_t status) {
    EXPECT_EQ(ZX_ERR_INVALID_ARGS, status);
    ++error_count;
  });

  CallbackStub stub;

  int callback_count = 0;
  stub.callback = [&callback_count](HLCPPIncomingMessage message, PendingResponse response) {
    ++callback_count;
    return ZX_OK;
  };

  stub_ctrl.set_stub(&stub);

  EXPECT_EQ(ZX_OK, h2.write(0, "a", 1, nullptr, 0));

  EXPECT_EQ(0, callback_count);
  EXPECT_EQ(0, error_count);
  loop.RunUntilIdle();
  EXPECT_EQ(0, callback_count);
  EXPECT_EQ(1, error_count);
}

}  // namespace
}  // namespace internal
}  // namespace fidl
