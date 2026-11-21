//! 程序入口：检查挂载点、配置挂载选项并启动 FUSE 服务。

mod filesystem;
mod state;

use std::os::unix::fs::MetadataExt; // 获取挂载目录所有者的 uid/gid
use std::path::PathBuf;

use fuser::{Config, MountOption};

use filesystem::MemFs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mountpoint = PathBuf::from(
        args.next()
            .ok_or("Usage: mini_memfs <empty-mount-directory>")?,
    );
    if args.next().is_some() {
        return Err("Usage: mini_memfs <empty-mount-directory>".into());
    }
    // 挂载会遮住目录原有内容，因此只接受已经存在的空目录。
    let metadata = std::fs::metadata(&mountpoint)?;
    if !metadata.is_dir() || std::fs::read_dir(&mountpoint)?.next().is_some() {
        return Err("Mount point must be an existing empty directory".into());
    }
    let fs = MemFs::new(metadata.uid(), metadata.gid());
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RW,
        MountOption::FSName("mini_memfs".into()),
        MountOption::DefaultPermissions,
    ];
    eprintln!("Mounting writable MemFs at {}", mountpoint.display());
    eprintln!("Use another Ubuntu terminal to read hello.txt and run fusermount3 -u <mountpoint>.");
    fuser::mount(fs, &mountpoint, &config)?;
    eprintln!("MemFs unmounted.");
    Ok(())
}
