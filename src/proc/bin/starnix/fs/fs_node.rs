// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_zircon as zx;
use once_cell::sync::OnceCell;
use std::sync::{Arc, Weak};

use crate::device::DeviceMode;
use crate::fs::pipe::Pipe;
use crate::fs::socket::*;
use crate::fs::*;
use crate::lock::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use crate::task::*;
use crate::types::as_any::AsAny;
use crate::types::*;

pub struct FsNode {
    /// The FsNodeOps for this FsNode.
    ///
    /// The FsNodeOps are implemented by the individual file systems to provide
    /// specific behaviors for this FsNode.
    ops: Box<dyn FsNodeOps>,

    /// The FileSystem that owns this FsNode's tree.
    fs: Weak<FileSystem>,

    /// The inode number for this FsNode.
    pub inode_num: ino_t,

    /// The pipe located at this node, if any.
    ///
    /// Used if, and only if, the node has a mode of FileMode::IFIFO.
    fifo: Option<Arc<Mutex<Pipe>>>,

    /// The socket located at this node, if any.
    ///
    /// Used if, and only if, the node has a mode of FileMode::IFSOCK.
    ///
    /// The `OnceCell` is initialized when a new socket node is created:
    ///   - in `Socket::new` (e.g., from `sys_socket`)
    ///   - in `sys_bind`, before the node is given a name (i.e., before it could be accessed by
    ///     others)
    socket: OnceCell<SocketHandle>,

    /// Mutable informationa about this node.
    ///
    /// This data is used to populate the stat_t structure.
    info: RwLock<FsNodeInfo>,

    /// A RwLock to synchronize append operations for this node.
    ///
    /// FileObjects writing with O_APPEND should grab a write() lock on this
    /// field to ensure they operate sequentially. FileObjects writing without
    /// O_APPEND should grab read() lock so that they can operate in parallel.
    pub append_lock: RwLock<()>,
}

pub type FsNodeHandle = Arc<FsNode>;

pub struct FsNodeInfo {
    pub mode: FileMode,
    pub size: usize,
    pub storage_size: usize,
    pub blksize: i64,
    pub uid: uid_t,
    pub gid: gid_t,
    pub link_count: u64,
    pub time_create: zx::Time,
    pub time_access: zx::Time,
    pub time_modify: zx::Time,
    pub dev: DeviceType,
    pub rdev: DeviceType,
}

/// st_blksize is measured in units of 512 bytes.
const DEFAULT_BYTES_PER_BLOCK: i64 = 512;

impl Default for FsNodeInfo {
    fn default() -> Self {
        FsNodeInfo {
            mode: Default::default(),
            size: Default::default(),
            storage_size: Default::default(),
            blksize: DEFAULT_BYTES_PER_BLOCK,
            uid: Default::default(),
            gid: Default::default(),
            link_count: Default::default(),
            time_create: Default::default(),
            time_access: Default::default(),
            time_modify: Default::default(),
            dev: Default::default(),
            rdev: Default::default(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnlinkKind {
    /// Unlink a directory.
    Directory,

    /// Unlink a non-directory.
    NonDirectory,
}

pub enum SymlinkTarget {
    Path(FsString),
    Node(NamespaceNode),
}

#[derive(PartialEq, Eq)]
pub enum XattrOp {
    /// Set the value of the extended attribute regardless of whether it exists.
    Set,
    /// Create a new extended attribute. Fail if it already exists.
    Create,
    /// Replace the value of the extended attribute. Fail if it doesn't exist.
    Replace,
}

pub trait FsNodeOps: Send + Sync + AsAny {
    /// Open a FileObject for this node.
    ///
    /// The returned FileOps will be used to create a FileObject, which might
    /// be assigned an FdNumber.
    fn open(&self, node: &FsNode, flags: OpenFlags) -> Result<Box<dyn FileOps>, Errno>;

    /// Find an existing child node and populate the child parameter. Return the node.
    ///
    /// The child parameter is an empty node. Operations other than initialize may panic before
    /// initialize is called.
    fn lookup(&self, _node: &FsNode, _name: &FsStr) -> Result<FsNodeHandle, Errno> {
        error!(ENOTDIR)
    }

    /// Create and return the given child node.
    ///
    /// The mode field of the FsNodeInfo indicates what kind of child to
    /// create.
    ///
    /// This function is never called with FileMode::IFDIR. The mkdir function
    /// is used to create directories instead.
    fn mknod(&self, _node: &FsNode, _name: &FsStr, _mode: FileMode) -> Result<FsNodeHandle, Errno> {
        error!(ENOTDIR)
    }

    /// Create and return the given child node as a subdirectory.
    fn mkdir(&self, _node: &FsNode, _name: &FsStr) -> Result<FsNodeHandle, Errno> {
        error!(ENOTDIR)
    }

    /// Creates a symlink with the given `target` path.
    fn create_symlink(
        &self,
        _node: &FsNode,
        _name: &FsStr,
        _target: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        error!(ENOTDIR)
    }

    /// Reads the symlink from this node.
    fn readlink(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
    ) -> Result<SymlinkTarget, Errno> {
        error!(EINVAL)
    }

    /// Create a hard link with the given name to the given child.
    fn link(&self, _node: &FsNode, _name: &FsStr, _child: &FsNodeHandle) -> Result<(), Errno> {
        error!(EPERM)
    }

    /// Remove the child with the given name, if the child exists.
    ///
    /// The UnlinkKind parameter indicates whether the caller intends to unlink
    /// a directory or a non-directory child.
    fn unlink(&self, _node: &FsNode, _name: &FsStr, _child: &FsNodeHandle) -> Result<(), Errno> {
        error!(ENOTDIR)
    }

    /// Change the length of the file.
    fn truncate(&self, _node: &FsNode, _length: u64) -> Result<(), Errno> {
        error!(EINVAL)
    }

    /// Update node.info as needed.
    ///
    /// FsNode calls this method before converting the FsNodeInfo struct into
    /// the stat_t struct to give the file system a chance to update this data
    /// before it is used by clients.
    ///
    /// File systems that keep the FsNodeInfo up-to-date do not need to
    /// override this function.
    ///
    /// Return a reader lock on the updated information.
    fn update_info<'a>(&self, node: &'a FsNode) -> Result<RwLockReadGuard<'a, FsNodeInfo>, Errno> {
        Ok(node.info())
    }

    /// Get an extended attribute on the node.
    fn get_xattr(&self, _name: &FsStr) -> Result<FsString, Errno> {
        error!(ENOTSUP)
    }

    /// Set an extended attribute on the node.
    fn set_xattr(&self, _name: &FsStr, _value: &FsStr, _op: XattrOp) -> Result<(), Errno> {
        error!(ENOTSUP)
    }

    fn remove_xattr(&self, _name: &FsStr) -> Result<(), Errno> {
        error!(ENOTSUP)
    }

    fn list_xattrs(&self) -> Result<Vec<FsString>, Errno> {
        error!(ENOTSUP)
    }
}

/// Implements [`FsNodeOps`] methods in a way that makes sense for symlinks.
/// You must implement [`FsNodeOps::readlink`].
macro_rules! fs_node_impl_symlink {
    () => {
        fn open(
            &self,
            _node: &crate::fs::FsNode,
            _flags: crate::types::OpenFlags,
        ) -> Result<Box<dyn crate::fs::FileOps>, crate::types::Errno> {
            unreachable!("Symlink nodes cannot be opened.");
        }
    };
}

/// Implements [`FsNodeOps::set_xattr`] by delegating to another [`FsNodeOps`]
/// object.
macro_rules! fs_node_impl_xattr_delegate {
    ($self:ident, $delegate:expr) => {
        fn get_xattr(
            &$self,
            name: &crate::fs::FsStr,
        ) -> Result<FsString, crate::types::Errno> {
            $delegate.get_xattr(name)
        }

        fn set_xattr(
            &$self,
            name: &crate::fs::FsStr,
            value: &crate::fs::FsStr,
            op: crate::fs::XattrOp,
        ) -> Result<(), crate::types::Errno> {
            $delegate.set_xattr(name, value, op)
        }

        fn remove_xattr(
            &$self,
            name: &crate::fs::FsStr,
        ) -> Result<(), crate::types::Errno> {
            $delegate.remove_xattr(name)
        }

        fn list_xattrs(
            &$self,
        ) -> Result<Vec<crate::fs::FsString>, crate::types::Errno> {
            $delegate.list_xattrs()
        }
    };
    ($delegate:expr) => { fs_node_impl_xattr_delegate(self, $delegate) };
}

// Public re-export of macros allows them to be used like regular rust items.
pub(crate) use fs_node_impl_symlink;
pub(crate) use fs_node_impl_xattr_delegate;

pub struct SpecialNode;

impl FsNodeOps for SpecialNode {
    fn open(&self, _node: &FsNode, _flags: OpenFlags) -> Result<Box<dyn FileOps>, Errno> {
        unreachable!("Special nodes cannot be opened.");
    }
}

impl FsNode {
    pub fn new_root(ops: impl FsNodeOps + 'static) -> FsNode {
        Self::new_internal(Box::new(ops), Weak::new(), 1, FileMode::IFDIR | FileMode::ALLOW_ALL)
    }

    pub fn new(
        ops: Box<dyn FsNodeOps>,
        fs: &FileSystemHandle,
        inode_num: ino_t,
        mode: FileMode,
    ) -> FsNodeHandle {
        Arc::new(Self::new_internal(ops, Arc::downgrade(fs), inode_num, mode))
    }

    fn new_internal(
        ops: Box<dyn FsNodeOps>,
        fs: Weak<FileSystem>,
        inode_num: ino_t,
        mode: FileMode,
    ) -> FsNode {
        let now = fuchsia_runtime::utc_time();
        let info = FsNodeInfo {
            mode,
            link_count: if mode.is_dir() { 2 } else { 1 },
            time_create: now,
            time_access: now,
            time_modify: now,
            ..Default::default()
        };
        Self {
            ops,
            fs,
            inode_num,
            fifo: if mode.is_fifo() { Some(Pipe::new()) } else { None },
            socket: OnceCell::new(),
            info: RwLock::new(info),
            append_lock: RwLock::new(()),
        }
    }

    pub fn fs(&self) -> FileSystemHandle {
        self.fs.upgrade().expect("FileSystem did not live long enough")
    }

    pub fn set_fs(&mut self, fs: &FileSystemHandle) {
        self.fs = Arc::downgrade(fs);
    }

    fn ops(&self) -> &dyn FsNodeOps {
        &*self.ops.as_ref()
    }

    /// Returns the `FsNode`'s `FsNodeOps` as a `&T`, or `None` if the downcast fails.
    pub fn downcast_ops<T>(&self) -> Option<&T>
    where
        T: 'static,
    {
        self.ops().as_any().downcast_ref::<T>()
    }

    pub fn open(
        &self,
        current_task: &CurrentTask,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        // If O_PATH is set, there is no need to create a real FileOps because
        // most file operations are disabled.
        if flags.contains(OpenFlags::PATH) {
            return Ok(Box::new(OPathOps::new()));
        }

        let (mode, rdev) = {
            // Don't hold the info lock while calling into open_device or self.ops().
            // TODO: The mode and rdev are immutable and shouldn't require a lock to read.
            let info = self.info();
            (info.mode, info.rdev)
        };

        match mode & FileMode::IFMT {
            FileMode::IFCHR => {
                current_task.kernel().open_device(current_task, self, flags, rdev, DeviceMode::Char)
            }
            FileMode::IFBLK => current_task.kernel().open_device(
                current_task,
                self,
                flags,
                rdev,
                DeviceMode::Block,
            ),
            FileMode::IFIFO => Ok(Pipe::open(self.fifo.as_ref().unwrap(), flags)),
            // UNIX domain sockets can't be opened.
            FileMode::IFSOCK => error!(ENXIO),
            _ => self.ops().open(self, flags),
        }
    }

    pub fn lookup(&self, name: &FsStr) -> Result<FsNodeHandle, Errno> {
        self.ops().lookup(self, name)
    }

    pub fn mknod(&self, name: &FsStr, mode: FileMode) -> Result<FsNodeHandle, Errno> {
        self.ops().mknod(self, name, mode)
    }

    pub fn mkdir(&self, name: &FsStr) -> Result<FsNodeHandle, Errno> {
        self.ops().mkdir(self, name)
    }

    pub fn create_symlink(&self, name: &FsStr, target: &FsStr) -> Result<FsNodeHandle, Errno> {
        self.ops().create_symlink(self, name, target)
    }

    pub fn readlink(&self, current_task: &CurrentTask) -> Result<SymlinkTarget, Errno> {
        let now = fuchsia_runtime::utc_time();
        self.info_write().time_access = now;
        self.ops().readlink(self, current_task)
    }

    pub fn link(&self, name: &FsStr, child: &FsNodeHandle) -> Result<(), Errno> {
        self.ops().link(self, name, child)
    }

    pub fn unlink(&self, name: &FsStr, child: &FsNodeHandle) -> Result<(), Errno> {
        self.ops().unlink(self, name, child)
    }

    pub fn truncate(&self, length: u64) -> Result<(), Errno> {
        self.ops().truncate(self, length)
    }

    /// Associates the provided socket with this file node.
    ///
    /// `set_socket` must be called before it is possible to look up `self`, since user space should
    ///  not be able to look up this node and find the socket missing.
    ///
    /// Note that it is a fatal error to call this method if a socket has already been bound for
    /// this node.
    ///
    /// # Parameters
    /// - `socket`: The socket to store in this file node.
    pub fn set_socket(&self, socket: SocketHandle) {
        assert!(self.socket.set(socket).is_ok());
    }

    /// Returns the socket associated with this node, if such a socket exists.
    pub fn socket(&self) -> Option<&SocketHandle> {
        self.socket.get()
    }

    /// Set the permissions on this FsNode to the given values.
    ///
    /// Does not change the IFMT of the node.
    pub fn chmod(&self, mode: FileMode) {
        let mut info = self.info_write();
        info.mode = (info.mode & !FileMode::PERMISSIONS) | (mode & FileMode::PERMISSIONS);
    }

    /// Sets the owner and/or group on this FsNode.
    pub fn chown(&self, owner: Option<uid_t>, group: Option<gid_t>) {
        let mut info = self.info_write();
        if let Some(owner) = owner {
            info.uid = owner;
        }
        if let Some(group) = group {
            info.gid = group;
        }
    }

    /// Whether this node is a directory.
    pub fn is_dir(&self) -> bool {
        self.info().mode.is_dir()
    }

    /// Whether this node is a socket.
    pub fn is_sock(&self) -> bool {
        self.info().mode.is_sock()
    }

    /// Whether this node is a symbolic link.
    pub fn is_lnk(&self) -> bool {
        self.info().mode.is_lnk()
    }

    /// Update the access and modify time for this node to now.
    pub fn touch(&self) {
        let now = fuchsia_runtime::utc_time();
        let mut info = self.info_write();
        info.time_access = now;
        info.time_modify = now;
    }

    pub fn stat(&self) -> Result<stat_t, Errno> {
        let info = self.ops().update_info(self)?;
        Ok(stat_t {
            st_ino: self.inode_num,
            st_mode: info.mode.bits(),
            st_size: info.size as off_t,
            st_blocks: info.storage_size as i64 / info.blksize,
            st_nlink: info.link_count,
            st_uid: info.uid,
            st_gid: info.gid,
            st_ctim: timespec_from_time(info.time_create),
            st_mtim: timespec_from_time(info.time_modify),
            st_atim: timespec_from_time(info.time_access),
            st_dev: info.dev.bits(),
            st_rdev: info.rdev.bits(),
            st_blksize: info.blksize,
            ..Default::default()
        })
    }

    pub fn get_xattr(&self, name: &FsStr) -> Result<FsString, Errno> {
        self.ops().get_xattr(name)
    }

    pub fn set_xattr(&self, name: &FsStr, value: &FsStr, op: XattrOp) -> Result<(), Errno> {
        self.ops().set_xattr(name, value, op)
    }

    pub fn remove_xattr(&self, name: &FsStr) -> Result<(), Errno> {
        self.ops().remove_xattr(name)
    }

    pub fn list_xattrs(&self) -> Result<Vec<FsString>, Errno> {
        self.ops().list_xattrs()
    }

    pub fn info(&self) -> RwLockReadGuard<'_, FsNodeInfo> {
        self.info.read()
    }
    pub fn info_write(&self) -> RwLockWriteGuard<'_, FsNodeInfo> {
        self.info.write()
    }
}

impl Drop for FsNode {
    fn drop(&mut self) {
        if let Some(fs) = self.fs.upgrade() {
            fs.remove_node(self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    #[::fuchsia::test]
    fn open_device_file() {
        let (_kernel, current_task) = create_kernel_and_task();

        // Create a device file that points to the `zero` device (which is automatically
        // registered in the kernel).
        current_task
            .fs
            .root
            .create_node(b"zero", FileMode::IFCHR | FileMode::from_bits(0o666), DeviceType::ZERO)
            .expect("create_node");

        // Prepare the user buffer with some values other than the expected content (non-zero).
        const CONTENT_LEN: usize = 10;
        let address = map_memory(&current_task, UserAddress::default(), CONTENT_LEN as u64);
        current_task.mm.write_memory(address, &[0xff; CONTENT_LEN]).expect("write memory");

        // Read from the zero device.
        let device_file =
            current_task.open_file(b"zero", OpenFlags::RDONLY).expect("open device file");
        device_file
            .read(&current_task, &[UserBuffer { address, length: CONTENT_LEN }])
            .expect("read from zero");

        // Assert the contents.
        let content = &mut [0xff; CONTENT_LEN];
        current_task.mm.read_memory(address, content).expect("read memory");
        assert_eq!(&[0; CONTENT_LEN], content);
    }

    #[::fuchsia::test]
    fn node_info_is_reflected_in_stat() {
        let (_kernel, current_task) = create_kernel_and_task();

        // Create a node.
        let node = &current_task
            .fs
            .root
            .create_node(b"zero", FileMode::IFCHR, DeviceType::ZERO)
            .expect("create_node")
            .entry
            .node;
        {
            let mut info = node.info_write();
            info.mode = FileMode::IFSOCK;
            info.size = 1;
            info.storage_size = 8;
            info.blksize = 4;
            info.uid = 9;
            info.gid = 10;
            info.link_count = 11;
            info.time_create = zx::Time::from_nanos(1);
            info.time_access = zx::Time::from_nanos(2);
            info.time_modify = zx::Time::from_nanos(3);
            info.dev = DeviceType::new(12, 12);
            info.rdev = DeviceType::new(13, 13);
        }
        let stat = node.stat().expect("stat");

        assert_eq!(stat.st_mode, FileMode::IFSOCK.bits());
        assert_eq!(stat.st_size, 1);
        assert_eq!(stat.st_blksize, 4);
        assert_eq!(stat.st_blocks, 2);
        assert_eq!(stat.st_uid, 9);
        assert_eq!(stat.st_gid, 10);
        assert_eq!(stat.st_nlink, 11);
        assert_eq!(time_from_timespec(stat.st_ctim).expect("ctim"), zx::Time::from_nanos(1));
        assert_eq!(time_from_timespec(stat.st_atim).expect("atim"), zx::Time::from_nanos(2));
        assert_eq!(time_from_timespec(stat.st_mtim).expect("mtim"), zx::Time::from_nanos(3));
        assert_eq!(stat.st_dev, DeviceType::new(12, 12).bits());
        assert_eq!(stat.st_rdev, DeviceType::new(13, 13).bits());
    }
}
