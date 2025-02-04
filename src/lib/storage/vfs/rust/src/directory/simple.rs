// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This is an implementation of "simple" pseudo directories.
//! Use [`mod@crate::directory::immutable::simple`]
//! to construct actual instances.  See [`Simple`] for details.

#[allow(unused_imports)]
use crate::{
    common::{rights_to_posix_mode_bits, send_on_open_with_error},
    directory::{
        connection::io1::DerivedConnection,
        dirents_sink,
        entry::{DirectoryEntry, EntryInfo},
        entry_container::{Directory, DirectoryWatcher},
        helper::DirectlyMutable,
        immutable::connection::io1::ImmutableConnection,
        mutable::connection::io1::MutableConnection,
        traversal_position::TraversalPosition,
        watchers::{
            event_producers::{SingleNameEventProducer, StaticVecEventProducer},
            Watchers,
        },
    },
    execution_scope::ExecutionScope,
    filesystem::{simple::SimpleFilesystem, Filesystem},
    path::Path,
    MAX_NAME_LENGTH,
};

use {
    async_trait::async_trait,
    fidl::endpoints::ServerEnd,
    fidl_fuchsia_io as fio,
    fuchsia_zircon::Status,
    static_assertions::assert_eq_size,
    std::{
        boxed::Box,
        clone::Clone,
        collections::{
            btree_map::{self, Entry},
            BTreeMap,
        },
        iter,
        marker::PhantomData,
        ops::DerefMut,
        sync::{Arc, Mutex},
    },
};

/// An implementation of a "simple" pseudo directory.  This directory holds a set of entries,
/// allowing the server to add or remove entries via the
/// [`crate::directory::helper::DirectlyMutable::add_entry()`] and
/// [`crate::directory::helper::DirectlyMutable::remove_entry`] methods, and, depending on the
/// connection been used (see [`ImmutableConnection`] or [`MutableConnection`])
/// it may also allow the clients to modify the entries as well.  This is a common implementation
/// for [`mod@crate::directory::immutable::simple`] and [`mod@crate::directory::mutable::simple`].
pub struct Simple<Connection>
where
    Connection: DerivedConnection + 'static,
{
    inner: Mutex<Inner>,

    // The inode for this directory. This should either be unique within this VFS, or INO_UNKNOWN.
    inode: u64,

    _connection: PhantomData<Connection>,

    fs: SimpleFilesystem<Self>,

    not_found_handler: Mutex<Option<Box<dyn FnMut(&str) + Send + Sync + 'static>>>,
}

struct Inner {
    entries: BTreeMap<String, Arc<dyn DirectoryEntry>>,

    watchers: Watchers,
}

impl<Connection> Simple<Connection>
where
    Connection: DerivedConnection + 'static,
{
    pub(super) fn new(inode: u64) -> Arc<Self> {
        Arc::new(Simple {
            inner: Mutex::new(Inner { entries: BTreeMap::new(), watchers: Watchers::new() }),
            _connection: PhantomData,
            inode,
            fs: SimpleFilesystem::new(),
            not_found_handler: Mutex::new(None),
        })
    }

    fn get_or_insert_entry(
        self: Arc<Self>,
        scope: ExecutionScope,
        flags: fio::OpenFlags,
        mode: u32,
        name: &str,
        path: &Path,
    ) -> Result<Arc<dyn DirectoryEntry>, Status> {
        let mut this = self.inner.lock().unwrap();

        match this.entries.get(name) {
            Some(entry) => {
                if flags.intersects(fio::OpenFlags::CREATE_IF_ABSENT) {
                    return Err(Status::ALREADY_EXISTS);
                }

                Ok(entry.clone())
            }
            None => {
                let entry = Connection::entry_not_found(
                    scope.clone(),
                    self.clone(),
                    flags,
                    mode,
                    name,
                    path,
                )?;

                let _ = this.entries.insert(name.to_string(), entry.clone());
                Ok(entry)
            }
        }
    }
    /// Registers a given function to be used when an item that is not present is opened. Typically
    /// used for logging.
    pub fn set_not_found_handler(
        self: Arc<Self>,
        handler: Box<dyn FnMut(&str) + Send + Sync + 'static>,
    ) {
        let mut this = self.not_found_handler.lock().unwrap();
        this.replace(handler);
    }

    /// Returns the entry identified by `name`.
    pub fn get_entry(&self, name: &str) -> Result<Arc<dyn DirectoryEntry>, Status> {
        assert_eq_size!(u64, usize);
        if name.len() as u64 > MAX_NAME_LENGTH {
            return Err(Status::INVALID_ARGS);
        }

        let this = self.inner.lock().unwrap();
        match this.entries.get(name) {
            Some(entry) => Ok(entry.clone()),
            None => Err(Status::NOT_FOUND),
        }
    }
}

impl<Connection> DirectoryEntry for Simple<Connection>
where
    Connection: DerivedConnection + 'static,
{
    fn open(
        self: Arc<Self>,
        scope: ExecutionScope,
        flags: fio::OpenFlags,
        mode: u32,
        mut path: Path,
        server_end: ServerEnd<fio::NodeMarker>,
    ) {
        // See if the path has a next segment, if so we want to traverse down
        // the directory. Otherwise we've arrived at the right directory.
        let (name, path_ref) = match path.next_with_ref() {
            (path_ref, Some(name)) => (name, path_ref),
            (_, None) => {
                if Connection::mutable() {
                    MutableConnection::create_connection(scope, self, flags, server_end);
                } else {
                    ImmutableConnection::create_connection(scope, self, flags, server_end);
                }
                return;
            }
        };

        // Create copies so if this fails to open we can call the not found handler
        let ref_copy = self.clone();
        let name_copy = name.to_string();

        // Do not hold the mutex more than necessary and the Mutex is not re-entrant.  So we need to
        // make sure to release the lock before we call `open()` is it may turn out to be a
        // recursive call, in case the directory contains itself directly or through a number of
        // other directories.  `get_entry` is responsible for locking `self` and it will unlock it
        // before returning.
        let found = match self.get_or_insert_entry(scope.clone(), flags, mode, name, path_ref) {
            Err(status) => {
                send_on_open_with_error(flags, server_end, status);
                false
            }
            Ok(entry) => {
                entry.open(scope, flags, mode, path, server_end);
                true
            }
        };

        if !found {
            let mut handler = ref_copy.not_found_handler.lock().unwrap();
            if let Some(handler) = handler.as_mut() {
                handler(&name_copy);
            }
        }
    }

    fn entry_info(&self) -> EntryInfo {
        EntryInfo::new(self.inode, fio::DirentType::Directory)
    }
}

#[async_trait]
impl<Connection> Directory for Simple<Connection>
where
    Connection: DerivedConnection + 'static,
{
    async fn read_dirents<'a>(
        &'a self,
        pos: &'a TraversalPosition,
        sink: Box<dyn dirents_sink::Sink>,
    ) -> Result<(TraversalPosition, Box<dyn dirents_sink::Sealed>), Status> {
        use dirents_sink::AppendResult;

        let this = self.inner.lock().unwrap();

        let (mut sink, entries_iter) = match pos {
            TraversalPosition::Start => {
                match sink.append(&EntryInfo::new(self.inode, fio::DirentType::Directory), ".") {
                    AppendResult::Ok(sink) => {
                        // I wonder why, but rustc can not infer T in
                        //
                        //   pub fn range<T, R>(&self, range: R) -> Range<K, V>
                        //   where
                        //     K: Borrow<T>,
                        //     R: RangeBounds<T>,
                        //     T: Ord + ?Sized,
                        //
                        // for some reason here.  It says:
                        //
                        //   error[E0283]: type annotations required: cannot resolve `_: std::cmp::Ord`
                        //
                        // pointing to "range".  Same for two the other "range()" invocations
                        // below.
                        (sink, this.entries.range::<String, _>(..))
                    }
                    AppendResult::Sealed(sealed) => {
                        let new_pos = match this.entries.keys().next() {
                            None => TraversalPosition::End,
                            Some(first_name) => TraversalPosition::Name(first_name.clone()),
                        };
                        return Ok((new_pos, sealed.into()));
                    }
                }
            }

            TraversalPosition::Name(next_name) => {
                (sink, this.entries.range::<String, _>(next_name.to_owned()..))
            }

            TraversalPosition::Index(_) => unreachable!(),

            TraversalPosition::End => return Ok((TraversalPosition::End, sink.seal().into())),
        };

        for (name, entry) in entries_iter {
            match sink.append(&entry.entry_info(), &name) {
                AppendResult::Ok(new_sink) => sink = new_sink,
                AppendResult::Sealed(sealed) => {
                    return Ok((TraversalPosition::Name(name.clone()), sealed.into()));
                }
            }
        }

        Ok((TraversalPosition::End, sink.seal().into()))
    }

    fn register_watcher(
        self: Arc<Self>,
        scope: ExecutionScope,
        mask: fio::WatchMask,
        watcher: DirectoryWatcher,
    ) -> Result<(), Status> {
        let mut this = self.inner.lock().unwrap();

        let mut names = StaticVecEventProducer::existing({
            let entry_names = this.entries.keys();
            iter::once(&".".to_string()).chain(entry_names).cloned().collect()
        });

        let controller = this.watchers.add(scope, self.clone(), mask, watcher);
        controller.send_event(&mut names);
        controller.send_event(&mut SingleNameEventProducer::idle());

        Ok(())
    }

    fn unregister_watcher(self: Arc<Self>, key: usize) {
        let mut this = self.inner.lock().unwrap();
        this.watchers.remove(key);
    }

    async fn get_attrs(&self) -> Result<fio::NodeAttributes, Status> {
        Ok(fio::NodeAttributes {
            mode: fio::MODE_TYPE_DIRECTORY
                | rights_to_posix_mode_bits(
                    /*r*/ true,
                    /*w*/ Connection::mutable(),
                    /*x*/ true,
                ),
            id: self.inode,
            content_size: 0,
            storage_size: 0,
            link_count: 1,
            creation_time: 0,
            modification_time: 0,
        })
    }

    fn close(&self) -> Result<(), Status> {
        Ok(())
    }
}

impl<Connection> DirectlyMutable for Simple<Connection>
where
    Connection: DerivedConnection + 'static,
{
    fn add_entry_impl(
        &self,
        name: String,
        entry: Arc<dyn DirectoryEntry>,
        overwrite: bool,
    ) -> Result<(), Status> {
        assert_eq_size!(u64, usize);
        if name.len() as u64 > MAX_NAME_LENGTH {
            return Err(Status::INVALID_ARGS);
        }
        if name.contains('/') {
            return Err(Status::INVALID_ARGS);
        }

        let mut this = self.inner.lock().unwrap();

        if !overwrite && this.entries.contains_key(&name) {
            return Err(Status::ALREADY_EXISTS);
        }

        this.watchers.send_event(&mut SingleNameEventProducer::added(&name));

        let _ = this.entries.insert(name, entry);
        Ok(())
    }

    fn remove_entry_impl(
        &self,
        name: String,
        must_be_directory: bool,
    ) -> Result<Option<Arc<dyn DirectoryEntry>>, Status> {
        assert_eq_size!(u64, usize);
        if name.len() as u64 >= MAX_NAME_LENGTH {
            return Err(Status::INVALID_ARGS);
        }

        let mut this = self.inner.lock().unwrap();

        match this.entries.entry(name) {
            Entry::Vacant(_) => Ok(None),
            Entry::Occupied(occupied) => {
                if must_be_directory
                    && occupied.get().entry_info().type_() != fio::DirentType::Directory
                {
                    Err(Status::NOT_DIR)
                } else {
                    let (key, value) = occupied.remove_entry();
                    this.watchers.send_event(&mut SingleNameEventProducer::removed(&key));
                    Ok(Some(value))
                }
            }
        }
    }

    fn rename_from(
        &self,
        src: String,
        to: Box<dyn FnOnce(Arc<dyn DirectoryEntry>) -> Result<(), Status>>,
    ) -> Result<(), Status> {
        if src.len() as u64 >= MAX_NAME_LENGTH {
            return Err(Status::INVALID_ARGS);
        }

        let mut this = self.inner.lock().unwrap();

        let Inner { entries, watchers, .. } = this.deref_mut();

        let map_entry = match entries.entry(src.clone()) {
            btree_map::Entry::Vacant(_) => return Err(Status::NOT_FOUND),
            btree_map::Entry::Occupied(map_entry) => map_entry,
        };

        to(map_entry.get().clone())?;

        watchers.send_event(&mut SingleNameEventProducer::removed(&src));

        let _ = map_entry.remove();
        Ok(())
    }

    fn rename_to(
        &self,
        dst: String,
        from: Box<dyn FnOnce() -> Result<Arc<dyn DirectoryEntry>, Status>>,
    ) -> Result<(), Status> {
        if dst.len() as u64 >= MAX_NAME_LENGTH {
            return Err(Status::INVALID_ARGS);
        }

        let mut this = self.inner.lock().unwrap();

        let entry = from()?;

        this.watchers.send_event(&mut SingleNameEventProducer::added(&dst));

        let _ = this.entries.insert(dst, entry);
        Ok(())
    }

    fn rename_within(&self, src: String, dst: String) -> Result<(), Status> {
        if src.len() as u64 >= MAX_NAME_LENGTH || dst.len() as u64 >= MAX_NAME_LENGTH {
            return Err(Status::INVALID_ARGS);
        }

        let mut this = self.inner.lock().unwrap();

        // If src doesn't exist, don't do the other stuff.
        if !this.entries.contains_key(&src) {
            return Err(Status::NOT_FOUND);
        }
        // I assume we should send these events even when `src == dst`.  In practice, a
        // particular client may not be aware that the names match, but may still rely on the fact
        // that the events occur.
        //
        // Watcher protocol expects to produce messages that list only one type of event.  So
        // we will send two independent event messages, each with one name.
        this.watchers.send_event(&mut SingleNameEventProducer::removed(&src));
        this.watchers.send_event(&mut SingleNameEventProducer::added(&dst));

        // We acquire the lock first, as in case `src != dst`, we want to make sure that the
        // recipients of these events can not see the directory in the state before the update.  I
        // assume that `src == dst` is unlikely case, and for the sake of reduction of code
        // duplication we can lock and then immediately unlock.
        //
        // It also provides sequencing for the watchers, as they will always receive `removed`
        // followed by `added`, and there will be no interleaving event in-between.  I think it is
        // not super important, as the watchers should probably be prepared to deal with all kinds
        // of sequences anyways.
        if src == dst {
            return Ok(());
        }

        let entry = match this.entries.remove(&src) {
            // This is truly surprising since this was checked previously, but
            // we leave this in place instead of doing `unwrap` on the chance
            // the earlier check is carelessly removed.
            None => return Err(Status::NOT_FOUND),
            Some(entry) => entry,
        };

        let _ = this.entries.insert(dst, entry);
        Ok(())
    }

    fn get_filesystem(&self) -> &dyn Filesystem {
        &self.fs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::vmo::read_only_static;

    #[test]
    fn name_with_path_separator() {
        let dir = crate::directory::mutable::simple();
        let status = dir
            .add_entry("path/with/separators", read_only_static(b"test"))
            .expect_err("add entry with path separator should fail");
        assert_eq!(status, Status::INVALID_ARGS);
        assert_eq!(
            dir.add_entry("path_without_separators", read_only_static(b"test")),
            Ok(()),
            "add entry with valid filename should succeed"
        );
    }
}
