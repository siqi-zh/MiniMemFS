#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
target_dir="${CARGO_TARGET_DIR:-$HOME/.cache/os-task1-target}"
binary="$target_dir/debug/mini_memfs"
test -x "$binary" || { echo 'Build first: cargo build --locked'; exit 1; }
mount_dir="$(mktemp -d /tmp/os-task1-hello.XXXXXX)"
mkdir -p artifacts
fs_pid=''
cleanup() {
    if mountpoint -q "$mount_dir"; then
        fusermount3 -u "$mount_dir" || return 1
    fi
    if [[ -n "$fs_pid" ]] && kill -0 "$fs_pid" 2>/dev/null; then
        kill "$fs_pid" 2>/dev/null || true
        wait "$fs_pid" 2>/dev/null || true
    fi
    rmdir "$mount_dir"
}
trap cleanup EXIT
"$binary" "$mount_dir" >artifacts/hello-fs.log 2>&1 &
fs_pid=$!
for _ in {1..50}; do
    if mountpoint -q "$mount_dir"; then break; fi
    if ! kill -0 "$fs_pid" 2>/dev/null; then cat artifacts/hello-fs.log; exit 1; fi
    sleep 0.1
done
mountpoint -q "$mount_dir" || { cat artifacts/hello-fs.log; exit 1; }
python3 - "$mount_dir" <<'PY'
import errno
import os
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
path = root / 'hello.txt'
expected = b'Hello from Rust FUSE!\n'
files = {
    'hello.txt': expected,
    'notes.txt': b'My first memory filesystem.\n',
}
assert sorted(p.name for p in root.iterdir()) == sorted(files)
assert (root / 'hello.txt').stat().st_ino != (root / 'notes.txt').stat().st_ino
for name, content in files.items():
    file_path = root / name
    assert file_path.stat().st_size == len(content)
    assert file_path.read_bytes() == content
    with file_path.open('rb') as handle:
        assert os.pread(handle.fileno(), 4, 6) == content[6:10]
        assert os.pread(handle.fileno(), 20, len(content)) == b''
        assert os.pread(handle.fileno(), 20, 1000) == b''
try:
    (root / 'missing.txt').read_bytes()
except FileNotFoundError:
    pass
else:
    raise AssertionError('Missing file should return ENOENT')
created = root / 'test.txt'
old_umask = os.umask(0o027)
try:
    fd = os.open(created, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o666)
finally:
    os.umask(old_umask)
try:
    assert created.stat().st_size == 0
    assert created.stat().st_mode & 0o777 == 0o640
    assert created.stat().st_uid == os.getuid()
    os.write(fd, b'hello')
    assert created.read_bytes() == b'hello'
    assert os.pwrite(fd, b'XY', 1) == 2
    assert created.read_bytes() == b'hXYlo'
    os.pwrite(fd, b'!', 8)
    assert created.read_bytes() == b'hXYlo\0\0\0!'
finally:
    os.close(fd)
with created.open('ab') as handle:
    handle.write(b' world')
assert created.read_bytes() == b'hXYlo\0\0\0! world'
created.write_bytes(b'xy')  # O_TRUNC 必须清除旧尾部
assert created.read_bytes() == b'xy'
assert created.stat().st_size == 2
os.truncate(created, 1)
assert created.read_bytes() == b'x'
os.truncate(created, 5)
assert created.read_bytes() == b'x\0\0\0\0'
os.utime(created, ns=(1_000_000_000, 2_000_000_000))
assert created.stat().st_mtime_ns == 2_000_000_000
try:
    os.open(created, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
except FileExistsError:
    pass
else:
    raise AssertionError('Exclusive creation must reject duplicate names')
try:
    os.truncate(created, 16 * 1024 * 1024 + 1)
except OSError as error:
    assert error.errno == errno.EFBIG, error
else:
    raise AssertionError('Oversized truncate should fail')
with created.open('r+b') as handle:
    try:
        os.pwrite(handle.fileno(), b'!', 16 * 1024 * 1024)
    except OSError as error:
        assert error.errno == errno.EFBIG, error
    else:
        raise AssertionError('Oversized write should fail')
assert created.read_bytes() == b'x\0\0\0\0'
assert path.read_bytes() == expected
assert (root / 'notes.txt').read_bytes() == files['notes.txt']
# 名字排序靠前的新文件，验证目录状态更新和文件间隔离。
(root / 'aaa.txt').write_bytes(b'independent')
assert sorted(p.name for p in root.iterdir()) == ['aaa.txt', 'hello.txt', 'notes.txt', 'test.txt']
print('PASS: read/create/write/append/truncate, zero fill, permissions, isolation, errors, limits')
PY
python3 scripts/lifecycle-test.py "$mount_dir"
fusermount3 -u "$mount_dir"
wait "$fs_pid"
fs_pid=''
echo 'PASS: clean unmount and process exit'
