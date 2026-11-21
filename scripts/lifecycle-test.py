"""在 smoke-test.sh 的临时 Linux 挂载中验证删除生命周期。"""
import ctypes
import os
from pathlib import Path
import sys
import time

root = Path(sys.argv[1])
log = Path('artifacts/hello-fs.log')
path = root / 'lifecycle.txt'
path.write_bytes(b'old content')
fd = os.open(path, os.O_RDWR)
second = os.open(path, os.O_RDWR)
duplicate = os.dup(fd)
old_ino = os.fstat(fd).st_ino
try:
    path.unlink()
    assert not path.exists()
    assert 'lifecycle.txt' not in os.listdir(root)
    assert os.fstat(fd).st_nlink == 0
    try:
        path.unlink()
    except FileNotFoundError:
        pass
    else:
        raise AssertionError('Repeated unlink must fail')
    path.write_bytes(b'new file')
    assert path.stat().st_ino != old_ino
    assert os.pread(fd, 100, 0) == b'old content'
    os.pwrite(second, b'OLD', 0)
    assert os.pread(duplicate, 100, 0) == b'OLD content'
    os.ftruncate(duplicate, 3)
    assert os.pread(second, 100, 0) == b'OLD'
    assert path.read_bytes() == b'new file'
    os.close(fd)
    fd = None
    assert os.pread(duplicate, 100, 0) == b'OLD'  # flush 不能当作 release
    os.close(second)
    second = None
    assert os.pread(duplicate, 100, 0) == b'OLD'
    assert f'reclaim: inode={old_ino}\n' not in log.read_text()
finally:
    for handle in (fd, second, duplicate):
        if handle is not None:
            os.close(handle)

# release/forget 可能异步到达，等待明确的回收证据。
deadline = time.monotonic() + 5
while f'reclaim: inode={old_ino}\n' not in log.read_text():
    assert time.monotonic() < deadline, 'Unlinked inode was not reclaimed'
    time.sleep(0.02)
assert path.read_bytes() == b'new file'
path.unlink()
assert not path.exists()
print('PASS: unlink, multiple opens, dup, ftruncate after unlink, name reuse, inode reclamation')

# Linux readdir64/telldir/seekdir：强制用保存的 cookie 重新读取，绕开 libc 预读缓存。
class Dirent64(ctypes.Structure):
    _fields_ = [('ino', ctypes.c_uint64), ('off', ctypes.c_int64),
                ('reclen', ctypes.c_ushort), ('type', ctypes.c_ubyte),
                ('name', ctypes.c_char * 256)]

libc = ctypes.CDLL(None, use_errno=True)
libc.opendir.argtypes = [ctypes.c_char_p]
libc.opendir.restype = ctypes.c_void_p
libc.readdir64.argtypes = [ctypes.c_void_p]
libc.readdir64.restype = ctypes.POINTER(Dirent64)
libc.telldir.argtypes = [ctypes.c_void_p]
libc.telldir.restype = ctypes.c_long
libc.seekdir.argtypes = [ctypes.c_void_p, ctypes.c_long]
libc.closedir.argtypes = [ctypes.c_void_p]
directory = libc.opendir(os.fsencode(root))
assert directory, ctypes.get_errno()

def next_name():
    ctypes.set_errno(0)
    entry = libc.readdir64(directory)
    if not entry:
        assert ctypes.get_errno() == 0
        return None
    return os.fsdecode(entry.contents.name)

try:
    while True:
        name = next_name()
        assert name is not None
        if name not in ('.', '..'):
            break
    cookie = libc.telldir(directory)
    (root / name).unlink()
    (root / 'after-delete.txt').write_bytes(b'added while iterating')
    expected = set(os.listdir(root))
    libc.seekdir(directory, 0)
    libc.seekdir(directory, cookie)
    remaining = []
    while (name := next_name()) is not None:
        remaining.append(name)
    assert len(remaining) == len(set(remaining)), remaining
    assert set(remaining) == expected, (remaining, expected)
finally:
    libc.closedir(directory)
print('PASS: directory resume cookie survives deletion and insertion')
