// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "simple-display.h"

#include <assert.h>
#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <fidl/fuchsia.sysmem/cpp/wire.h>
#include <fuchsia/hardware/display/controller/cpp/banjo.h>
#include <fuchsia/hardware/pci/c/banjo.h>
#include <lib/async-loop/default.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <lib/device-protocol/pci.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <zircon/pixelformat.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>
#include <zircon/types.h>

#include <atomic>
#include <memory>
#include <utility>

#include <fbl/alloc_checker.h>

#include "fbl/auto_lock.h"

namespace {

static constexpr uint64_t kDisplayId = 1;

static constexpr uint64_t kImageHandle = 0xdecafc0ffee;

// Just guess that it's 30fps
static constexpr uint64_t kRefreshRateHz = 30;

static constexpr auto kVSyncInterval = zx::usec(1000000 / kRefreshRateHz);

fuchsia_sysmem2::wire::HeapProperties GetHeapProperties(fidl::AnyArena& arena) {
  fuchsia_sysmem2::wire::CoherencyDomainSupport coherency_domain_support(arena);
  coherency_domain_support.set_cpu_supported(false)
      .set_ram_supported(true)
      .set_inaccessible_supported(false);
  fuchsia_sysmem2::wire::HeapProperties heap_properties(arena);
  heap_properties.set_coherency_domain_support(arena, std::move(coherency_domain_support))
      .set_need_clear(false);
  return heap_properties;
}

void OnHeapServerClose(fidl::UnbindInfo info, zx::channel channel) {
  if (info.is_dispatcher_shutdown()) {
    // Pending wait is canceled because the display device that the heap belongs
    // to has been destroyed.
    zxlogf(INFO, "Simple display destroyed: status: %s", info.status_string());
    return;
  }

  if (info.is_peer_closed()) {
    zxlogf(INFO, "Client closed heap connection");
    return;
  }

  zxlogf(ERROR, "Channel internal error: status: %s", info.FormatDescription().c_str());
}

}  // namespace

// implement display controller protocol:

void SimpleDisplay::DisplayControllerImplSetDisplayControllerInterface(
    const display_controller_interface_protocol_t* intf) {
  intf_ = ddk::DisplayControllerInterfaceProtocolClient(intf);

  added_display_args_t args = {};
  args.display_id = kDisplayId;
  args.edid_present = false;
  args.panel.params.height = height_;
  args.panel.params.width = width_;
  args.panel.params.refresh_rate_e2 = kRefreshRateHz * 100;
  args.pixel_format_list = &format_;
  args.pixel_format_count = 1;

  intf_.OnDisplaysChanged(&args, 1, nullptr, 0, nullptr, 0, nullptr);
}

zx_status_t SimpleDisplay::DisplayControllerImplImportImage(image_t* image,
                                                            zx_unowned_handle_t handle,
                                                            uint32_t index) {
  auto result = fidl::WireCall(fidl::UnownedClientEnd<fuchsia_sysmem::BufferCollection>(
                                   zx::unowned_channel(handle)))
                    ->WaitForBuffersAllocated();
  if (!result.ok()) {
    zxlogf(ERROR, "failed to wait for buffers allocated, %s", result.FormatDescription().c_str());
    return result.status();
  }
  if (result.value_NEW().status != ZX_OK) {
    return result.value_NEW().status;
  }

  fuchsia_sysmem::wire::BufferCollectionInfo2& collection_info =
      result.value_NEW().buffer_collection_info;

  if (!collection_info.settings.has_image_format_constraints) {
    zxlogf(ERROR, "no image format constraints");
    return ZX_ERR_INVALID_ARGS;
  }
  if (index > 0) {
    zxlogf(ERROR, "invalid index %d, greater than 0", index);
    return ZX_ERR_OUT_OF_RANGE;
  }

  zx::vmo vmo = std::move(collection_info.buffers[0].vmo);

  zx_info_handle_basic_t import_info;
  size_t actual, avail;
  zx_status_t status =
      vmo.get_info(ZX_INFO_HANDLE_BASIC, &import_info, sizeof(import_info), &actual, &avail);
  if (status != ZX_OK) {
    return status;
  }
  if (import_info.koid != framebuffer_koid_) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (image->width != width_ || image->height != height_ || image->pixel_format != format_) {
    return ZX_ERR_INVALID_ARGS;
  }
  image->handle = kImageHandle;
  return ZX_OK;
}

void SimpleDisplay::DisplayControllerImplReleaseImage(image_t* image) {
  // noop
}

uint32_t SimpleDisplay::DisplayControllerImplCheckConfiguration(
    const display_config_t** display_configs, size_t display_count, uint32_t** layer_cfg_results,
    size_t* layer_cfg_result_count) {
  if (display_count != 1) {
    ZX_DEBUG_ASSERT(display_count == 0);
    return CONFIG_DISPLAY_OK;
  }
  ZX_DEBUG_ASSERT(display_configs[0]->display_id == kDisplayId);
  bool success;
  if (display_configs[0]->layer_count != 1) {
    success = false;
  } else {
    primary_layer_t* layer = &display_configs[0]->layer_list[0]->cfg.primary;
    frame_t frame = {
        .x_pos = 0,
        .y_pos = 0,
        .width = width_,
        .height = height_,
    };
    success = display_configs[0]->layer_list[0]->type == LAYER_TYPE_PRIMARY &&
              layer->transform_mode == FRAME_TRANSFORM_IDENTITY && layer->image.width == width_ &&
              layer->image.height == height_ &&
              memcmp(&layer->dest_frame, &frame, sizeof(frame_t)) == 0 &&
              memcmp(&layer->src_frame, &frame, sizeof(frame_t)) == 0 &&
              display_configs[0]->cc_flags == 0 && layer->alpha_mode == ALPHA_DISABLE;
  }
  if (!success) {
    layer_cfg_results[0][0] = CLIENT_MERGE_BASE;
    for (unsigned i = 1; i < display_configs[0]->layer_count; i++) {
      layer_cfg_results[0][i] = CLIENT_MERGE_SRC;
    }
    layer_cfg_result_count[0] = display_configs[0]->layer_count;
  }
  return CONFIG_DISPLAY_OK;
}

void SimpleDisplay::DisplayControllerImplApplyConfiguration(const display_config_t** display_config,
                                                            size_t display_count,
                                                            const config_stamp_t* config_stamp) {
  has_image_ = display_count != 0 && display_config[0]->layer_count != 0;
  {
    fbl::AutoLock lock(&mtx_);
    config_stamp_ = *config_stamp;
  }
}

// TODO(fxb/81875): Remove support when no longer used.
uint32_t SimpleDisplay::DisplayControllerImplComputeLinearStride(uint32_t width,
                                                                 zx_pixel_format_t format) {
  return (width == width_ && format == format_) ? stride_ : 0;
}

// TODO(fxb/81875): Remove support when no longer used.
zx_status_t SimpleDisplay::DisplayControllerImplAllocateVmo(uint64_t size, zx::vmo* vmo_out) {
  zx_info_handle_count handle_count;
  size_t actual, avail;
  zx_status_t status = framebuffer_mmio_.get_vmo()->get_info(ZX_INFO_HANDLE_COUNT, &handle_count,
                                                             sizeof(handle_count), &actual, &avail);
  if (status != ZX_OK) {
    return status;
  }
  if (handle_count.handle_count != 1) {
    return ZX_ERR_NO_RESOURCES;
  }
  if (size > height_ * stride_ * ZX_PIXEL_FORMAT_BYTES(format_)) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  return framebuffer_mmio_.get_vmo()->duplicate(ZX_RIGHT_SAME_RIGHTS, vmo_out);
}

zx_status_t SimpleDisplay::DisplayControllerImplGetSysmemConnection(zx::channel connection) {
  zx_status_t status = sysmem_connect(&sysmem_, connection.release());
  if (status != ZX_OK) {
    zxlogf(ERROR, "could not connect to sysmem");
    return status;
  }

  return ZX_OK;
}

zx_status_t SimpleDisplay::DisplayControllerImplSetBufferCollectionConstraints(
    const image_t* config, uint32_t collection) {
  fuchsia_sysmem::wire::BufferCollectionConstraints constraints = {};
  constraints.usage.display = fuchsia_sysmem::wire::kDisplayUsageLayer;
  constraints.has_buffer_memory_constraints = true;
  fuchsia_sysmem::wire::BufferMemoryConstraints& buffer_constraints =
      constraints.buffer_memory_constraints;
  buffer_constraints.min_size_bytes = 0;
  uint32_t bytes_per_row = stride_ * ZX_PIXEL_FORMAT_BYTES(format_);
  buffer_constraints.max_size_bytes = height_ * bytes_per_row;
  buffer_constraints.physically_contiguous_required = false;
  buffer_constraints.secure_required = false;
  buffer_constraints.ram_domain_supported = true;
  buffer_constraints.cpu_domain_supported = true;
  buffer_constraints.heap_permitted_count = 1;
  buffer_constraints.heap_permitted[0] = fuchsia_sysmem::wire::HeapType::kFramebuffer;
  constraints.image_format_constraints_count = 1;
  fuchsia_sysmem::wire::ImageFormatConstraints& image_constraints =
      constraints.image_format_constraints[0];
  switch (format_) {
    case ZX_PIXEL_FORMAT_ARGB_8888:
    case ZX_PIXEL_FORMAT_RGB_x888:
      image_constraints.pixel_format.type = fuchsia_sysmem::wire::PixelFormatType::kBgra32;
      break;
    case ZX_PIXEL_FORMAT_ABGR_8888:
    case ZX_PIXEL_FORMAT_BGR_888x:
      image_constraints.pixel_format.type = fuchsia_sysmem::wire::PixelFormatType::kR8G8B8A8;
      break;
    case ZX_PIXEL_FORMAT_RGB_888:
      image_constraints.pixel_format.type = fuchsia_sysmem::wire::PixelFormatType::kBgr24;
      break;
  }
  image_constraints.pixel_format.has_format_modifier = true;
  image_constraints.pixel_format.format_modifier.value =
      fuchsia_sysmem::wire::kFormatModifierLinear;
  image_constraints.color_spaces_count = 1;
  image_constraints.color_space[0].type = fuchsia_sysmem::wire::ColorSpaceType::kSrgb;
  image_constraints.min_coded_width = width_;
  image_constraints.max_coded_width = width_;
  image_constraints.min_coded_height = height_;
  image_constraints.max_coded_height = height_;
  image_constraints.min_bytes_per_row = bytes_per_row;
  image_constraints.max_bytes_per_row = bytes_per_row;
  constraints.image_format_constraints_count = 1;

  auto result = fidl::WireCall(fidl::UnownedClientEnd<fuchsia_sysmem::BufferCollection>(
                                   zx::unowned_channel(collection)))
                    ->SetConstraints(true, constraints);

  if (!result.ok()) {
    zxlogf(ERROR, "failed to set constraints, %s", result.FormatDescription().c_str());
    return result.status();
  }

  return ZX_OK;
}

// implement device protocol:

void SimpleDisplay::DdkRelease() { delete this; }

// implement sysmem heap protocol:

void SimpleDisplay::AllocateVmo(AllocateVmoRequestView request,
                                AllocateVmoCompleter::Sync& completer) {
  zx_info_handle_count handle_count;
  size_t actual, avail;
  zx_status_t status = framebuffer_mmio_.get_vmo()->get_info(ZX_INFO_HANDLE_COUNT, &handle_count,
                                                             sizeof(handle_count), &actual, &avail);
  if (status != ZX_OK) {
    completer.Reply(status, zx::vmo{});
    return;
  }
  if (handle_count.handle_count != 1) {
    completer.Reply(ZX_ERR_NO_RESOURCES, zx::vmo{});
    return;
  }
  zx::vmo vmo;
  status = framebuffer_mmio_.get_vmo()->duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo);
  if (status != ZX_OK) {
    completer.Reply(status, zx::vmo{});
  } else {
    completer.Reply(ZX_OK, std::move(vmo));
  }
}

void SimpleDisplay::CreateResource(CreateResourceRequestView request,
                                   CreateResourceCompleter::Sync& completer) {
  zx_info_handle_basic_t framebuffer_info;
  size_t actual, avail;
  zx_status_t status = request->vmo.get_info(ZX_INFO_HANDLE_BASIC, &framebuffer_info,
                                             sizeof(framebuffer_info), &actual, &avail);
  if (status != ZX_OK) {
    completer.Reply(status, 0);
    return;
  }
  zx_koid_t expect = ZX_KOID_INVALID;
  if (!framebuffer_koid_.compare_exchange_strong(expect, framebuffer_info.koid)) {
    completer.Reply(ZX_ERR_NO_RESOURCES, 0);
    return;
  }
  completer.Reply(ZX_OK, 0);
}

void SimpleDisplay::DestroyResource(DestroyResourceRequestView request,
                                    DestroyResourceCompleter::Sync& completer) {
  framebuffer_koid_ = ZX_KOID_INVALID;
  completer.Reply();
}

// implement driver object:

zx_status_t SimpleDisplay::Bind(const char* name, std::unique_ptr<SimpleDisplay>* vbe_ptr) {
  zx_status_t status;
  zx::channel heap_request, heap_connection;
  if ((status = zx::channel::create(0, &heap_request, &heap_connection)) != ZX_OK) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  if ((status = sysmem_register_heap(
           &sysmem_, static_cast<uint64_t>(fuchsia_sysmem2::wire::HeapType::kFramebuffer),
           heap_connection.release())) != ZX_OK) {
    printf("%s: failed to register sysmem heap: %d\n", name, status);
    return ZX_ERR_NOT_SUPPORTED;
  }

  status = DdkAdd(name);
  if (status != ZX_OK) {
    return status;
  }

  // Start heap server.
  auto arena = std::make_unique<fidl::Arena<512>>();
  fuchsia_sysmem2::wire::HeapProperties heap_properties = GetHeapProperties(*arena.get());
  async::PostTask(
      loop_.dispatcher(),
      [server_end = fidl::ServerEnd<fuchsia_sysmem2::Heap>(std::move(heap_request)),
       arena = std::move(arena), heap_properties = std::move(heap_properties), this]() mutable {
        auto binding = fidl::BindServer(loop_.dispatcher(), std::move(server_end), this,
                                        [](SimpleDisplay* self, fidl::UnbindInfo info,
                                           fidl::ServerEnd<fuchsia_sysmem2::Heap> server_end) {
                                          OnHeapServerClose(info, server_end.TakeChannel());
                                        });
        auto result = fidl::WireSendEvent(binding)->OnRegister(std::move(heap_properties));
        if (!result.ok()) {
          zxlogf(ERROR, "OnRegister() failed: %s", result.FormatDescription().c_str());
        }
      });

  // Start vsync loop.
  async::PostTask(loop_.dispatcher(), [this]() { OnPeriodicVSync(); });

  // DevMgr now owns this pointer, release it to avoid destroying the object
  // when device goes out of scope.
  __UNUSED auto ptr = vbe_ptr->release();

  zxlogf(INFO, "%s: initialized display, %u x %u (stride=%u format=%08x)", name, width_, height_,
         stride_, format_);

  return ZX_OK;
}

SimpleDisplay::SimpleDisplay(zx_device_t* parent, sysmem_protocol_t sysmem,
                             fdf::MmioBuffer framebuffer_mmio, uint32_t width, uint32_t height,
                             uint32_t stride, zx_pixel_format_t format)
    : DeviceType(parent),
      sysmem_(std::move(sysmem)),
      loop_(&kAsyncLoopConfigNoAttachToCurrentThread),
      framebuffer_koid_(ZX_KOID_INVALID),
      has_image_(false),
      framebuffer_mmio_(std::move(framebuffer_mmio)),
      width_(width),
      height_(height),
      stride_(stride),
      format_(format),
      next_vsync_time_(zx::clock::get_monotonic()) {
  // Start thread. Heap server must be running on a separate
  // thread as sysmem might be making synchronous allocation requests
  // from the main thread.
  loop_.StartThread("simple-display");
}

void SimpleDisplay::OnPeriodicVSync() {
  if (intf_.is_valid()) {
    fbl::AutoLock lock(&mtx_);
    intf_.OnDisplayVsync(kDisplayId, next_vsync_time_.get(), &config_stamp_);
  }
  next_vsync_time_ += kVSyncInterval;
  async::PostTaskForTime(
      loop_.dispatcher(), [this]() { OnPeriodicVSync(); }, next_vsync_time_);
}

zx_status_t bind_simple_pci_display_bootloader(zx_device_t* dev, const char* name, uint32_t bar,
                                               bool use_fidl) {
  uint32_t format, width, height, stride;
  // Please do not use get_root_resource() in new code. See fxbug.dev/31358.
  zx_status_t status =
      zx_framebuffer_get_info(get_root_resource(), &format, &width, &height, &stride);
  if (status != ZX_OK) {
    printf("%s: failed to get bootloader dimensions: %d\n", name, status);
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (use_fidl) {
    return bind_simple_fidl_pci_display(dev, name, bar, width, height, stride, format);
  }
  return bind_simple_pci_display(dev, name, bar, width, height, stride, format);
}

zx_status_t bind_simple_pci_display(zx_device_t* dev, const char* name, uint32_t bar,
                                    uint32_t width, uint32_t height, uint32_t stride,
                                    zx_pixel_format_t format) {
  pci_protocol_t pci;
  zx_status_t status = ZX_ERR_NOT_SUPPORTED;
  if ((status = device_get_fragment_protocol(dev, "pci", ZX_PROTOCOL_PCI, &pci)) != ZX_OK) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  sysmem_protocol_t sysmem;
  if ((status = device_get_fragment_protocol(dev, "sysmem", ZX_PROTOCOL_SYSMEM, &sysmem)) !=
      ZX_OK) {
    zxlogf(ERROR, "%s: could not get SYSMEM protocol: %d", name, status);
    return status;
  }

  mmio_buffer_t mmio;
  // map framebuffer window
  status = pci_map_bar_buffer(&pci, bar, ZX_CACHE_POLICY_WRITE_COMBINING, &mmio);
  if (status != ZX_OK) {
    printf("%s: failed to map pci bar %d: %d\n", name, bar, status);
    return status;
  }
  fdf::MmioBuffer framebuffer_mmio(mmio);

  fbl::AllocChecker ac;
  std::unique_ptr<SimpleDisplay> display(new (&ac) SimpleDisplay(
      dev, std::move(sysmem), std::move(framebuffer_mmio), width, height, stride, format));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  return display->Bind(name, &display);
}

zx_status_t bind_simple_fidl_pci_display(zx_device_t* dev, const char* name, uint32_t bar,
                                         uint32_t width, uint32_t height, uint32_t stride,
                                         zx_pixel_format_t format) {
  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_pci::Device>();
  if (endpoints.is_error()) {
    zxlogf(ERROR, "%s: could not create FIDL endpoints: %s", name, endpoints.status_string());
    return endpoints.status_value();
  }

  zx_status_t status = device_connect_fragment_fidl_protocol(
      dev, "pci", fidl::DiscoverableProtocolName<fuchsia_hardware_pci::Device>,
      endpoints->server.TakeHandle().release());
  if (status != ZX_OK) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  fidl::WireSyncClient<fuchsia_hardware_pci::Device> pci(std::move(endpoints->client));
  sysmem_protocol_t sysmem;
  if ((status = device_get_fragment_protocol(dev, "sysmem", ZX_PROTOCOL_SYSMEM, &sysmem)) !=
      ZX_OK) {
    zxlogf(ERROR, "%s: could not get SYSMEM protocol: %d", name, status);
    return status;
  }

  fidl::WireResult<fuchsia_hardware_pci::Device::GetBar> bar_result = pci->GetBar(bar);
  if (!bar_result.ok()) {
    zxlogf(ERROR, "Failed to send map PCI bar %d: %s", bar, bar_result.FormatDescription().data());
    return bar_result.status();
  }

  if (bar_result.value_NEW().is_error()) {
    zxlogf(ERROR, "Failed to map PCI bar %d: %s", bar,
           zx_status_get_string(bar_result.value_NEW().error_value()));
    return bar_result.value_NEW().error_value();
  }

  if (!bar_result.value_NEW().value()->result.result.is_vmo()) {
    zxlogf(ERROR, "PCI bar %u is not an MMIO BAR!", bar);
    return ZX_ERR_WRONG_TYPE;
  }

  // map framebuffer window
  auto mmio =
      fdf::MmioBuffer::Create(0, bar_result.value_NEW().value()->result.size,
                              std::move(bar_result.value_NEW().value()->result.result.vmo()),
                              ZX_CACHE_POLICY_WRITE_COMBINING);
  if (mmio.is_error()) {
    printf("%s: failed to map pci bar %d: %s\n", name, bar, mmio.status_string());
    return mmio.status_value();
  }

  auto display = std::make_unique<SimpleDisplay>(dev, std::move(sysmem), std::move(*mmio), width,
                                                 height, stride, format);

  return display->Bind(name, &display);
}
