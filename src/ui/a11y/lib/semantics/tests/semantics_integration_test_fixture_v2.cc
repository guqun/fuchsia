// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(fxb.dev/98392): Remove non-cts deps, and use <> for sdk libraries.
#include "src/ui/a11y/lib/semantics/tests/semantics_integration_test_fixture_v2.h"

#include <fuchsia/cobalt/cpp/fidl.h>
#include <fuchsia/component/cpp/fidl.h>
#include <fuchsia/fonts/cpp/fidl.h>
#include <fuchsia/hardware/display/cpp/fidl.h>
#include <fuchsia/intl/cpp/fidl.h>
#include <fuchsia/kernel/cpp/fidl.h>
#include <fuchsia/memorypressure/cpp/fidl.h>
#include <fuchsia/net/interfaces/cpp/fidl.h>
#include <fuchsia/posix/socket/cpp/fidl.h>
#include <fuchsia/scheduler/cpp/fidl.h>
#include <fuchsia/sys/cpp/fidl.h>
#include <fuchsia/tracing/provider/cpp/fidl.h>
#include <fuchsia/ui/app/cpp/fidl.h>
#include <fuchsia/ui/input/cpp/fidl.h>
#include <fuchsia/ui/pointerinjector/cpp/fidl.h>
#include <fuchsia/ui/policy/cpp/fidl.h>
#include <fuchsia/ui/scenic/cpp/fidl.h>
#include <fuchsia/vulkan/loader/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/view_ref_pair.h>
#include <lib/ui/scenic/cpp/view_token_pair.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <cmath>

#include "fuchsia/sysmem/cpp/fidl.h"
#include "lib/sys/component/cpp/testing/realm_builder_types.h"
#include "src/lib/fsl/handles/object_info.h"

namespace accessibility_test {

namespace {

using ScenicEvent = fuchsia::ui::scenic::Event;
using GfxEvent = fuchsia::ui::gfx::Event;

using component_testing::ParentRef;
using component_testing::Protocol;
using component_testing::Route;
using fuchsia::accessibility::semantics::Node;

bool CompareFloat(float f0, float f1, float epsilon = 0.01f) {
  return std::abs(f0 - f1) <= epsilon;
}

}  // namespace
void SemanticsManagerProxy::Start(std::unique_ptr<LocalComponentHandles> mock_handles) {
  FX_CHECK(mock_handles->outgoing()->AddPublicService(
               fidl::InterfaceRequestHandler<fuchsia::accessibility::semantics::SemanticsManager>(
                   [this](auto request) {
                     bindings_.AddBinding(this, std::move(request), dispatcher_);
                   })) == ZX_OK);
  mock_handles_.emplace_back(std::move(mock_handles));
}

void SemanticsManagerProxy::RegisterViewForSemantics(
    fuchsia::ui::views::ViewRef view_ref,
    fidl::InterfaceHandle<fuchsia::accessibility::semantics::SemanticListener> handle,
    fidl::InterfaceRequest<fuchsia::accessibility::semantics::SemanticTree> semantic_tree_request) {
  semantics_manager_->RegisterViewForSemantics(std::move(view_ref), std::move(handle),
                                               std::move(semantic_tree_request));
}

void SemanticsIntegrationTestV2::SetUp() {
  FX_LOGS(INFO) << "Setting up test fixture";

  // Initialize ui test manager.
  ui_testing::UITestManager::Config config;
  config.scene_owner = GetParam();
  config.ui_to_client_services = {fuchsia::ui::scenic::Scenic::Name_};
  ui_test_manager_ = std::make_unique<ui_testing::UITestManager>(config);

  // Initialize ui test realm.
  BuildRealm();
}

void SemanticsIntegrationTestV2::BuildRealm() {
  FX_LOGS(INFO) << "Building realm";
  realm_ = std::make_unique<Realm>(ui_test_manager_->AddSubrealm());

  view_manager_ = std::make_unique<a11y::ViewManager>(
      std::make_unique<a11y::SemanticTreeServiceFactory>(),
      std::make_unique<MockViewSemanticsFactory>(), std::make_unique<MockAnnotationViewFactory>(),
      std::make_unique<MockViewInjectorFactory>(),
      std::make_unique<a11y::A11ySemanticsEventManager>(),
      std::make_unique<MockAccessibilityView>(), context_.get(), context_->outgoing()->debug_dir());

  semantics_manager_proxy_ =
      std::make_unique<SemanticsManagerProxy>(view_manager_.get(), dispatcher());
  realm_->AddLocalChild(SemanticsIntegrationTestV2::kSemanticsManager, semantics_manager_proxy());

  // Let subclass configure Realm beyond this base setup.
  ConfigureRealm();

  ui_test_manager_->BuildRealm();

  realm_exposed_services_ = ui_test_manager_->TakeExposedServicesDirectory();
}

void SemanticsIntegrationTestV2::SetupScene() {
  ui_test_manager_->InitializeScene();
  RunLoopUntil([this]() { return ui_test_manager_->ClientViewIsRendering(); });

  auto view_ref_koid = ui_test_manager_->ClientViewRefKoid();
  ASSERT_TRUE(view_ref_koid.has_value());
  view_ref_koid_ = *view_ref_koid;
}

const Node* SemanticsIntegrationTestV2::FindNodeWithLabel(const Node* node, zx_koid_t view_ref_koid,
                                                          std::string label) {
  if (!node) {
    return nullptr;
  }

  if (node->has_attributes() && node->attributes().has_label() &&
      node->attributes().label() == label) {
    return node;
  }

  if (!node->has_child_ids()) {
    return nullptr;
  }
  for (const auto& child_id : node->child_ids()) {
    const auto* child = view_manager()->GetSemanticNode(view_ref_koid, child_id);
    FX_DCHECK(child);
    auto result = FindNodeWithLabel(child, view_ref_koid, label);
    if (result != nullptr) {
      return result;
    }
  }

  return nullptr;
}

a11y::SemanticTransform SemanticsIntegrationTestV2::GetTransformForNode(zx_koid_t view_ref_koid,
                                                                        uint32_t node_id) {
  std::vector<const Node*> path;
  // Perform a DFS to find the path to the target node
  std::function<bool(const Node*)> traverse = [&](const Node* node) {
    if (node->node_id() == node_id) {
      path.push_back(node);
      return true;
    }
    if (!node->has_child_ids()) {
      return false;
    }
    for (const auto& child_id : node->child_ids()) {
      const auto* child = view_manager()->GetSemanticNode(view_ref_koid, child_id);
      FX_DCHECK(child);
      if (traverse(child)) {
        path.push_back(node);
        return true;
      }
    }
    return false;
  };

  auto root = view_manager()->GetSemanticNode(view_ref_koid, 0u);
  traverse(root);

  a11y::SemanticTransform transform;
  for (auto& node : path) {
    if (node->has_transform()) {
      transform.ChainLocalTransform(node->transform());
    }
  }

  return transform;
}

std::optional<uint32_t> SemanticsIntegrationTestV2::HitTest(zx_koid_t view_ref_koid,
                                                            fuchsia::math::PointF target) {
  std::optional<fuchsia::accessibility::semantics::Hit> target_hit;
  FX_LOGS(INFO) << "target is: " << target.x << ":" << target.y;
  auto hit_callback = [&target_hit](fuchsia::accessibility::semantics::Hit hit) {
    target_hit = std::move(hit);
  };

  view_manager()->ExecuteHitTesting(view_ref_koid, target, hit_callback);

  RunLoopUntil([&target_hit] { return target_hit.has_value(); });
  if (!target_hit.has_value() || !target_hit->has_node_id()) {
    return std::nullopt;
  }
  return target_hit->node_id();
}

fuchsia::math::PointF
SemanticsIntegrationTestV2::CalculateCenterOfSemanticNodeBoundingBoxCoordinate(
    zx_koid_t view_ref_koid, const fuchsia::accessibility::semantics::Node* node) {
  // Semantic trees may have transforms in each node.  That transform defines the spatial relation
  // between coordinates in the node's space to coordinates in it's parent's space.  This is done
  // to enable semantic providers to avoid recomputing location information on every child node
  // when a parent node (or the entire view) undergoes a spatial change.

  // Get the transform from the node's local space to the view's local space.
  auto transform = view_manager()->GetNodeToRootTransform(view_ref_koid, node->node_id());
  FX_DCHECK(transform) << "Could not compute a transform for the semantic node: " << view_ref_koid
                       << ":" << node->node_id();

  const auto node_bounding_box = node->location();
  const auto node_bounding_box_center_x = (node_bounding_box.min.x + node_bounding_box.max.x) / 2.f;
  const auto node_bounding_box_center_y = (node_bounding_box.min.y + node_bounding_box.max.y) / 2.f;
  const fuchsia::ui::gfx::vec3 node_bounding_box_center_local = {node_bounding_box_center_x,
                                                                 node_bounding_box_center_y, 0.f};

  const fuchsia::ui::gfx::vec3 node_bounding_box_center_root =
      transform->Apply(node_bounding_box_center_local);

  return {node_bounding_box_center_root.x, node_bounding_box_center_root.y};
}

bool SemanticsIntegrationTestV2::PerformAccessibilityAction(
    zx_koid_t view_ref_koid, uint32_t node_id, fuchsia::accessibility::semantics::Action action) {
  std::optional<bool> callback_handled;
  auto callback = [&callback_handled](bool handled) { callback_handled = handled; };
  view_manager()->PerformAccessibilityAction(view_ref_koid, node_id, action, callback);

  RunLoopUntil([&callback_handled] { return callback_handled.has_value(); });
  return *callback_handled;
}

void SemanticsIntegrationTestV2::WaitForScaleFactor() {
  RunLoopUntil([this] {
    auto scale_factor = ui_test_manager_->ClientViewScaleFactor();
    auto node = view_manager()->GetSemanticNode(view_ref_koid(), 0u);
    if (!node) {
      return false;
    }

    // TODO(fxb.dev/93943): Remove accommodation for transform field.
    return (node->has_transform() && CompareFloat(node->transform().matrix[0], 1 / scale_factor)) ||
           (node->has_node_to_container_transform() &&
            CompareFloat(node->node_to_container_transform().matrix[0], 1 / scale_factor));
  });
}

}  // namespace accessibility_test
