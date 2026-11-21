//! FUSE 适配层：接收内核请求、持锁访问状态，并用 Reply 类型回复结果。
//! 回调由 fuser 分发，执行顺序由实际文件操作决定。

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

use crate::state::{FsState, MAX_FILE_SIZE, MAX_FILES, Node};

// 每次重新查询属性，便于课堂演示时观察写入和删除后的变化。
const TTL: Duration = Duration::ZERO;

pub(super) struct MemFs {
    // FUSE 回调接收 &self；用 Mutex 串行访问状态，保持多张表一致。
    state: Mutex<FsState>,
}

impl MemFs {
    pub(super) fn new(uid: u32, gid: u32) -> Self {
        Self {
            state: Mutex::new(FsState::new(uid, gid)),
        }
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, FsState>, Errno> {
        // 如果其他请求曾在持锁期间 panic，不再使用可能不一致的状态。
        self.state.lock().map_err(|_| Errno::EIO)
    }
}

// 按照 Filesystem 这个 trait 的约定，提供相应文件操作的实现
// 实现回调：
// create    创建新文件
// open      打开文件
// unlink    删除名称
// release   关闭文件句柄 (一次打开的所有引用释放时)
// flush     检查文件句柄 (文件描述符关闭时，不落盘、不释放句柄)
// forget    释放内核引用
// write     写文件
// setattr   修改文件属性
// lookup    根据名称找节点
// getattr   获取节点属性
// readdir   列出目录内容
// read      读取文件内容
// 这些函数通常由 fuser 在处理内核请求时调用，不需要在 main 中手动按顺序调用它们
impl Filesystem for MemFs {
    // 创建请求将名称与空节点一起插入；持有同一把锁，避免两张表不同步。
    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        eprintln!("create: parent={} name={name:?}", u64::from(parent));
        let result = (|| {
            let mut state = self.lock_state()?;
            if parent != INodeNo::ROOT {
                return Err(if state.nodes.contains_key(&parent) {
                    Errno::ENOTDIR
                } else {
                    Errno::ENOENT
                });
            }
            let bytes = name.as_bytes();
            if bytes.is_empty()
                || bytes.contains(&b'/')
                || bytes.contains(&0)
                || name == "."
                || name == ".."
            {
                return Err(Errno::EINVAL);
            }
            if bytes.len() > 255 {
                return Err(Errno::ENAMETOOLONG);
            }
            if state.entries.contains_key(name) {
                return Err(Errno::EEXIST);
            }
            if state.nodes.len() >= MAX_FILES {
                return Err(Errno::ENOSPC);
            }
            let ino = INodeNo(state.next_ino);
            // inode+1 用作目录 cookie，限制在有符号 off_t 的正数范围内。
            if state.next_ino >= i64::MAX as u64 {
                return Err(Errno::ENOSPC);
            }
            state.next_fh.checked_add(1).ok_or(Errno::EMFILE)?;
            let next = state.next_ino.checked_add(1).ok_or(Errno::ENOSPC)?;
            let now = SystemTime::now();
            let attr = FileAttr {
                ino,
                size: 0,
                blocks: 0,
                kind: FileType::RegularFile,
                perm: (mode & !umask & 0o777) as u16,
                nlink: 1,
                uid: req.uid(),
                gid: req.gid(),
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                ..state.root
            };
            state.nodes.insert(
                ino,
                Node {
                    attr,
                    data: Vec::new(),
                    nlookup: 1, // create 的 entry 回复占一个内核查找引用
                },
            );
            state.entries.insert(name.to_os_string(), ino);
            state.next_ino = next;
            state.root.mtime = now;
            state.root.ctime = now;
            let fh = state.open_file(ino, OpenFlags(flags))?;
            Ok((attr, fh))
        })();
        match result {
            Ok((attr, fh)) => {
                reply.created(&TTL, &attr, Generation(0), fh, FopenFlags::FOPEN_DIRECT_IO)
            }
            Err(error) => reply.error(error),
        }
    }

    // direct_io 让读写直接到达本程序；不启用 writeback cache。
    // 未请求 ATOMIC_O_TRUNC 能力，Linux 会在 O_TRUNC 打开前通过 setattr 清零。
    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let result = self
            .lock_state()
            .and_then(|mut state| state.open_file(ino, flags));
        match result {
            Ok(fh) => reply.opened(fh, FopenFlags::FOPEN_DIRECT_IO),
            Err(error) => reply.error(error),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        eprintln!("unlink: parent={} name={name:?}", u64::from(parent));
        let result = (|| {
            let mut state = self.lock_state()?;
            if parent != INodeNo::ROOT {
                return Err(if state.nodes.contains_key(&parent) {
                    Errno::ENOTDIR
                } else {
                    Errno::ENOENT
                });
            }
            if name == "." || name == ".." {
                return Err(Errno::EISDIR);
            }
            let ino = *state.entries.get(name).ok_or(Errno::ENOENT)?;
            let node = state.nodes.get_mut(&ino).ok_or(Errno::EIO)?;
            let now = SystemTime::now();
            node.attr.nlink = node.attr.nlink.checked_sub(1).ok_or(Errno::EIO)?;
            node.attr.ctime = now;
            state.entries.remove(name);
            state.root.mtime = now;
            state.root.ctime = now;
            state.reclaim(ino);
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        eprintln!("release: inode={} fh={}", u64::from(ino), fh.0);
        let result = (|| {
            let mut state = self.lock_state()?;
            state.check_handle(ino, fh)?;
            state.handles.remove(&fh.0);
            state.reclaim(ino);
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    // 纯内存文件系统：这里只验证句柄，不执行落盘，也不释放打开状态。
    // dup 出来的多个描述符会多次 flush，但只在最后关闭时 release 一次。
    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let result = self
            .lock_state()
            .and_then(|state| state.check_handle(ino, fh).map(|_| ()));
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let Ok(mut state) = self.lock_state() else {
            return;
        };
        if let Some(node) = state.nodes.get_mut(&ino) {
            if let Some(remaining) = node.nlookup.checked_sub(nlookup) {
                node.nlookup = remaining;
                eprintln!("forget: inode={} remaining={remaining}", u64::from(ino));
                state.reclaim(ino);
            } else {
                eprintln!(
                    "forget: invalid reference count for inode={}",
                    u64::from(ino)
                );
            }
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        eprintln!(
            "write: inode={} offset={offset} size={}",
            u64::from(ino),
            data.len()
        );
        let result = (|| {
            let mut state = self.lock_state()?;
            if ino == INodeNo::ROOT {
                return Err(Errno::EISDIR);
            }
            if state.check_handle(ino, fh)?.flags.acc_mode() == OpenAccMode::O_RDONLY {
                return Err(Errno::EBADF);
            }
            let node = state.nodes.get_mut(&ino).ok_or(Errno::ENOENT)?;
            if data.is_empty() {
                return Ok(0);
            }
            let end = offset.checked_add(data.len() as u64).ok_or(Errno::EFBIG)?;
            if end > MAX_FILE_SIZE as u64 {
                return Err(Errno::EFBIG);
            }
            if end > node.data.len() as u64 {
                node.resize(end)?;
            }
            node.data[offset as usize..end as usize].copy_from_slice(data);
            node.changed();
            Ok(data.len() as u32)
        })();
        match result {
            Ok(size) => reply.written(size),
            Err(error) => reply.error(error),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        eprintln!("setattr: inode={} size={size:?}", u64::from(ino));
        let result = (|| {
            // 本阶段仅支持长度和访问/修改时间；其他修改明确拒绝，避免虚假成功。
            if mode.is_some()
                || uid.is_some()
                || gid.is_some()
                || crtime.is_some()
                || chgtime.is_some()
                || bkuptime.is_some()
                || flags.is_some()
            {
                return Err(Errno::EOPNOTSUPP);
            }
            let mut state = self.lock_state()?;
            if let Some(fh) = fh {
                let file = state.check_handle(ino, fh)?;
                if size.is_some() && file.flags.acc_mode() == OpenAccMode::O_RDONLY {
                    return Err(Errno::EBADF);
                }
            }
            let attr = if ino == INodeNo::ROOT {
                if size.is_some() {
                    return Err(Errno::EISDIR);
                }
                &mut state.root
            } else {
                let node = state.nodes.get_mut(&ino).ok_or(Errno::ENOENT)?;
                if let Some(size) = size {
                    node.resize(size)?;
                }
                &mut node.attr
            };
            let now = SystemTime::now();
            let resolve = |time| match time {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => now,
            };
            if let Some(time) = atime {
                attr.atime = resolve(time);
            }
            if let Some(time) = mtime {
                attr.mtime = resolve(time);
            }
            attr.ctime = ctime.unwrap_or(now);
            Ok(*attr)
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(error),
        }
    }

    // 文件名 -> inode 和属性。
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        eprintln!("lookup: parent={} name={name:?}", u64::from(parent));
        let mut state = match self.lock_state() {
            Ok(state) => state,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        if parent != INodeNo::ROOT {
            reply.error(if state.nodes.contains_key(&parent) {
                Errno::ENOTDIR
            } else {
                Errno::ENOENT
            });
        } else if let Some(ino) = state.entries.get(name).copied() {
            match state.nodes.get_mut(&ino) {
                Some(node) => {
                    let Some(count) = node.nlookup.checked_add(1) else {
                        reply.error(Errno::EOVERFLOW);
                        return;
                    };
                    node.nlookup = count;
                    reply.entry(&TTL, &node.attr, Generation(0));
                }
                None => reply.error(Errno::EIO), // 目录项与节点表必须保持一致
            }
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    // inode -> 文件属性；ls -l 和 stat 会使用这些信息。
    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        eprintln!("getattr: inode={}", u64::from(ino));
        let state = match self.lock_state() {
            Ok(state) => state,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        if ino == INodeNo::ROOT {
            reply.attr(&TTL, &state.root);
        } else if let Some(node) = state.nodes.get(&ino) {
            reply.attr(&TTL, &node.attr);
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    // offset 是目录遍历的位置；缓冲区满时，下次从尚未返回的条目继续。
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        eprintln!("readdir: inode={} offset={offset}", u64::from(ino));
        let state = match self.lock_state() {
            Ok(state) => state,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        if ino != INodeNo::ROOT {
            reply.error(if state.nodes.contains_key(&ino) {
                Errno::ENOTDIR
            } else {
                Errno::ENOENT
            });
            return;
        }
        let mut entries = vec![
            (1, INodeNo::ROOT, FileType::Directory, OsStr::new(".")),
            (2, INodeNo::ROOT, FileType::Directory, OsStr::new("..")),
        ];
        // 按单调分配的 inode 排序，新增文件不会插入已有续读位置之前。
        for (name, entry_ino) in &state.entries {
            let Some(node) = state.nodes.get(entry_ino) else {
                reply.error(Errno::EIO);
                return;
            };
            entries.push((
                u64::from(*entry_ino) + 1,
                *entry_ino,
                node.attr.kind,
                name.as_os_str(),
            ));
        }
        entries[2..].sort_by_key(|entry| entry.0);
        // cookie 不依赖数组下标：删除任何已返回条目，不会移动未返回条目的位置。
        for (cookie, entry_ino, kind, name) in entries {
            if cookie <= offset {
                continue;
            }
            if reply.add(entry_ino, cookie, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    // 文件内容按字节切片；必须同时尊重 offset、size 和文件末尾。
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        eprintln!("read: inode={} offset={offset} size={size}", u64::from(ino));
        let state = match self.lock_state() {
            Ok(state) => state,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        if ino == INodeNo::ROOT {
            reply.error(Errno::EISDIR);
            return;
        }
        match state.check_handle(ino, fh) {
            Ok(file) if file.flags.acc_mode() != OpenAccMode::O_WRONLY => {}
            _ => {
                reply.error(Errno::EBADF);
                return;
            }
        }
        let Some(node) = state.nodes.get(&ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let start = offset.min(node.data.len() as u64) as usize;
        let end = start.saturating_add(size as usize).min(node.data.len());
        reply.data(&node.data[start..end]);
    }
}
