// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_F2FS_F2FS_H_
#define SRC_STORAGE_F2FS_F2FS_H_

// clang-format off
#ifdef __Fuchsia__

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fidl-async/cpp/bind.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <fbl/auto_lock.h>
#include <fbl/condition_variable.h>
#include <fbl/mutex.h>
#include <fidl/fuchsia.fs/cpp/wire.h>
#endif  // __Fuchsia__

#include <fcntl.h>

#include <storage/buffer/vmoid_registry.h>

#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/listnode.h>
#include <zircon/types.h>

#include <lib/syslog/cpp/macros.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/zx/status.h>

#include <fbl/algorithm.h>
#include <fbl/intrusive_wavl_tree.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/macros.h>
#include <fbl/ref_ptr.h>
#include <fbl/string_buffer.h>

#include <condition_variable>

#ifdef __Fuchsia__
#include "src/lib/storage/vfs/cpp/paged_vfs.h"
#include "src/lib/storage/vfs/cpp/paged_vnode.h"
#include "src/lib/storage/vfs/cpp/watcher.h"
#include "src/lib/storage/vfs/cpp/shared_mutex.h"
#include "src/lib/storage/vfs/cpp/service.h"

#include "lib/inspect/cpp/inspect.h"
#include "lib/inspect/service/cpp/service.h"
#include "src/lib/storage/vfs/cpp/fuchsia_vfs.h"
#include "src/lib/storage/vfs/cpp/inspect/inspect_tree.h"
#else  // __Fuchsia__
#include "src/storage/f2fs/sync_host.h"
#endif  // __Fuchsia__

#include "src/lib/storage/vfs/cpp/vfs.h"
#include "src/lib/storage/vfs/cpp/vnode.h"
#include "src/lib/storage/vfs/cpp/transaction/buffered_operations_builder.h"

#include "src/storage/f2fs/f2fs_types.h"
#include "src/storage/f2fs/f2fs_lib.h"
#include "src/storage/f2fs/f2fs_layout.h"
#ifdef __Fuchsia__
#include "src/storage/f2fs/vmo_manager.h"
#endif  // __Fuchsia__
#include "src/storage/f2fs/file_cache.h"
#include "src/storage/f2fs/node_page.h"
#include "src/storage/f2fs/f2fs_internal.h"
#include "src/storage/f2fs/namestring.h"
#include "src/storage/f2fs/bcache.h"
#include "src/storage/f2fs/writeback.h"
#include "src/storage/f2fs/vnode.h"
#include "src/storage/f2fs/vnode_cache.h"
#include "src/storage/f2fs/dir.h"
#include "src/storage/f2fs/file.h"
#include "src/storage/f2fs/node.h"
#include "src/storage/f2fs/segment.h"
#include "src/storage/f2fs/mkfs.h"
#include "src/storage/f2fs/mount.h"
#include "src/storage/f2fs/fsck.h"
#include "src/storage/f2fs/admin.h"
#include "src/storage/f2fs/dir_entry_cache.h"
#ifdef __Fuchsia__
#include "src/storage/f2fs/inspect.h"
#endif  // __Fuchsia__
// clang-format on

namespace f2fs {

zx_status_t LoadSuperblock(f2fs::Bcache *bc, Superblock *out_info);
zx_status_t LoadSuperblock(f2fs::Bcache *bc, Superblock *out_info, block_t bno);

#ifdef __Fuchsia__
zx::status<std::unique_ptr<F2fs>> CreateFsAndRoot(const MountOptions &mount_options,
                                                  async_dispatcher_t *dispatcher,
                                                  std::unique_ptr<f2fs::Bcache> bcache,
                                                  fidl::ServerEnd<fuchsia_io::Directory> root,
                                                  fit::closure on_unmount);

using SyncCallback = fs::Vnode::SyncCallback;
#else   // __Fuchsia__
zx::status<std::unique_ptr<F2fs>> CreateFsAndRoot(const MountOptions &mount_options,
                                                  std::unique_ptr<f2fs::Bcache> bcache);
#endif  // __Fuchsia__

#ifdef __Fuchsia__
// The F2fs class *has* to be final because it calls PagedVfs::TearDown from
// its destructor which is required to ensure thread-safety at destruction time.
class F2fs final : public fs::PagedVfs {
#else   // __Fuchsia__
class F2fs : public fs::Vfs {
#endif  // __Fuchsia__
 public:
  // Not copyable or moveable
  F2fs(const F2fs &) = delete;
  F2fs &operator=(const F2fs &) = delete;
  F2fs(F2fs &&) = delete;
  F2fs &operator=(F2fs &&) = delete;
  ~F2fs() override;

#ifdef __Fuchsia__
  explicit F2fs(async_dispatcher_t *dispatcher, std::unique_ptr<f2fs::Bcache> bc,
                std::unique_ptr<Superblock> sb, const MountOptions &mount_options);
  [[nodiscard]] static zx_status_t Create(async_dispatcher_t *dispatcher,
                                          std::unique_ptr<f2fs::Bcache> bc,
                                          const MountOptions &options, std::unique_ptr<F2fs> *out);

  void SetUnmountCallback(fit::closure closure) { on_unmount_ = std::move(closure); }
  void Shutdown(fs::FuchsiaVfs::ShutdownCallback cb) final;
  void OnNoConnections() final;

  void SetAdminService(fbl::RefPtr<AdminService> svc) { admin_svc_ = std::move(svc); }

  zx::status<fs::FilesystemInfo> GetFilesystemInfo() final;
  DirEntryCache &GetDirEntryCache() { return dir_entry_cache_; }
  InspectTree &GetInspectTree() { return inspect_tree_; }
  void Sync(SyncCallback closure);
#else   // __Fuchsia__
  explicit F2fs(std::unique_ptr<f2fs::Bcache> bc, std::unique_ptr<Superblock> sb,
                const MountOptions &mount_options);
  [[nodiscard]] static zx_status_t Create(std::unique_ptr<f2fs::Bcache> bc,
                                          const MountOptions &options, std::unique_ptr<F2fs> *out);
#endif  // __Fuchsia__

  VnodeCache &GetVCache() { return vnode_cache_; }
  inline zx_status_t InsertVnode(VnodeF2fs *vn) { return vnode_cache_.Add(vn); }
  inline void EvictVnode(VnodeF2fs *vn) { __UNUSED zx_status_t status = vnode_cache_.Evict(vn); }
  inline zx_status_t LookupVnode(ino_t ino, fbl::RefPtr<VnodeF2fs> *out) {
    return vnode_cache_.Lookup(ino, out);
  }

  void ResetBc(std::unique_ptr<f2fs::Bcache> *out = nullptr) {
    if (out == nullptr) {
      bc_.reset();
      return;
    }
    *out = std::move(bc_);
  }
  Bcache &GetBc() { return *bc_; }
  Superblock &RawSb() { return *raw_sb_; }
  SuperblockInfo &GetSuperblockInfo() { return *superblock_info_; }
  SegmentManager &GetSegmentManager() { return *segment_manager_; }
  NodeManager &GetNodeManager() { return *node_manager_; }

  // For testing Reset() and ResetBc()
  bool IsValid() const;
  void ResetPsuedoVnodes() {
    root_vnode_.reset();
    meta_vnode_.reset();
    node_vnode_.reset();
  }
  void ResetSuperblockInfo() { superblock_info_.reset(); }
  void ResetSegmentManager() {
    segment_manager_->DestroySegmentManager();
    segment_manager_.reset();
  }
  void ResetNodeManager() {
    node_manager_->DestroyNodeManager();
    node_manager_.reset();
  }

  // super.cc
  void PutSuper();
  void SyncFs(bool bShutdown = false);
  zx_status_t SanityCheckRawSuper();
  zx_status_t SanityCheckCkpt();
  void InitSuperblockInfo();
  zx_status_t FillSuper();
  void ParseOptions();
  void Reset();
#if 0  // porting needed
  void InitOnce(void *foo);
  VnodeF2fs *F2fsAllocInode();
  static void F2fsICallback(rcu_head *head);
  void F2fsDestroyInode(inode *inode);
  int F2fsStatfs(dentry *dentry /*, kstatfs *buf*/);
  int F2fsShowOptions(/*seq_file *seq*/);
  VnodeF2fs *F2fsNfsGetInode(uint64_t ino, uint32_t generation);
  dentry *F2fsFhToDentry(fid *fid, int fh_len, int fh_type);
  dentry *F2fsFhToParent(fid *fid, int fh_len, int fh_type);
  dentry *F2fsMount(file_system_type *fs_type, int flags,
       const char *dev_name, void *data);
  int InitInodecache(void);
  void DestroyInodecache(void);
  int /*__init*/ initF2fsFs(void);
  void /*__exit*/ exitF2fsFs(void);
#endif

  // checkpoint.cc
  zx_status_t GrabMetaPage(pgoff_t index, LockedPage *out);
  zx_status_t GetMetaPage(pgoff_t index, LockedPage *out);
  zx_status_t F2fsWriteMetaPage(LockedPage &page, bool is_reclaim = false);

  zx_status_t CheckOrphanSpace();
  void AddOrphanInode(VnodeF2fs *vnode);
  void AddOrphanInode(nid_t ino);
  void RemoveOrphanInode(nid_t ino);
  void RecoverOrphanInode(nid_t ino);
  int RecoverOrphanInodes();
  void WriteOrphanInodes(block_t start_blk);
  zx_status_t GetValidCheckpoint();
  zx_status_t ValidateCheckpoint(block_t cp_addr, uint64_t *version, LockedPage *out);
  void BlockOperations();
  void UnblockOperations();
  void DoCheckpoint(bool is_umount);
  void WriteCheckpoint(bool blocked, bool is_umount);
  void InitOrphanInfo();
#if 0  // porting needed
  int F2fsWriteMetaPages(address_space *mapping, WritebackControl *wbc);
  int F2fsSetMetaPageDirty(Page *page);
  void SetDirtyDirPage(VnodeF2fs *vnode, Page *page);
  void RemoveDirtyDirInode(VnodeF2fs *vnode);
  int CreateCheckpointCaches();
  void DestroyCheckpointCaches();
#endif

  // recovery.cc
  bool SpaceForRollForward();
  FsyncInodeEntry *GetFsyncInode(list_node_t *head, nid_t ino);
  // TODO: Use reference type parameters instead of pointer type
  zx_status_t RecoverDentry(NodePage *ipage, VnodeF2fs *vnode);
  zx_status_t RecoverInode(VnodeF2fs *inode, NodePage *node_page);
  zx_status_t FindFsyncDnodes(list_node_t *head);
  void DestroyFsyncDnodes(list_node_t *head);
  void CheckIndexInPrevNodes(block_t blkaddr);
  void DoRecoverData(VnodeF2fs *inode, NodePage *page, block_t blkaddr);
  void RecoverData(list_node_t *head, CursegType type);
  void RecoverFsyncData();

  // block count
  void DecValidBlockCount(VnodeF2fs *vnode, block_t count);
  zx_status_t IncValidBlockCount(VnodeF2fs *vnode, block_t count);
  block_t ValidUserBlocks();
  uint32_t ValidNodeCount();
  void IncValidInodeCount();
  void DecValidInodeCount();
  uint32_t ValidInodeCount();
  loff_t MaxFileSize(unsigned bits);

  VnodeF2fs &GetNodeVnode() { return *node_vnode_; }
  VnodeF2fs &GetMetaVnode() { return *meta_vnode_; }

  // Flush all dirty Pages for the meta vnode that meet |operation|.if_page.
  pgoff_t SyncMetaPages(WritebackOperation &operation);
  // Flush all dirty data Pages for dirty vnodes that meet |operation|.if_vnode and if_page.
  pgoff_t SyncDirtyDataPages(WritebackOperation &operation);

  zx_status_t MakeOperation(storage::OperationType op, LockedPage &page, block_t blk_addr,
                            PageType type, block_t nblocks = 1);

  zx_status_t MakeOperation(storage::OperationType op, block_t blk_addr, block_t nblocks = 1);

  void ScheduleWriterSubmitPages(sync_completion_t *completion = nullptr,
                                 PageType type = PageType::kNrPageType) {
    writer_->ScheduleSubmitPages(completion, type);
  }

 private:
  zx_status_t MakeReadOperation(LockedPage &page, block_t blk_addr, bool is_sync = true);
  zx_status_t MakeWriteOperation(LockedPage &page, block_t blk_addr, PageType type);

  std::unique_ptr<f2fs::Bcache> bc_;

  std::unique_ptr<VnodeF2fs> node_vnode_;
  std::unique_ptr<VnodeF2fs> meta_vnode_;
  fbl::RefPtr<VnodeF2fs> root_vnode_;
  fit::closure on_unmount_;
  MountOptions mount_options_;

  std::shared_ptr<Superblock> raw_sb_;
  std::unique_ptr<SuperblockInfo> superblock_info_;
  std::unique_ptr<SegmentManager> segment_manager_;
  std::unique_ptr<NodeManager> node_manager_;

  VnodeCache vnode_cache_;
  std::unique_ptr<Writer> writer_;

#ifdef __Fuchsia__
  DirEntryCache dir_entry_cache_;
  fbl::RefPtr<AdminService> admin_svc_;
  zx::event fs_id_;
  InspectTree inspect_tree_;
#endif  // __Fuchsia__
};

f2fs_hash_t DentryHash(std::string_view name);

}  // namespace f2fs

#endif  // SRC_STORAGE_F2FS_F2FS_H_
