// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/cobalt/cpp/fidl.h>
#include <fuchsia/component/cpp/fidl.h>
#include <fuchsia/memorypressure/cpp/fidl.h>
#include <fuchsia/posix/socket/cpp/fidl.h>
#include <fuchsia/scheduler/cpp/fidl.h>
#include <fuchsia/session/scene/cpp/fidl.h>
#include <fuchsia/sys/cpp/fidl.h>
#include <fuchsia/tracing/provider/cpp/fidl.h>
#include <fuchsia/ui/app/cpp/fidl.h>
#include <fuchsia/ui/scenic/cpp/fidl.h>
#include <fuchsia/vulkan/loader/cpp/fidl.h>
#include <fuchsia/web/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/gtest/real_loop_fixture.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>
#include <zircon/status.h>
#include <zircon/types.h>
#include <zircon/utc.h>

#include <cstddef>
#include <cstdint>
#include <iostream>
#include <memory>
#include <type_traits>
#include <utility>
#include <vector>

#include <gtest/gtest.h>
#include <test/inputsynthesis/cpp/fidl.h>
#include <test/mouse/cpp/fidl.h>

#include "src/ui/testing/ui_test_manager/ui_test_manager.h"

namespace {

// Types imported for the realm_builder library.
using component_testing::ChildRef;
using component_testing::LocalComponent;
using component_testing::LocalComponentHandles;
using component_testing::ParentRef;
using component_testing::Protocol;
using component_testing::Realm;
using component_testing::Route;

// Alias for Component child name as provided to Realm Builder.
using ChildName = std::string;

// Alias for Component Legacy URL as provided to Realm Builder.
using LegacyUrl = std::string;

// Max timeout in failure cases.
// Set this as low as you can that still works across all test platforms.
constexpr zx::duration kTimeout = zx::min(5);

// Combines all vectors in `vecs` into one.
template <typename T>
std::vector<T> merge(std::initializer_list<std::vector<T>> vecs) {
  std::vector<T> result;
  for (auto v : vecs) {
    result.insert(result.end(), v.begin(), v.end());
  }
  return result;
}

// `ResponseListener` is a local test protocol that our test Flutter app uses to let us know
// what position and button press state the mouse cursor has.
class ResponseListenerServer : public test::mouse::ResponseListener, public LocalComponent {
 public:
  explicit ResponseListenerServer(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  // |test::mouse::ResponseListener|
  void Respond(test::mouse::PointerData pointer_data) override {
    FX_CHECK(respond_callback_) << "Expected callback to be set for test.mouse.Respond().";
    respond_callback_(std::move(pointer_data));
  }

  // |MockComponent::Start|
  // When the component framework requests for this component to start, this
  // method will be invoked by the realm_builder library.
  void Start(std::unique_ptr<LocalComponentHandles> mock_handles) override {
    // When this component starts, add a binding to the test.mouse.ResponseListener
    // protocol to this component's outgoing directory.
    FX_CHECK(mock_handles->outgoing()->AddPublicService(
                 fidl::InterfaceRequestHandler<test::mouse::ResponseListener>([this](auto request) {
                   bindings_.AddBinding(this, std::move(request), dispatcher_);
                 })) == ZX_OK);
    mock_handles_.emplace_back(std::move(mock_handles));
  }

  void SetRespondCallback(fit::function<void(test::mouse::PointerData)> callback) {
    respond_callback_ = std::move(callback);
  }

 private:
  // Not owned.
  async_dispatcher_t* dispatcher_ = nullptr;
  fidl::BindingSet<test::mouse::ResponseListener> bindings_;
  std::vector<std::unique_ptr<LocalComponentHandles>> mock_handles_;
  fit::function<void(test::mouse::PointerData)> respond_callback_;
};

constexpr auto kResponseListener = "response_listener";

class MouseInputBase : public gtest::RealLoopFixture {
 protected:
  MouseInputBase() : response_listener_(std::make_unique<ResponseListenerServer>(dispatcher())) {}

  sys::ServiceDirectory* realm_exposed_services() { return realm_exposed_services_.get(); }

  ResponseListenerServer* response_listener() { return response_listener_.get(); }

  void SetUp() override {
    // Post a "just in case" quit task, if the test hangs.
    async::PostDelayedTask(
        dispatcher(),
        [] { FX_LOGS(FATAL) << "\n\n>> Test did not complete in time, terminating.  <<\n\n"; },
        kTimeout);

    ui_testing::UITestManager::Config config;
    config.use_flatland = true;
    config.scene_owner = ui_testing::UITestManager::SceneOwnerType::SCENE_MANAGER;
    config.use_input = true;
    config.accessibility_owner = ui_testing::UITestManager::AccessibilityOwnerType::FAKE;
    config.ui_to_client_services = {
        fuchsia::ui::scenic::Scenic::Name_, fuchsia::ui::composition::Flatland::Name_,
        fuchsia::ui::composition::Allocator::Name_, fuchsia::ui::input::ImeService::Name_,
        fuchsia::ui::input3::Keyboard::Name_};
    ui_test_manager_ = std::make_unique<ui_testing::UITestManager>(std::move(config));

    AssembleRealm(this->GetTestComponents(), this->GetTestRoutes());
  }

  // Subclass should implement this method to add components to the test realm
  // next to the base ones added.
  virtual std::vector<std::pair<ChildName, LegacyUrl>> GetTestComponents() { return {}; }

  // Subclass should implement this method to add capability routes to the test
  // realm next to the base ones added.
  virtual std::vector<Route> GetTestRoutes() { return {}; }

  // Helper method for checking the test.mouse.ResponseListener response from the client app.
  void SetResponseExpectations(uint32_t expected_x, uint32_t expected_y,
                               zx::basic_time<ZX_CLOCK_MONOTONIC>& input_injection_time,
                               std::string component_name, bool& injection_complete) {
    response_listener()->SetRespondCallback([expected_x, expected_y, component_name,
                                             &input_injection_time, &injection_complete](
                                                test::mouse::PointerData pointer_data) {
      FX_LOGS(INFO) << "Client received tap at (" << pointer_data.local_x() << ", "
                    << pointer_data.local_y() << ").";
      FX_LOGS(INFO) << "Expected tap is at approximately (" << expected_x << ", " << expected_y
                    << ").";

      zx::duration elapsed_time =
          zx::basic_time<ZX_CLOCK_MONOTONIC>(pointer_data.time_received()) - input_injection_time;
      EXPECT_TRUE(elapsed_time.get() > 0 && elapsed_time.get() != ZX_TIME_INFINITE);
      FX_LOGS(INFO) << "Input Injection Time (ns): " << input_injection_time.get();
      FX_LOGS(INFO) << "Client Received Time (ns): " << pointer_data.time_received();
      FX_LOGS(INFO) << "Elapsed Time (ns): " << elapsed_time.to_nsecs();

      // Allow for minor rounding differences in coordinates.
      EXPECT_NEAR(pointer_data.local_x(), expected_x, 1);
      EXPECT_NEAR(pointer_data.local_y(), expected_y, 1);
      EXPECT_EQ(pointer_data.component_name(), component_name);

      injection_complete = true;
    });
  }

  void AssembleRealm(const std::vector<std::pair<ChildName, LegacyUrl>>& components,
                     const std::vector<Route>& routes) {
    FX_LOGS(INFO) << "Building realm";
    realm_ = std::make_unique<Realm>(ui_test_manager_->AddSubrealm());

    // Key part of service setup: have this test component vend the
    // |ResponseListener| service in the constructed realm.
    realm_->AddLocalChild(kResponseListener, response_listener());

    // Add components specific for this test case to the realm.
    for (const auto& [name, component] : components) {
      realm_->AddChild(name, component);
    }

    // Add the necessary routing for each of the extra components added above.
    for (const auto& route : routes) {
      realm_->AddRoute(route);
    }

    // Finally, build the realm using the provided components and routes.
    ui_test_manager_->BuildRealm();
    realm_exposed_services_ = ui_test_manager_->TakeExposedServicesDirectory();
  }

  void LaunchClient() {
    // Initialize scene, and attach client view.
    ui_test_manager_->InitializeScene();
    FX_LOGS(INFO) << "Wait for client view to render";
    RunLoopUntil([this]() { return ui_test_manager_->ClientViewIsRendering(); });
  }

  std::unique_ptr<ui_testing::UITestManager> ui_test_manager_;
  std::unique_ptr<sys::ServiceDirectory> realm_exposed_services_;
  std::unique_ptr<Realm> realm_;

  std::unique_ptr<ResponseListenerServer> response_listener_;
};

class FlutterInputTest : public MouseInputBase {
 protected:
  std::vector<std::pair<ChildName, LegacyUrl>> GetTestComponents() override {
    return {
        std::make_pair(kMouseInputFlutter, kMouseInputFlutterUrl),
        std::make_pair(kMemoryPressureProvider, kMemoryPressureProviderUrl),
        std::make_pair(kNetstack, kNetstackUrl),
    };
  }

  std::vector<Route> GetTestRoutes() override {
    return merge({GetFlutterRoutes(ChildRef{kMouseInputFlutter}),
                  {
                      {.capabilities = {Protocol{fuchsia::ui::app::ViewProvider::Name_}},
                       .source = ChildRef{kMouseInputFlutter},
                       .targets = {ParentRef()}},
                  }});
  }

  // Routes needed to setup Flutter client.
  static std::vector<Route> GetFlutterRoutes(ChildRef target) {
    return {{.capabilities =
                 {
                     Protocol{test::mouse::ResponseListener::Name_},
                 },
             .source = ChildRef{kResponseListener},
             .targets = {target}},
            {.capabilities =
                 {
                     Protocol{fuchsia::ui::composition::Allocator::Name_},
                     Protocol{fuchsia::ui::composition::Flatland::Name_},
                     Protocol{fuchsia::ui::scenic::Scenic::Name_},
                     // Redirect logging output for the test realm to
                     // the host console output.
                     Protocol{fuchsia::logger::LogSink::Name_},
                     Protocol{fuchsia::scheduler::ProfileProvider::Name_},
                     Protocol{fuchsia::sysmem::Allocator::Name_},
                     Protocol{fuchsia::tracing::provider::Registry::Name_},
                     Protocol{fuchsia::vulkan::loader::Loader::Name_},
                 },
             .source = ParentRef(),
             .targets = {target}},
            {.capabilities = {Protocol{fuchsia::memorypressure::Provider::Name_}},
             .source = ChildRef{kMemoryPressureProvider},
             .targets = {target}},
            {.capabilities = {Protocol{fuchsia::posix::socket::Provider::Name_}},
             .source = ChildRef{kNetstack},
             .targets = {target}}};
  }

  static constexpr auto kMouseInputFlutter = "mouse-input-flutter";
  static constexpr auto kMouseInputFlutterUrl = "#meta/mouse-input-flutter-realm.cm";

 private:
  static constexpr auto kMemoryPressureProvider = "memory_pressure_provider";
  static constexpr auto kMemoryPressureProviderUrl = "#meta/memory_monitor.cm";

  static constexpr auto kNetstack = "netstack";
  static constexpr auto kNetstackUrl = "#meta/netstack.cm";
};

TEST_F(FlutterInputTest, FlutterMouseMove) {
  // Use `ZX_CLOCK_MONOTONIC` to avoid complications due to wall-clock time changes.
  zx::basic_time<ZX_CLOCK_MONOTONIC> input_injection_time(0);

  bool initialization_complete = false;
  SetResponseExpectations(/*expected_x=*/0,
                          /*expected_y=*/0, input_injection_time,
                          /*component_name=*/"mouse-input-flutter", initialization_complete);

  LaunchClient();

  FX_LOGS(INFO) << "Wait for the initial mouse state";
  RunLoopUntil([&initialization_complete] { return initialization_complete; });

  // TODO: Inject input.
  auto input_synthesis = realm_exposed_services()->Connect<test::inputsynthesis::Mouse>();
}

}  // namespace
