// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "unit_lib.h"

#include <gtest/gtest.h>

#include "src/lib/storage/block_client/cpp/fake_block_device.h"
#include "src/storage/f2fs/f2fs.h"

namespace f2fs {

using block_client::FakeBlockDevice;

F2fsFakeDevTestFixture::F2fsFakeDevTestFixture(const TestOptions options)
    : block_count_(options.block_count),
      block_size_(options.block_size)

{
  for (auto opt : options.mount_options) {
    options_.SetValue(options_.GetNameView(opt.first), opt.second);
  }

  fbl::RefPtr<VnodeF2fs> root;
  FileTester::MkfsOnFakeDev(&bc_, block_count_);
  FileTester::MountWithOptions(loop_.dispatcher(), options_, &bc_, &fs_);
  FileTester::CreateRoot(fs_.get(), &root);
  root_dir_ = fbl::RefPtr<Dir>::Downcast(std::move(root));
}

F2fsFakeDevTestFixture::~F2fsFakeDevTestFixture() {
  root_dir_->Close();
  root_dir_ = nullptr;
  FileTester::Unmount(std::move(fs_), &bc_);
}

void FileTester::MkfsOnFakeDev(std::unique_ptr<Bcache> *bc, uint64_t block_count,
                               uint32_t block_size, bool btrim) {
  auto device = std::make_unique<FakeBlockDevice>(FakeBlockDevice::Config{
      .block_count = block_count, .block_size = block_size, .supports_trim = btrim});
  bool readonly_device = false;
  ASSERT_EQ(CreateBcache(std::move(device), &readonly_device, bc), ZX_OK);

  MkfsOptions options;
  MkfsWorker mkfs(std::move(*bc), options);
  auto ret = mkfs.DoMkfs();
  ASSERT_EQ(ret.is_error(), false);
  *bc = std::move(*ret);
}

void FileTester::MkfsOnFakeDevWithOptions(std::unique_ptr<Bcache> *bc, const MkfsOptions &options,
                                          uint64_t blockCount, uint32_t blockSize, bool btrim) {
  auto device = std::make_unique<FakeBlockDevice>(FakeBlockDevice::Config{
      .block_count = blockCount, .block_size = blockSize, .supports_trim = btrim});
  bool readonly_device = false;
  ASSERT_EQ(CreateBcache(std::move(device), &readonly_device, bc), ZX_OK);

  MkfsWorker mkfs(std::move(*bc), options);
  auto ret = mkfs.DoMkfs();
  ASSERT_EQ(ret.is_error(), false);
  *bc = std::move(*ret);
}

void FileTester::MountWithOptions(async_dispatcher_t *dispatcher, const MountOptions &options,
                                  std::unique_ptr<Bcache> *bc, std::unique_ptr<F2fs> *fs) {
  ASSERT_EQ(F2fs::Create(dispatcher, std::move(*bc), options, fs), ZX_OK);
}

void FileTester::Unmount(std::unique_ptr<F2fs> fs, std::unique_ptr<Bcache> *bc) {
  fs->SyncFs(true);
  fs->PutSuper();
  fs->ResetBc(bc);
  fs.reset();
}

void FileTester::SuddenPowerOff(std::unique_ptr<F2fs> fs, std::unique_ptr<Bcache> *bc) {
  fs->ResetPsuedoVnodes();
  fs->GetVCache().Reset();

  // destroy f2fs internal modules
  fs->GetNodeManager().DestroyNodeManager();
  fs->GetSegmentManager().DestroySegmentManager();

  fs->ResetBc(bc);
  fs.reset();
}

void FileTester::CreateRoot(F2fs *fs, fbl::RefPtr<VnodeF2fs> *out) {
  ASSERT_EQ(VnodeF2fs::Vget(fs, fs->RawSb().root_ino, out), ZX_OK);
  ASSERT_EQ((*out)->Open((*out)->ValidateOptions(fs::VnodeConnectionOptions()).value(), nullptr),
            ZX_OK);
}

void FileTester::Lookup(VnodeF2fs *parent, std::string_view name, fbl::RefPtr<fs::Vnode> *out) {
  fbl::RefPtr<fs::Vnode> vn = nullptr;
  if (zx_status_t status = parent->Lookup(name, &vn); status != ZX_OK) {
    *out = nullptr;
    return;
  }
  ASSERT_TRUE(vn);
  ASSERT_EQ(vn->Open(vn->ValidateOptions(fs::VnodeConnectionOptions()).value(), nullptr), ZX_OK);
  *out = std::move(vn);
}

void FileTester::CreateChild(Dir *vn, uint32_t mode, std::string_view name) {
  fbl::RefPtr<fs::Vnode> tmp_child;
  ASSERT_EQ(vn->Create(name, mode, &tmp_child), ZX_OK);
  ASSERT_EQ(tmp_child->Close(), ZX_OK);
}

void FileTester::DeleteChild(Dir *vn, std::string_view name) {
  ASSERT_EQ(vn->Unlink(name, true), ZX_OK);
  // TODO: After EvictInode available, check if nids of the child are correctly freed
}

void FileTester::CreateChildren(F2fs *fs, std::vector<fbl::RefPtr<VnodeF2fs>> &vnodes,
                                std::vector<uint32_t> &inos, fbl::RefPtr<Dir> &parent,
                                std::string name, uint32_t inode_cnt) {
  for (uint32_t i = 0; i < inode_cnt; ++i) {
    fbl::RefPtr<fs::Vnode> test_file;

    name += std::to_string(i);
    ASSERT_EQ(parent->Create(name, S_IFREG, &test_file), ZX_OK);
    fbl::RefPtr<VnodeF2fs> test_file_vn = fbl::RefPtr<VnodeF2fs>::Downcast(std::move(test_file));

    inos.push_back(test_file_vn->Ino());
    vnodes.push_back(std::move(test_file_vn));
  }
}

void FileTester::DeleteChildren(std::vector<fbl::RefPtr<VnodeF2fs>> &vnodes,
                                fbl::RefPtr<Dir> &parent, uint32_t inode_cnt) {
  uint32_t deleted_file_cnt = 0;
  for (const auto &iter : vnodes) {
    ASSERT_EQ(parent->Unlink(iter->GetNameView(), false), ZX_OK);
    ++deleted_file_cnt;
  }
  ASSERT_EQ(deleted_file_cnt, inode_cnt);
}

void FileTester::VnodeWithoutParent(F2fs *fs, uint32_t mode, fbl::RefPtr<VnodeF2fs> &vnode) {
  nid_t inode_nid;
  ASSERT_TRUE(fs->GetNodeManager().AllocNid(inode_nid));

  VnodeF2fs::Allocate(fs, inode_nid, mode, &vnode);
  ASSERT_EQ(vnode->Open(vnode->ValidateOptions(fs::VnodeConnectionOptions()).value(), nullptr),
            ZX_OK);
  vnode->UnlockNewInode();
  fs->GetNodeManager().AllocNidDone(vnode->Ino());

  fs->InsertVnode(vnode.get());
  vnode->MarkInodeDirty();
}

void FileTester::CheckInlineDir(VnodeF2fs *vn) {
  ASSERT_NE(vn->TestFlag(InodeInfoFlag::kInlineDentry), 0);
  ASSERT_EQ(vn->GetSize(), vn->MaxInlineData());
}

void FileTester::CheckNonInlineDir(VnodeF2fs *vn) {
  ASSERT_EQ(vn->TestFlag(InodeInfoFlag::kInlineDentry), 0);
  ASSERT_GT(vn->GetSize(), vn->MaxInlineData());
}

void FileTester::CheckInlineFile(VnodeF2fs *vn) {
  ASSERT_NE(vn->TestFlag(InodeInfoFlag::kInlineData), 0);
}

void FileTester::CheckNonInlineFile(VnodeF2fs *vn) {
  ASSERT_EQ(vn->TestFlag(InodeInfoFlag::kInlineData), 0);
}

void FileTester::CheckDataExistFlagSet(VnodeF2fs *vn) {
  ASSERT_NE(vn->TestFlag(InodeInfoFlag::kDataExist), 0);
}

void FileTester::CheckDataExistFlagUnset(VnodeF2fs *vn) {
  ASSERT_EQ(vn->TestFlag(InodeInfoFlag::kDataExist), 0);
}

void FileTester::CheckChildrenFromReaddir(Dir *dir, std::unordered_set<std::string> childs) {
  childs.insert(".");

  fs::VdirCookie cookie;
  uint8_t buf[kPageSize];
  size_t len;

  ASSERT_EQ(dir->Readdir(&cookie, buf, sizeof(buf), &len), ZX_OK);

  uint8_t *buf_ptr = buf;

  while (len > 0 && buf_ptr < buf + kPageSize) {
    auto entry = reinterpret_cast<const vdirent_t *>(buf_ptr);
    size_t entry_size = entry->size + sizeof(vdirent_t);

    std::string_view entry_name(entry->name, entry->size);
    auto iter = childs.begin();
    for (; iter != childs.end(); ++iter) {
      if (entry_name == *iter) {
        break;
      }
    }

    ASSERT_NE(iter, childs.end());
    childs.erase(iter);

    buf_ptr += entry_size;
    len -= entry_size;
  }

  ASSERT_TRUE(childs.empty());
}

void FileTester::CheckChildrenInBlock(Dir *vn, uint64_t bidx,
                                      std::unordered_set<std::string> childs) {
  if (bidx == 0) {
    childs.insert(".");
    childs.insert("..");
  }

  fbl::RefPtr<Page> page = nullptr;

  if (childs.empty()) {
    ASSERT_EQ(vn->FindDataPage(bidx, &page), ZX_ERR_NOT_FOUND);
    return;
  }

  ASSERT_EQ(vn->FindDataPage(bidx, &page), ZX_OK);
  DentryBlock *dentry_blk = page->GetAddress<DentryBlock>();

  uint32_t bit_pos = FindNextBit(dentry_blk->dentry_bitmap, kNrDentryInBlock, 0);
  while (bit_pos < kNrDentryInBlock) {
    DirEntry *de = &dentry_blk->dentry[bit_pos];
    uint32_t slots = (LeToCpu(de->name_len) + kNameLen - 1) / kNameLen;

    std::string_view dir_entry_name(reinterpret_cast<char *>(dentry_blk->filename[bit_pos]),
                                    LeToCpu(de->name_len));
    auto iter = childs.begin();
    for (; iter != childs.end(); ++iter) {
      if (dir_entry_name == *iter) {
        break;
      }
    }

    ASSERT_NE(iter, childs.end());
    childs.erase(iter);

    bit_pos = FindNextBit(dentry_blk->dentry_bitmap, kNrDentryInBlock, bit_pos + slots);
  }

  ASSERT_TRUE(childs.empty());
}

std::string FileTester::GetRandomName(unsigned int len) {
  const char *char_list = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  auto char_list_len = strlen(char_list);
  auto generator = [&]() { return char_list[rand() % char_list_len]; };
  std::string str(len, 0);
  std::generate_n(str.begin(), len, generator);
  return str;
}

void FileTester::AppendToFile(File *file, const void *data, size_t len) {
  size_t end = 0;
  size_t ret = 0;

  ASSERT_EQ(file->Append(data, len, &end, &ret), ZX_OK);
  ASSERT_EQ(ret, len);
}

void FileTester::ReadFromFile(File *file, void *data, size_t len, size_t off) {
  size_t ret = 0;

  ASSERT_EQ(file->Read(data, len, off, &ret), ZX_OK);
  ASSERT_EQ(ret, len);
}

void MapTester::CheckNodeLevel(F2fs *fs, VnodeF2fs *vn, int level) {
  LockedPage ipage;
  ASSERT_EQ(fs->GetNodeManager().GetNodePage(vn->Ino(), &ipage), ZX_OK);
  Inode *inode = &(ipage->GetAddress<Node>()->i);

  int i;
  for (i = 0; i < level; ++i)
    ASSERT_NE(inode->i_nid[i], 0U);

  for (; i < kNidsPerInode; ++i)
    ASSERT_EQ(inode->i_nid[i], 0U);
}

void MapTester::CheckNidsFree(F2fs *fs, std::unordered_set<nid_t> &nids) {
  NodeManager &nm_i = fs->GetNodeManager();

  std::lock_guard lock(nm_i.free_nid_list_lock_);
  for (auto nid : nids) {
    bool found = false;
    list_node_t *iter;
    list_for_every(&nm_i.free_nid_list_, iter) {
      FreeNid *fnid = containerof(iter, FreeNid, list);
      if (fnid->nid == nid) {
        found = true;
        break;
      }
    }
    ASSERT_TRUE(found);
  }
}

void MapTester::CheckNidsInuse(F2fs *fs, std::unordered_set<nid_t> &nids) {
  NodeManager &nm_i = fs->GetNodeManager();

  std::lock_guard lock(nm_i.free_nid_list_lock_);
  for (auto nid : nids) {
    bool found = false;
    list_node_t *iter;
    list_for_every(&nm_i.free_nid_list_, iter) {
      FreeNid *fnid = containerof(iter, FreeNid, list);
      if (fnid->nid == nid) {
        found = true;
        break;
      }
    }
    ASSERT_FALSE(found);
  }
}

void MapTester::CheckBlkaddrsFree(F2fs *fs, std::unordered_set<block_t> &blkaddrs) {
  SuperblockInfo &superblock_info = fs->GetSuperblockInfo();
  for (auto blkaddr : blkaddrs) {
    SegmentManager &manager = fs->GetSegmentManager();
    SegmentEntry &se = manager.GetSegmentEntry(manager.GetSegmentNumber(blkaddr));
    uint32_t offset = manager.GetSegOffFromSeg0(blkaddr) & (superblock_info.GetBlocksPerSeg() - 1);
    ASSERT_EQ(TestValidBitmap(offset, se.ckpt_valid_map.get()), 0);
  }
}

void MapTester::CheckBlkaddrsInuse(F2fs *fs, std::unordered_set<block_t> &blkaddrs) {
  SuperblockInfo &superblock_info = fs->GetSuperblockInfo();
  for (auto blkaddr : blkaddrs) {
    SegmentManager &manager = fs->GetSegmentManager();
    SegmentEntry &se = manager.GetSegmentEntry(manager.GetSegmentNumber(blkaddr));
    uint32_t offset = manager.GetSegOffFromSeg0(blkaddr) & (superblock_info.GetBlocksPerSeg() - 1);
    ASSERT_NE(TestValidBitmap(offset, se.ckpt_valid_map.get()), 0);
  }
}

void MapTester::CheckDnodePage(NodePage &page, nid_t exp_nid) {
  ASSERT_EQ(page.NidOfNode(), exp_nid);
  ASSERT_EQ(DatablockAddr(&page, uint64_t{1}), block_t{0});
}

bool MapTester::IsCachedNat(NodeManager &node_manager, nid_t n) {
  fs::SharedLock nat_lock(node_manager.nat_tree_lock_);
  auto entry = node_manager.nat_cache_.find(n);
  return entry != node_manager.nat_cache_.end();
}

void MapTester::RemoveTruncatedNode(NodeManager &node_manager, std::vector<nid_t> &nids) {
  fs::SharedLock nat_lock(node_manager.nat_tree_lock_);
  for (auto iter = nids.begin(); iter != nids.end();) {
    auto cache_entry = node_manager.nat_cache_.find(*iter);
    if (cache_entry != node_manager.nat_cache_.end()) {
      if ((*cache_entry).GetBlockAddress() == kNullAddr) {
        iter = nids.erase(iter);
      } else {
        ++iter;
      }
    }
  }
}

void MapTester::DoWriteNat(F2fs *fs, nid_t nid, block_t blkaddr, uint8_t version) {
  NodeManager *nm_i = &fs->GetNodeManager();
  std::unique_ptr<NatEntry> nat_entry = std::make_unique<NatEntry>();
  auto cache_entry = nat_entry.get();

  cache_entry->SetNid(nid);

  ZX_ASSERT(!(*cache_entry).fbl::WAVLTreeContainable<std::unique_ptr<NatEntry>>::InContainer());

  std::lock_guard nat_lock(nm_i->nat_tree_lock_);
  nm_i->nat_cache_.insert(std::move(nat_entry));

  ZX_ASSERT(!(*cache_entry).fbl::DoublyLinkedListable<NatEntry *>::InContainer());
  nm_i->clean_nat_list_.push_back(cache_entry);
  ++nm_i->nat_entries_count_;

  cache_entry->ClearCheckpointed();
  cache_entry->SetBlockAddress(blkaddr);
  cache_entry->SetVersion(version);
  ZX_ASSERT((*cache_entry).fbl::DoublyLinkedListable<NatEntry *>::InContainer());
  nm_i->clean_nat_list_.erase(*cache_entry);
  ZX_ASSERT(!(*cache_entry).fbl::DoublyLinkedListable<NatEntry *>::InContainer());
  nm_i->dirty_nat_list_.push_back(cache_entry);
}

void MapTester::ClearAllDirtyNatEntries(NodeManager &manager) {
  std::lock_guard nat_lock(manager.nat_tree_lock_);
  for (auto &dirty_entry : manager.dirty_nat_list_) {
    manager.dirty_nat_list_.erase(dirty_entry);
    --manager.nat_entries_count_;
  }
}

void MapTester::RemoveAllNatEntries(NodeManager &manager) {
  std::lock_guard nat_lock(manager.nat_tree_lock_);
  for (auto &nat_entry : manager.nat_cache_) {
    ZX_ASSERT((nat_entry).fbl::DoublyLinkedListable<NatEntry *>::InContainer());
    manager.clean_nat_list_.erase(nat_entry);
    ZX_ASSERT((nat_entry).fbl::WAVLTreeContainable<std::unique_ptr<NatEntry>>::InContainer());
    --manager.nat_entries_count_;
  }
  manager.nat_cache_.clear();
}

nid_t MapTester::ScanFreeNidList(NodeManager &manager, nid_t start) {
  // Check initial free list (BuildFreeNids)
  list_node_t *this_list;
  std::lock_guard nat_lock(manager.free_nid_list_lock_);
  list_for_every(&manager.free_nid_list_, this_list) {
    FreeNid *fi = containerof(this_list, FreeNid, list);
    ZX_ASSERT(fi->nid == start);
    ZX_ASSERT(fi->state == static_cast<int>(NidState::kNidNew));
    ++start;
  }
  return start;
}

void MapTester::GetCachedNatEntryBlockAddress(NodeManager &manager, nid_t nid, block_t &out) {
  fs::SharedLock nat_lock(manager.nat_tree_lock_);
  auto entry = manager.nat_cache_.find(nid);
  ASSERT_TRUE(entry != manager.nat_cache_.end());
  ASSERT_EQ(entry->GetNodeInfo().nid, nid);
  out = entry->GetBlockAddress();
}

void MapTester::SetCachedNatEntryBlockAddress(NodeManager &manager, nid_t nid, block_t address) {
  std::lock_guard nat_lock(manager.nat_tree_lock_);
  auto entry = manager.nat_cache_.find(nid);
  ASSERT_TRUE(entry != manager.nat_cache_.end());
  ASSERT_EQ(entry->GetNodeInfo().nid, nid);
  entry->SetBlockAddress(address);
}

void MapTester::SetCachedNatEntryCheckpointed(NodeManager &manager, nid_t nid) {
  std::lock_guard nat_lock(manager.nat_tree_lock_);
  auto entry = manager.nat_cache_.find(nid);
  ASSERT_TRUE(entry != manager.nat_cache_.end());
  ASSERT_EQ(entry->GetNodeInfo().nid, nid);
  entry->SetCheckpointed();
  ASSERT_TRUE(entry->IsCheckpointed());
}

zx_status_t MkfsTester::InitAndGetDeviceInfo(MkfsWorker &mkfs) {
  mkfs.InitGlobalParameters();
  return mkfs.GetDeviceInfo();
}

zx::status<std::unique_ptr<Bcache>> MkfsTester::FormatDevice(MkfsWorker &mkfs) {
  if (zx_status_t ret = mkfs.FormatDevice(); ret != ZX_OK)
    return zx::error(ret);
  return zx::ok(std::move(mkfs.bc_));
}

}  // namespace f2fs
