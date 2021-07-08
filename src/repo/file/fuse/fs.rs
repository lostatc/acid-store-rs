/*
 * Copyright 2019-2021 Wren Powell
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::collections::{hash_map::Entry as HashMapEntry, HashMap};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use fuse::{
    FileAttr, FileType as FuseFileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, ReplyXattr, Request,
};
use nix::fcntl::OFlag;
use nix::libc;
use nix::sys::stat::{self, SFlag};
use once_cell::sync::Lazy;
use relative_path::RelativePath;
use time::Timespec;

use super::handle::{DirectoryEntry, DirectoryHandle, FileHandle, HandleState, HandleTable};
use super::inode::InodeTable;
use super::object::ObjectTable;

use crate::repo::file::{
    entry::{Entry, FileType},
    metadata::UnixMetadata,
    repository::{FileRepo, EMPTY_PATH},
    special::UnixSpecialType,
};
use crate::repo::Commit;

/// The block size used to calculate `st_blocks`.
const BLOCK_SIZE: u64 = 512;

/// The default TTL value to use in FUSE replies.
///
/// Because the backing `FileRepo` can only be safely modified through the FUSE file system, while
/// it is mounted, we can set this to an arbitrarily large value.
const DEFAULT_TTL: Timespec = Timespec {
    sec: i64::MAX,
    nsec: i32::MAX,
};

/// The value of `st_rdev` value to use if the file is not a character or block device.
const NON_SPECIAL_RDEV: u32 = 0;

/// The default permissions bits for a directory.
const DEFAULT_DIR_MODE: u32 = 0o775;

/// The default permissions bits for a file.
const DEFAULT_FILE_MODE: u32 = 0o664;

/// The set of `open` flags which are not supported by this file system.
static UNSUPPORTED_OPEN_FLAGS: Lazy<OFlag> = Lazy::new(|| OFlag::O_DIRECT | OFlag::O_TMPFILE);

/// Handle a `crate::Result` in a FUSE method.
macro_rules! try_result {
    ($result:expr, $reply:expr) => {
        match $result {
            Ok(result) => result,
            Err(error) => {
                $reply.error(crate::Error::from(error).to_errno());
                return;
            }
        }
    };
}

/// Handle an `Option` in a FUSE method.
macro_rules! try_option {
    ($result:expr, $reply:expr, $error:expr) => {
        match $result {
            Some(result) => result,
            None => {
                $reply.error($error);
                return;
            }
        }
    };
}

impl crate::Error {
    /// Get the libc errno for this error.
    fn to_errno(&self) -> i32 {
        match self {
            crate::Error::AlreadyExists => libc::EEXIST,
            crate::Error::NotFound => libc::ENOENT,
            crate::Error::InvalidPath => libc::ENOENT,
            crate::Error::NotEmpty => libc::ENOTEMPTY,
            crate::Error::NotDirectory => libc::ENOTDIR,
            crate::Error::NotFile => libc::EISDIR,
            crate::Error::Io(error) => match error.raw_os_error() {
                Some(errno) => errno,
                None => libc::EIO,
            },
            _ => libc::EIO,
        }
    }
}

/// Convert the given `time` to a `SystemTime`.
fn to_system_time(time: Timespec) -> SystemTime {
    let duration = Duration::new(time.sec.abs() as u64, time.nsec.abs() as u32);
    if time.sec.is_positive() {
        SystemTime::UNIX_EPOCH + duration
    } else {
        SystemTime::UNIX_EPOCH - duration
    }
}

/// Convert the given `time` to a `Timespec`.
fn to_timespec(time: SystemTime) -> Timespec {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => Timespec {
            sec: duration.as_secs() as i64,
            nsec: duration.subsec_nanos() as i32,
        },
        Err(error) => Timespec {
            sec: -(error.duration().as_secs() as i64),
            nsec: -(error.duration().subsec_nanos() as i32),
        },
    }
}

impl Entry<UnixSpecialType, UnixMetadata> {
    /// Create a new `Entry` of the given `file_type` with default metadata.
    fn new(file_type: FileType<UnixSpecialType>, req: &Request) -> Self {
        let mut entry = Self {
            file_type,
            metadata: None,
        };
        entry.metadata = Some(entry.default_metadata(req));
        entry
    }

    /// The default `UnixMetadata` for an entry that has no metadata.
    fn default_metadata(&self, req: &Request) -> UnixMetadata {
        UnixMetadata {
            mode: if self.is_directory() {
                DEFAULT_DIR_MODE
            } else {
                DEFAULT_FILE_MODE
            },
            modified: SystemTime::now(),
            accessed: SystemTime::now(),
            user: req.uid(),
            group: req.gid(),
            attributes: HashMap::new(),
            acl: HashMap::new(),
        }
    }

    /// Return this entry's metadata or the default metadata if it's `None`.
    fn metadata_or_default(self, req: &Request) -> UnixMetadata {
        match self.metadata {
            Some(metadata) => metadata,
            None => self.default_metadata(req),
        }
    }
}

impl FileType<UnixSpecialType> {
    /// Convert this `FileType` to a `fuse`-compatible file type.
    pub fn to_file_type(&self) -> FuseFileType {
        match self {
            FileType::File => FuseFileType::RegularFile,
            FileType::Directory => FuseFileType::Directory,
            FileType::Special(UnixSpecialType::BlockDevice { .. }) => FuseFileType::BlockDevice,
            FileType::Special(UnixSpecialType::CharacterDevice { .. }) => FuseFileType::CharDevice,
            FileType::Special(UnixSpecialType::SymbolicLink { .. }) => FuseFileType::Symlink,
            FileType::Special(UnixSpecialType::NamedPipe { .. }) => FuseFileType::NamedPipe,
        }
    }
}

#[derive(Debug)]
pub struct FuseAdapter<'a> {
    /// The repository which contains the virtual file system.
    repo: &'a mut FileRepo<UnixSpecialType, UnixMetadata>,

    /// A table for allocating inodes.
    inodes: InodeTable,

    /// A table for allocating file handles.
    handles: HandleTable,

    /// A map of inodes to currently open file objects.
    objects: ObjectTable,
}

impl<'a> FuseAdapter<'a> {
    /// Create a new `FuseAdapter` from the given `repo`.
    pub fn new(
        repo: &'a mut FileRepo<UnixSpecialType, UnixMetadata>,
        root: &RelativePath,
    ) -> crate::Result<Self> {
        if root == *EMPTY_PATH {
            return Err(crate::Error::InvalidPath);
        }

        let mut inodes = InodeTable::new(root);

        for path in repo.walk(root)? {
            inodes.insert(path);
        }

        Ok(Self {
            repo,
            inodes,
            handles: HandleTable::new(),
            objects: ObjectTable::new(),
        })
    }

    /// Get the `FileAttr` for the `entry` with the given `inode`.
    fn entry_attr(
        &mut self,
        entry: &Entry<UnixSpecialType, UnixMetadata>,
        inode: u64,
        req: &Request,
    ) -> crate::Result<FileAttr> {
        let entry_path = self.inodes.path(inode).ok_or(crate::Error::NotFound)?;
        let default_metadata = entry.default_metadata(req);
        let metadata = entry.metadata.as_ref().unwrap_or(&default_metadata);

        let size = match &entry.file_type {
            FileType::File => self
                .objects
                .open_commit(inode, self.repo.open(entry_path).unwrap())?
                .size()
                .unwrap(),
            FileType::Directory => 0,
            FileType::Special(special) => match special {
                // The `st_size` of a symlink should be the length of the pathname it contains.
                UnixSpecialType::SymbolicLink { target } => target.as_os_str().len() as u64,
                _ => 0,
            },
        };

        Ok(FileAttr {
            ino: inode,
            size,
            blocks: size / BLOCK_SIZE,
            atime: to_timespec(metadata.accessed),
            mtime: to_timespec(metadata.modified),
            ctime: to_timespec(SystemTime::now()),
            crtime: to_timespec(SystemTime::now()),
            kind: match &entry.file_type {
                FileType::File => fuse::FileType::RegularFile,
                FileType::Directory => fuse::FileType::Directory,
                FileType::Special(special) => match special {
                    UnixSpecialType::SymbolicLink { .. } => fuse::FileType::Symlink,
                    UnixSpecialType::NamedPipe => fuse::FileType::NamedPipe,
                    UnixSpecialType::BlockDevice { .. } => fuse::FileType::BlockDevice,
                    UnixSpecialType::CharacterDevice { .. } => fuse::FileType::CharDevice,
                },
            },
            perm: metadata.mode as u16,
            nlink: 0,
            uid: metadata.user,
            gid: metadata.group,
            rdev: match &entry.file_type {
                FileType::Special(special) => match special {
                    UnixSpecialType::BlockDevice { major, minor } => {
                        stat::makedev(*major, *minor) as u32
                    }
                    UnixSpecialType::CharacterDevice { major, minor } => {
                        stat::makedev(*major, *minor) as u32
                    }
                    _ => NON_SPECIAL_RDEV,
                },
                _ => NON_SPECIAL_RDEV,
            },
            flags: 0,
        })
    }
}

impl<'a> Filesystem for FuseAdapter<'a> {
    fn lookup(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let file_name = try_option!(name.to_str(), reply, libc::ENOENT);
        let entry_path = try_option!(self.inodes.path(parent), reply, libc::ENOENT).join(file_name);
        let entry_inode = try_option!(self.inodes.inode(&entry_path), reply, libc::ENOENT);
        let entry = try_result!(self.repo.entry(&entry_path), reply);

        let attr = try_result!(self.entry_attr(&entry, entry_inode, req), reply);

        let generation = self.inodes.generation(entry_inode);

        reply.entry(&DEFAULT_TTL, &attr, generation);
    }

    fn getattr(&mut self, req: &Request, ino: u64, reply: ReplyAttr) {
        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);
        let entry = try_result!(self.repo.entry(&entry_path), reply);
        let attr = try_result!(self.entry_attr(&entry, ino, req), reply);

        reply.attr(&DEFAULT_TTL, &attr);
    }

    fn setattr(
        &mut self,
        req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<Timespec>,
        mtime: Option<Timespec>,
        _fh: Option<u64>,
        _crtime: Option<Timespec>,
        _chgtime: Option<Timespec>,
        _bkuptime: Option<Timespec>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT).to_owned();

        // If `size` is not `None`, that means we must truncate the file.
        if let Some(size) = size {
            let object = try_result!(
                self.objects
                    .open_commit(ino, self.repo.open(&entry_path).unwrap()),
                reply
            );
            try_result!(object.truncate(size), reply);
        }

        let mut entry = try_result!(self.repo.entry(&entry_path), reply);
        let default_metadata = entry.default_metadata(req);
        let metadata = entry.metadata.get_or_insert(default_metadata);

        if let Some(mode) = mode {
            metadata.mode = mode;
        }

        if let Some(uid) = uid {
            metadata.user = uid;
        }

        if let Some(gid) = gid {
            metadata.group = gid;
        }

        if let Some(atime) = atime {
            metadata.accessed = to_system_time(atime);
        }

        if let Some(mtime) = mtime {
            metadata.modified = to_system_time(mtime);
        }

        try_result!(
            self.repo.set_metadata(&entry_path, entry.metadata.clone()),
            reply
        );

        try_result!(self.repo.commit(), reply);

        let attr = try_result!(self.entry_attr(&entry, ino, req), reply);
        reply.attr(&DEFAULT_TTL, &attr);
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);
        let entry = try_result!(self.repo.entry(&entry_path), reply);
        match &entry.file_type {
            FileType::Special(UnixSpecialType::SymbolicLink { target }) => {
                reply.data(target.as_os_str().as_bytes());
            }
            _ => {
                reply.error(libc::EINVAL);
            }
        };
    }

    fn mknod(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let flags = SFlag::from_bits_truncate(mode);
        let file_name = try_option!(name.to_str(), reply, libc::EINVAL);
        let entry_path = try_option!(self.inodes.path(parent), reply, libc::ENOENT).join(file_name);

        let file_type = if flags.contains(SFlag::S_IFREG) {
            FileType::File
        } else if flags.contains(SFlag::S_IFCHR) {
            let major = stat::major(rdev as u64);
            let minor = stat::minor(rdev as u64);
            FileType::Special(UnixSpecialType::CharacterDevice { major, minor })
        } else if flags.contains(SFlag::S_IFBLK) {
            let major = stat::major(rdev as u64);
            let minor = stat::minor(rdev as u64);
            FileType::Special(UnixSpecialType::BlockDevice { major, minor })
        } else if flags.contains(SFlag::S_IFIFO) {
            FileType::Special(UnixSpecialType::NamedPipe)
        } else if flags.contains(SFlag::S_IFSOCK) {
            // Sockets aren't supported by `FileRepo`. `mknod(2)` specifies that `EPERM`
            // should be returned if the file system doesn't support the type of node being
            // requested.
            reply.error(libc::EPERM);
            return;
        } else {
            // Other file types aren't supported by `mknod`.
            reply.error(libc::EINVAL);
            return;
        };

        let mut entry = Entry::new(file_type, req);
        entry.metadata.as_mut().unwrap().mode = mode;

        try_result!(self.repo.create(&entry_path, &entry), reply);

        try_result!(self.repo.commit(), reply);

        let entry_inode = self.inodes.insert(entry_path);
        let attr = try_result!(self.entry_attr(&entry, entry_inode, req), reply);
        let generation = self.inodes.generation(entry_inode);

        reply.entry(&DEFAULT_TTL, &attr, generation);
    }

    fn mkdir(&mut self, req: &Request, parent: u64, name: &OsStr, mode: u32, reply: ReplyEntry) {
        let file_name = try_option!(name.to_str(), reply, libc::EINVAL);
        let entry_path = try_option!(self.inodes.path(parent), reply, libc::ENOENT).join(file_name);

        let mut entry = Entry::new(FileType::Directory, req);
        let metadata = entry.metadata.as_mut().unwrap();
        metadata.mode = mode;

        try_result!(self.repo.create(&entry_path, &entry), reply);

        try_result!(self.repo.commit(), reply);

        let entry_inode = self.inodes.insert(entry_path);
        let attr = try_result!(self.entry_attr(&entry, entry_inode, req), reply);
        let generation = self.inodes.generation(entry_inode);

        reply.entry(&DEFAULT_TTL, &attr, generation);
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let file_name = try_option!(name.to_str(), reply, libc::ENOENT);
        let entry_path = try_option!(self.inodes.path(parent), reply, libc::ENOENT).join(file_name);
        let entry_inode = try_option!(self.inodes.inode(&entry_path), reply, libc::ENOENT);

        if self.repo.is_directory(&entry_path) {
            reply.error(libc::EISDIR);
            return;
        }

        try_result!(self.repo.remove(&entry_path), reply);

        try_result!(self.repo.commit(), reply);

        self.inodes.remove(entry_inode);
        self.objects.close(entry_inode);

        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let file_name = try_option!(name.to_str(), reply, libc::ENOENT);
        let entry_path = try_option!(self.inodes.path(parent), reply, libc::ENOENT).join(file_name);
        let entry_inode = try_option!(self.inodes.inode(&entry_path), reply, libc::ENOENT);

        if !self.repo.is_directory(&entry_path) {
            reply.error(libc::ENOTDIR);
            return;
        }

        // `FileRepo::remove` method checks that the directory entry is empty.
        try_result!(self.repo.remove(&entry_path), reply);

        try_result!(self.repo.commit(), reply);

        self.inodes.remove(entry_inode);

        reply.ok();
    }

    fn symlink(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        link: &Path,
        reply: ReplyEntry,
    ) {
        let file_name = try_option!(name.to_str(), reply, libc::EINVAL);
        let entry_path = try_option!(self.inodes.path(parent), reply, libc::ENOENT).join(file_name);

        let entry = Entry::new(
            FileType::Special(UnixSpecialType::SymbolicLink {
                target: link.to_owned(),
            }),
            req,
        );

        try_result!(self.repo.create(&entry_path, &entry), reply);

        try_result!(self.repo.commit(), reply);

        let entry_inode = self.inodes.insert(entry_path);
        let attr = try_result!(self.entry_attr(&entry, entry_inode, req), reply);
        let generation = self.inodes.generation(entry_inode);

        reply.entry(&DEFAULT_TTL, &attr, generation);
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEmpty,
    ) {
        let source_name = try_option!(name.to_str(), reply, libc::ENOENT);
        let source_path =
            try_option!(self.inodes.path(parent), reply, libc::ENOENT).join(source_name);
        let dest_name = try_option!(newname.to_str(), reply, libc::EINVAL);
        let dest_path =
            try_option!(self.inodes.path(newparent), reply, libc::ENOENT).join(dest_name);

        if !self.repo.exists(&source_path) {
            reply.error(libc::ENOENT);
            return;
        }

        // We cannot make a directory a subdirectory of itself.
        if dest_path.starts_with(&source_path) {
            reply.error(libc::EINVAL);
            return;
        }

        // Check if the parent of the destination path is not a directory.
        if !self.repo.is_directory(&dest_path.parent().unwrap()) {
            reply.error(libc::ENOTDIR);
            return;
        }

        // Remove the destination path unless it is a non-empty directory.
        if let Err(error @ crate::Error::NotEmpty) = self.repo.remove(&dest_path) {
            reply.error(error.to_errno());
            return;
        }

        // We've already checked all the possible error conditions.
        self.repo.copy(&source_path, &dest_path).ok();

        try_result!(self.repo.commit(), reply);

        reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: u32, reply: ReplyOpen) {
        let flags = OFlag::from_bits_truncate(flags as i32);

        if flags.intersects(*UNSUPPORTED_OPEN_FLAGS) {
            reply.error(libc::ENOTSUP);
            return;
        }

        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);

        if !self.repo.is_file(&entry_path) {
            reply.error(libc::ENOTSUP);
            return;
        }

        let state = HandleState::File(FileHandle { flags, position: 0 });
        let fh = self.handles.open(state);

        reply.opened(fh, 0);
    }

    fn read(&mut self, req: &Request, ino: u64, fh: u64, offset: i64, size: u32, reply: ReplyData) {
        // Technically, on Unix systems, a file should still be accessible via its file descriptor
        // once it's been unlinked. Because this isn't how repositories work, we will return `EBADF`
        // if the user tries to read from a file which has been unlinked since it was opened.
        let entry_path = match self.inodes.path(ino) {
            Some(path) => path.to_owned(),
            None => {
                self.handles.close(fh);
                reply.error(libc::EBADF);
                return;
            }
        };

        let state = match self.handles.state_mut(fh) {
            None => {
                reply.error(libc::EBADF);
                return;
            }
            Some(HandleState::Directory(_)) => {
                reply.error(libc::EISDIR);
                return;
            }
            Some(HandleState::File(state)) => state,
        };

        let mut buffer = vec![0u8; size as usize];
        let mut total_bytes_read = 0;

        {
            let object = try_result!(
                self.objects
                    .open_commit(ino, self.repo.open(&entry_path).unwrap()),
                reply
            );
            try_result!(object.seek(SeekFrom::Start(offset as u64)), reply);

            // `Filesystem::read` should read the exact number of bytes requested except on EOF or error.
            let mut bytes_read;
            loop {
                bytes_read = try_result!(
                    object.read(&mut buffer[total_bytes_read..size as usize]),
                    reply
                );
                total_bytes_read += bytes_read;

                if bytes_read == 0 {
                    // Either the object has reached EOF or we've already read `size` bytes from it.
                    break;
                }
            }
        }

        state.position = offset as u64 + total_bytes_read as u64;

        // Update the file's `st_atime` unless the `O_NOATIME` flag was passed.
        if !state.flags.contains(OFlag::O_NOATIME) {
            let mut metadata =
                try_result!(self.repo.entry(&entry_path), reply).metadata_or_default(req);
            metadata.accessed = SystemTime::now();
            try_result!(self.repo.set_metadata(&entry_path, Some(metadata)), reply);
        }

        reply.data(&buffer[..total_bytes_read]);
    }

    fn write(
        &mut self,
        req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _flags: u32,
        reply: ReplyWrite,
    ) {
        // Technically, on Unix systems, a file should still be accessible via its file descriptor
        // once it's been unlinked. Because this isn't how repositories work, we will return `EBADF`
        // if the user tries to read from a file which has been unlinked since it was opened.
        let entry_path = match self.inodes.path(ino) {
            Some(path) => path.to_owned(),
            None => {
                self.handles.close(fh);
                reply.error(libc::EBADF);
                return;
            }
        };

        let state = match self.handles.state_mut(fh) {
            None => {
                reply.error(libc::EBADF);
                return;
            }
            Some(HandleState::Directory(_)) => {
                reply.error(libc::EISDIR);
                return;
            }
            Some(HandleState::File(state)) => state,
        };

        let mut metadata =
            try_result!(self.repo.entry(&entry_path), reply).metadata_or_default(req);

        let bytes_written = {
            let object = if state.flags.contains(OFlag::O_APPEND) {
                let object = try_result!(
                    self.objects
                        .open_commit(ino, self.repo.open(&entry_path).unwrap()),
                    reply
                );
                try_result!(object.seek(SeekFrom::End(0)), reply);
                object
            } else if offset as u64 == state.position {
                // Because the offset is the same as the previous offset, we don't need to seek and
                // therefore don't need to commit changes.
                self.objects.open(ino, self.repo.open(&entry_path).unwrap())
            } else {
                let object = try_result!(
                    self.objects
                        .open_commit(ino, self.repo.open(&entry_path).unwrap()),
                    reply
                );
                try_result!(object.seek(SeekFrom::Start(offset as u64)), reply);
                object
            };

            try_result!(object.write(data), reply)
        };

        state.position = offset as u64 + bytes_written as u64;

        // After this point, we need to be more careful about error handling. Because bytes have
        // been written to the object, if an error occurs, we need to drop the `Object` to discard
        // any uncommitted changes before returning so that bytes will only have been written to the
        // object if this method returns successfully.

        // Update the `st_atime` and `st_mtime` for the entry.
        metadata.accessed = SystemTime::now();
        metadata.modified = SystemTime::now();
        if let Err(error) = self.repo.set_metadata(&entry_path, Some(metadata)) {
            self.objects.close(ino);
            reply.error(error.to_errno());
            return;
        }

        // If the `O_SYNC` or `O_DSYNC` flags were passed, we need to commit changes to the object
        // *and* commit changes to the repository after each write.
        if state.flags.intersects(OFlag::O_SYNC | OFlag::O_DSYNC) {
            if let Err(error) = self.objects.commit(ino) {
                self.objects.close(ino);
                reply.error(error.to_errno());
                return;
            }

            if let Err(error) = self.repo.commit() {
                self.objects.close(ino);
                reply.error(error.to_errno());
                return;
            }
        }

        reply.written(bytes_written as u32);
    }

    fn flush(&mut self, _req: &Request, ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        try_result!(self.objects.commit(ino), reply);
        reply.ok()
    }

    fn release(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.close(fh);
        self.objects.close(ino);
        reply.ok()
    }

    fn fsync(&mut self, _req: &Request, ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        try_result!(self.objects.commit(ino), reply);
        try_result!(self.repo.commit(), reply);
        reply.ok();
    }

    fn opendir(&mut self, _req: &Request, ino: u64, _flags: u32, reply: ReplyOpen) {
        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);

        if !self.repo.is_directory(entry_path) {
            reply.error(libc::ENOTDIR);
            return;
        }

        let mut entries = Vec::new();
        for child_path in try_result!(self.repo.list(entry_path), reply) {
            let file_name = child_path.file_name().unwrap().to_string();
            let inode = self.inodes.inode(&child_path).unwrap();
            let file_type = try_result!(self.repo.entry(&child_path), reply)
                .file_type
                .to_file_type();
            entries.push(DirectoryEntry {
                file_name,
                file_type,
                inode,
            })
        }

        let state = HandleState::Directory(DirectoryHandle { entries });
        let fh = self.handles.open(state);

        reply.opened(fh, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.handles.state(fh) {
            None => {
                reply.error(libc::EBADF);
                return;
            }
            Some(HandleState::File(_)) => {
                reply.error(libc::ENOTDIR);
                return;
            }
            Some(HandleState::Directory(DirectoryHandle { entries })) => entries,
        };

        for (i, dir_entry) in entries[offset as usize..].iter().enumerate() {
            if reply.add(
                dir_entry.inode,
                (i + 1) as i64,
                dir_entry.file_type,
                &dir_entry.file_name,
            ) {
                break;
            }
        }

        reply.ok();
    }

    fn releasedir(&mut self, _req: &Request, _ino: u64, fh: u64, _flags: u32, reply: ReplyEmpty) {
        self.handles.close(fh);
        reply.ok()
    }

    fn fsyncdir(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        try_result!(self.repo.commit(), reply);
        reply.ok();
    }

    fn setxattr(
        &mut self,
        req: &Request,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        flags: u32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let attr_name = try_option!(name.to_str(), reply, libc::EINVAL).to_owned();

        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);
        let mut metadata =
            try_result!(self.repo.entry(&entry_path), reply).metadata_or_default(req);

        if flags == 0 {
            metadata.attributes.insert(attr_name, value.to_vec());
        } else if flags == libc::XATTR_CREATE as u32 {
            match metadata.attributes.entry(attr_name) {
                HashMapEntry::Occupied(_) => {
                    reply.error(libc::EEXIST);
                    return;
                }
                HashMapEntry::Vacant(entry) => {
                    entry.insert(value.to_vec());
                }
            }
        } else if flags == libc::XATTR_REPLACE as u32 {
            match metadata.attributes.entry(attr_name) {
                HashMapEntry::Occupied(mut entry) => {
                    entry.insert(value.to_vec());
                }
                HashMapEntry::Vacant(_) => {
                    reply.error(libc::ENODATA);
                    return;
                }
            }
        } else {
            reply.error(libc::EINVAL);
            return;
        }

        try_result!(self.repo.set_metadata(entry_path, Some(metadata)), reply);

        try_result!(self.repo.commit(), reply);

        reply.ok();
    }

    fn getxattr(&mut self, req: &Request, ino: u64, name: &OsStr, size: u32, reply: ReplyXattr) {
        let attr_name = try_option!(name.to_str(), reply, libc::ENODATA).to_owned();

        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);
        let metadata = try_result!(self.repo.entry(&entry_path), reply).metadata_or_default(req);

        let attr_value = try_option!(metadata.attributes.get(&attr_name), reply, libc::ENODATA);

        if size == 0 {
            reply.size(attr_value.len() as u32);
            return;
        }

        if attr_value.len() > size as usize {
            reply.error(libc::ERANGE);
            return;
        }

        reply.data(attr_value.as_slice());
    }

    fn listxattr(&mut self, req: &Request, ino: u64, size: u32, reply: ReplyXattr) {
        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);
        let metadata = try_result!(self.repo.entry(&entry_path), reply).metadata_or_default(req);

        // Construct a byte string of null-terminated attribute names.
        let mut attr_names = Vec::new();
        for attr_name in metadata.attributes.keys() {
            attr_names.extend_from_slice(attr_name.as_bytes());
            attr_names.push(0u8);
        }

        if size == 0 {
            reply.size(attr_names.len() as u32);
            return;
        }

        if attr_names.len() > size as usize {
            reply.error(libc::ERANGE);
            return;
        }

        reply.data(attr_names.as_slice());
    }

    fn removexattr(&mut self, req: &Request, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        let attr_name = try_option!(name.to_str(), reply, libc::ENODATA).to_owned();

        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);
        let mut metadata =
            try_result!(self.repo.entry(&entry_path), reply).metadata_or_default(req);

        metadata.attributes.remove(&attr_name);

        try_result!(self.repo.set_metadata(entry_path, Some(metadata)), reply);

        try_result!(self.repo.commit(), reply);

        reply.ok();
    }
}
