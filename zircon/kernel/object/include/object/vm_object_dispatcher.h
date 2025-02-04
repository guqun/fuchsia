// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_OBJECT_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_OBJECT_DISPATCHER_H_

#include <lib/user_copy/user_iovec.h>
#include <lib/user_copy/user_ptr.h>
#include <lib/zx/status.h>
#include <sys/types.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <fbl/canary.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <vm/vm_object.h>

class VmObjectDispatcher final : public SoloDispatcher<VmObjectDispatcher, ZX_DEFAULT_VMO_RIGHTS>,
                                 public VmObjectChildObserver {
 public:
  enum class InitialMutability { kMutable, kImmutable };

  static zx_status_t parse_create_syscall_flags(uint32_t flags, uint32_t* out_flags);

  static zx_status_t Create(fbl::RefPtr<VmObject> vmo, uint64_t content_size,
                            InitialMutability initial_mutability,
                            KernelHandle<VmObjectDispatcher>* handle, zx_rights_t* rights) {
    return Create(ktl::move(vmo), content_size, ZX_KOID_INVALID, initial_mutability, handle,
                  rights);
  }

  static zx_status_t Create(fbl::RefPtr<VmObject> vmo, uint64_t content_size, zx_koid_t pager_koid,
                            InitialMutability initial_mutability,
                            KernelHandle<VmObjectDispatcher>* handle, zx_rights_t* rights);
  ~VmObjectDispatcher() final;

  // VmObjectChildObserver implementation.
  void OnZeroChild() final;
  void OnOneChild() final;

  // SoloDispatcher implementation.
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_VMO; }
  void get_name(char (&out_name)[ZX_MAX_NAME_LEN]) const final;
  zx_status_t set_name(const char* name, size_t len) final;

  // Dispatcher implementation.
  void on_zero_handles() final;

  // VmObjectDispatcher own methods.
  zx_status_t Read(VmAspace* current_aspace, user_out_ptr<char> user_data, size_t length,
                   uint64_t offset, size_t* out_actual);
  zx_status_t ReadVector(VmAspace* current_aspace, user_out_iovec_t user_data, size_t length,
                         uint64_t offset, size_t* out_actual);
  zx_status_t Write(VmAspace* current_aspace, user_in_ptr<const char> user_data, size_t length,
                    uint64_t offset, size_t* out_actual);
  zx_status_t WriteVector(VmAspace* current_aspace, user_in_iovec_t user_data, size_t length,
                          uint64_t offset, size_t* out_actual);
  zx_status_t SetSize(uint64_t);
  zx_status_t GetSize(uint64_t* size);
  zx_status_t RangeOp(uint32_t op, uint64_t offset, uint64_t size, user_inout_ptr<void> buffer,
                      size_t buffer_size, zx_rights_t rights);
  zx_status_t CreateChild(uint32_t options, uint64_t offset, uint64_t size, bool copy_name,
                          fbl::RefPtr<VmObject>* child_vmo);

  zx_status_t SetMappingCachePolicy(uint32_t cache_policy);

  zx_info_vmo_t GetVmoInfo(zx_rights_t rights);

  zx_status_t SetContentSize(uint64_t);
  uint64_t GetContentSize() const;

  // Attempts to resize the VMO to fit the |requested_content_size|. Returns the actual content size
  // upon success, since the content size might have been expanded to partially accommodate
  // |requested_content_size|. Otherwise returns the failure status.
  zx::status<uint64_t> ExpandContentIfNeeded(uint64_t requested_content_size,
                                             uint64_t zero_until_offset);

  const fbl::RefPtr<VmObject>& vmo() const { return vmo_; }
  zx_koid_t pager_koid() const { return pager_koid_; }

  // The shrink lock exists to prevent the VMO from being shrunk whilst stream reads are taking
  // place, which would otherwise cause errors to be returned to clients.  Readers that care (e.g.
  // reads that come via stream APIs) should acquire a ShrinkInhibitGuard whilst shrinking should
  // acquire the ShrinkGuard.  For now, a mutex is used but it is possible that this could become a
  // reader/writer lock at some point.
  using ShrinkInhibitGuard = Guard<Mutex>;
  using ShrinkGuard = Guard<Mutex>;
  Lock<Mutex>& shrink_lock() const { return shrink_lock_; }

 private:
  explicit VmObjectDispatcher(fbl::RefPtr<VmObject> vmo, uint64_t size, zx_koid_t pager_koid,
                              InitialMutability initial_mutability);
  // The 'const' here is load bearing; we give a raw pointer to
  // ourselves to |vmo_| so we have to ensure we don't reset vmo_
  // except during destruction.
  fbl::RefPtr<VmObject> const vmo_;

  // The content_size_lock_ is used to synchronize vmo_ operations and updates to content_size_.
  // Ideally the existing dispatchers lock would be used, but presently it is possible for page
  // requests to get waited on while this lock is held due to calls to vmo_->ZeroRange, and so
  // prefer to use a separate lock that we can add instrumentation to without needing to change the
  // entire dispatcher lock.
  // TODO: Remove this and use dispatcher lock once content size operations will not block.
  mutable DECLARE_MUTEX(VmObjectDispatcher,
                        lockdep::LockFlagsActiveListDisabled) content_size_lock_;

  // The size of the content stored in the VMO in bytes.
  uint64_t content_size_ TA_GUARDED(content_size_lock_) = 0u;

  // The koid of the related pager object, or ZX_KOID_INVALID if
  // there is no related pager.
  const zx_koid_t pager_koid_;

  // Indicates whether the VMO was immutable at creation time.
  const InitialMutability initial_mutability_;

  // See the comment above near the shrink_lock() method. Note that this lock might be held whilst
  // waiting for page requests to be fulfilled.
  mutable DECLARE_MUTEX(VmObjectDispatcher, lockdep::LockFlagsActiveListDisabled) shrink_lock_;
};

zx_info_vmo_t VmoToInfoEntry(const VmObject* vmo, bool is_handle, zx_rights_t handle_rights);

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_OBJECT_DISPATCHER_H_
