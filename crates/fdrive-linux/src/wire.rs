use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use crate::adapter::Adapter;
use fdrive_core::path::RelPath;

const TTL: Duration = Duration::from_secs(60);
const ROOT: u64 = 1;

struct InodeTable {
    paths: HashMap<u64, RelPath>,
    inos: HashMap<RelPath, u64>,
    next_ino: u64,
}

pub struct MountFs {
    rt: tokio::runtime::Handle,
    wire: Arc<Wire>,
}

struct Wire {
    adapter: Arc<Adapter>,
    inodes: Mutex<InodeTable>,
    uid: u32,
    gid: u32,
}

impl Filesystem for MountFs {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        let _ = config.add_capabilities(fuser::InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(path) = self.wire.child(parent.0, name) else {
            return reply.error(Errno::ENOENT);
        };
        self.go(move |wire| match wire.attr(&path) {
            Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
            None => reply.error(Errno::ENOENT),
        });
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.wire.path(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        self.go(move |wire| match wire.attr(&path) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.wire.path(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        self.go(move |wire| {
            if let Some(size) = size {
                log::debug!("truncate path={path} size={size}");
                if let Err(err) = wire.adapter.truncate(&path, size) {
                    return reply.error(Errno::from(err));
                }
            }
            match wire.attr(&path) {
                Some(attr) => reply.attr(&TTL, &attr),
                None => reply.error(Errno::ENOENT),
            }
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(dir) = self.wire.path(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        self.go(move |wire| {
            log::debug!("ls path={dir} offset={offset}");
            let listing = match wire.adapter.ls(&dir) {
                Ok(listing) => listing,
                Err(err) => return reply.error(Errno::from(err)),
            };
            let mut items: Vec<(INodeNo, FileType, String)> = vec![
                (ino, FileType::Directory, ".".to_string()),
                (INodeNo(ROOT), FileType::Directory, "..".to_string()),
            ];
            for entry in listing {
                let kind = match entry.kind {
                    fdrive_core::sdk::FileType::Directory => FileType::Directory,
                    fdrive_core::sdk::FileType::File => FileType::RegularFile,
                };
                let child = dir.join(&entry.name);
                if child.parent_or_root() != dir {
                    continue;
                }
                items.push((INodeNo(wire.ino(&child)), kind, entry.name));
            }
            for (i, (ino, kind, name)) in items.into_iter().enumerate().skip(offset as usize) {
                if reply.add(ino, (i + 1) as u64, kind, name) {
                    break;
                }
            }
            reply.ok();
        });
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(path) = self.wire.child(parent.0, name) else {
            return reply.error(Errno::EINVAL);
        };
        self.go(move |wire| {
            log::debug!("mkdir path={path}");
            if let Err(err) = wire.adapter.mkdir(&path) {
                return reply.error(Errno::from(err));
            }
            match wire.attr(&path) {
                Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
                None => reply.error(Errno::EIO),
            }
        });
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(path) = self.wire.child(parent.0, name) else {
            return reply.error(Errno::EINVAL);
        };
        self.go(move |wire| {
            log::debug!("create path={path}");
            if let Err(err) = wire.adapter.create(&path) {
                return reply.error(Errno::from(err));
            }
            match wire.attr(&path) {
                Some(attr) => reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(0),
                    FopenFlags::empty(),
                ),
                None => reply.error(Errno::EIO),
            }
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.wire.path(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        self.go(move |wire| {
            log::debug!("open path={path} flags={flags:x}");
            let result = if flags.0 & libc::O_TRUNC != 0 {
                wire.adapter.truncate(&path, 0)
            } else {
                wire.adapter.hydrate_start(&path)
            };
            match result {
                Ok(()) => reply.opened(FileHandle(0), FopenFlags::empty()),
                Err(err) => reply.error(Errno::from(err)),
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(path) = self.wire.path(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        self.go(move |wire| match wire.adapter.read(&path, offset, size) {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(Errno::from(err)),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let Some(path) = self.wire.path(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        let data = data.to_vec();
        self.go(move |wire| match wire.adapter.write(&path, offset, &data) {
            Ok(written) => reply.written(written),
            Err(err) => reply.error(Errno::from(err)),
        });
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.wire.child(parent.0, name) else {
            return reply.error(Errno::ENOENT);
        };
        self.go(move |wire| {
            log::debug!("rm path={path}");
            match wire.adapter.delete(&path, false) {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(Errno::from(err)),
            }
        });
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.wire.child(parent.0, name) else {
            return reply.error(Errno::ENOENT);
        };
        self.go(move |wire| {
            log::debug!("rmdir path={path}");
            match wire.adapter.rmdir(&path) {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(Errno::from(err)),
            }
        });
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(from), Some(to)) =
            (self.wire.child(parent.0, name), self.wire.child(newparent.0, newname))
        else {
            return reply.error(Errno::ENOENT);
        };
        self.go(move |wire| {
            log::debug!("mv from={from} to={to}");
            match wire.adapter.rename(&from, &to) {
                Ok(()) => {
                    wire.remap(&from, &to);
                    reply.ok();
                }
                Err(err) => reply.error(Errno::from(err)),
            }
        });
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(path) = self.wire.path(ino.0) {
            self.wire.adapter.released(&path);
        }
        reply.ok();
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let Some(path) = self.wire.path(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(name) = name.to_str() else {
            return reply.error(Errno::EINVAL);
        };
        match self.wire.adapter.xattrs().set(&path, name, value, flags) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(path) = self.wire.path(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        match name
            .to_str()
            .and_then(|name| self.wire.adapter.xattrs().get(&path, name))
        {
            Some(value) => xattr_reply(reply, &value, size),
            None => reply.error(Errno::ENODATA),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let Some(path) = self.wire.path(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        xattr_reply(reply, &self.wire.adapter.xattrs().list(&path), size);
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.wire.path(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(name) = name.to_str() else {
            return reply.error(Errno::EINVAL);
        };
        match self.wire.adapter.xattrs().remove(&path, name) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }
}

impl MountFs {
    pub fn new(adapter: Arc<Adapter>) -> Self {
        let root = RelPath::root();
        Self {
            rt: adapter.rt().clone(),
            wire: Arc::new(Wire {
                adapter,
                inodes: Mutex::new(InodeTable {
                    paths: HashMap::from([(ROOT, root.clone())]),
                    inos: HashMap::from([(root, ROOT)]),
                    next_ino: 2,
                }),
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
            }),
        }
    }

    fn go(&self, task: impl FnOnce(&Wire) + Send + 'static) {
        let wire = self.wire.clone();
        self.rt.spawn_blocking(move || task(&wire));
    }
}

impl Wire {
    fn ino(&self, path: &RelPath) -> u64 {
        let mut inodes = self.inodes.lock().unwrap();
        if let Some(ino) = inodes.inos.get(path) {
            return *ino;
        }
        let ino = inodes.next_ino;
        inodes.next_ino += 1;
        inodes.inos.insert(path.clone(), ino);
        inodes.paths.insert(ino, path.clone());
        ino
    }

    fn path(&self, ino: u64) -> Option<RelPath> {
        self.inodes.lock().unwrap().paths.get(&ino).cloned()
    }

    fn child(&self, parent: u64, name: &OsStr) -> Option<RelPath> {
        let name = name.to_str()?;
        let path = self.path(parent)?.join(name);
        (path.parent_or_root() == self.path(parent)?).then_some(path)
    }

    fn attr(&self, path: &RelPath) -> Option<FileAttr> {
        let (is_dir, size, mtime) = self.adapter.attr(path).ok()??;
        let kind = if is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        Some(FileAttr {
            ino: INodeNo(self.ino(path)),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm: if is_dir { 0o755 } else { 0o644 },
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }

    fn remap(&self, from: &RelPath, to: &RelPath) {
        let mut inodes = self.inodes.lock().unwrap();
        let moved: Vec<(RelPath, u64)> = inodes
            .inos
            .iter()
            .filter(|(p, _)| *p == from || p.is_descendant_of(from))
            .map(|(p, i)| (p.clone(), *i))
            .collect();
        for (old, ino) in moved {
            let new = RelPath::new(&old.as_str().replacen(from.as_str(), to.as_str(), 1));
            inodes.inos.remove(&old);
            inodes.inos.insert(new.clone(), ino);
            inodes.paths.insert(ino, new);
        }
    }
}

fn xattr_reply(reply: ReplyXattr, data: &[u8], size: u32) {
    if size == 0 {
        reply.size(data.len() as u32);
    } else if data.len() as u32 <= size {
        reply.data(data);
    } else {
        reply.error(Errno::ERANGE);
    }
}
