//! 内存数据模型：节点内容、目录项、打开句柄与节点回收。
//! entries、nodes、handles 的修改由 filesystem 模块持有同一把锁保护。

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::time::SystemTime;

use fuser::{Errno, FileAttr, FileHandle, FileType, INodeNo, OpenFlags};

pub(super) const MAX_FILE_SIZE: usize = 16 * 1024 * 1024; // 单文件最多 16 MiB
pub(super) const MAX_FILES: usize = 256;

// 节点保存属性与内容；目录项单独保存名称到 inode 的映射。
pub(super) struct Node {
    pub(super) attr: FileAttr,
    pub(super) data: Vec<u8>,
    pub(super) nlookup: u64, // 成功 lookup/create 回复增加，forget 通知减少
}

pub(super) struct OpenFile {
    pub(super) ino: INodeNo,
    pub(super) flags: OpenFlags,
}

impl Node {
    pub(super) fn resize(&mut self, size: u64) -> Result<(), Errno> {
        let size = usize::try_from(size).map_err(|_| Errno::EFBIG)?;
        if size > MAX_FILE_SIZE {
            return Err(Errno::EFBIG);
        }
        if size > self.data.len() {
            self.data
                .try_reserve(size - self.data.len())
                .map_err(|_| Errno::ENOMEM)?;
        }
        self.data.resize(size, 0);
        self.changed();
        Ok(())
    }

    pub(super) fn changed(&mut self) {
        self.attr.size = self.data.len() as u64;
        self.attr.blocks = self.attr.size.div_ceil(512);
        self.attr.mtime = SystemTime::now();
        self.attr.ctime = self.attr.mtime;
    }
}

pub(super) struct FsState {
    pub(super) root: FileAttr,
    // 文件名 -> inode；unlink 只移除这里的名称，节点可能继续存活。
    pub(super) entries: BTreeMap<OsString, INodeNo>,
    pub(super) nodes: BTreeMap<INodeNo, Node>,
    pub(super) next_ino: u64,
    // 每次 open/create 对应一个句柄；dup 共享同一次打开。
    pub(super) handles: BTreeMap<u64, OpenFile>,
    pub(super) next_fh: u64,
}

// 根目录、名称映射、节点和打开句柄共同组成内存状态。
impl FsState {
    pub(super) fn new(uid: u32, gid: u32) -> Self {
        let now = SystemTime::now();
        let root = FileAttr {
            ino: INodeNo::ROOT,        // 节点编号
            size: 0,                   // 目录大小
            blocks: 0,                 // 分配的块数
            atime: now,                // 最后访问时间
            mtime: now,                // 最后修改内容时间
            ctime: now,                // 最后修改元数据时间
            crtime: now,               // 创建时间
            kind: FileType::Directory, // 节点类型
            perm: 0o755,               // 所有者可创建文件
            nlink: 2,                  // 硬链接数 表示有多少个目录项指向同一个 inode
            uid,
            gid,
            rdev: 0,
            blksize: 512, // 报告的 I/O 块大小
            flags: 0,
        };
        let mut state = Self {
            root,
            entries: BTreeMap::new(),
            nodes: BTreeMap::new(),
            next_ino: 2,
            handles: BTreeMap::new(),
            next_fh: 1,
        };
        // 预置内容统一在此注册，FUSE 回调无需感知具体文件名。
        state.add_file("hello.txt", b"Hello from Rust FUSE!\n");
        state.add_file("notes.txt", b"My first memory filesystem.\n");
        state
    }

    // 仅供初始化使用；运行时创建由 create 回调检查权限、重名和容量。
    fn add_file(&mut self, name: &str, content: &[u8]) {
        assert!(!self.entries.contains_key(OsStr::new(name)));
        let ino = INodeNo(self.next_ino);
        self.next_ino = self.next_ino.checked_add(1).expect("inode exhausted");
        let data = content.to_vec();
        let attr = FileAttr {
            ino,
            size: data.len() as u64,
            blocks: (data.len() as u64).div_ceil(512),
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            ..self.root
        };
        self.nodes.insert(
            ino,
            Node {
                attr,
                data,
                nlookup: 0,
            },
        );
        self.entries.insert(OsString::from(name), ino);
    }

    pub(super) fn open_file(
        &mut self,
        ino: INodeNo,
        flags: OpenFlags,
    ) -> Result<FileHandle, Errno> {
        if ino == INodeNo::ROOT {
            return Err(Errno::EISDIR);
        }
        if !self.nodes.contains_key(&ino) {
            return Err(Errno::ENOENT);
        }
        let next = self.next_fh.checked_add(1).ok_or(Errno::EMFILE)?;
        let fh = self.next_fh;
        self.handles.insert(fh, OpenFile { ino, flags });
        self.next_fh = next;
        eprintln!("open: inode={} fh={fh}", u64::from(ino));
        Ok(FileHandle(fh))
    }

    pub(super) fn check_handle(&self, ino: INodeNo, fh: FileHandle) -> Result<&OpenFile, Errno> {
        self.handles
            .get(&fh.0)
            .filter(|file| file.ino == ino)
            .ok_or(Errno::EBADF)
    }

    // 三种引用都消失才回收；release 与 forget 的先后顺序不作假设。
    pub(super) fn reclaim(&mut self, ino: INodeNo) {
        if self
            .nodes
            .get(&ino)
            .is_some_and(|n| n.attr.nlink == 0 && n.nlookup == 0)
            && !self.handles.values().any(|file| file.ino == ino)
        {
            self.nodes.remove(&ino);
            eprintln!("reclaim: inode={}", u64::from(ino));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reclaim_waits_for_links_all_handles_and_all_lookups_in_either_order() {
        for forget_first in [true, false] {
            let mut state = FsState::new(1000, 1000);
            let ino = *state.entries.get(OsStr::new("hello.txt")).unwrap();
            state.reclaim(ino);
            assert!(state.nodes.contains_key(&ino)); // 仍有目录项不能回收
            let first = state.open_file(ino, OpenFlags(0)).unwrap();
            let last = state.open_file(ino, OpenFlags(0)).unwrap();
            state.entries.remove(OsStr::new("hello.txt"));
            let node = state.nodes.get_mut(&ino).unwrap();
            node.attr.nlink = 0;
            node.nlookup = 2;
            state.reclaim(ino);
            assert!(state.nodes.contains_key(&ino));
            state.handles.remove(&first.0);
            state.reclaim(ino);
            assert!(state.nodes.contains_key(&ino));
            if forget_first {
                state.nodes.get_mut(&ino).unwrap().nlookup = 0;
            } else {
                state.handles.remove(&last.0);
            }
            state.reclaim(ino);
            assert!(state.nodes.contains_key(&ino));
            state.nodes.get_mut(&ino).unwrap().nlookup = 0;
            state.handles.remove(&last.0);
            state.reclaim(ino);
            assert!(!state.nodes.contains_key(&ino));
        }
    }
}
