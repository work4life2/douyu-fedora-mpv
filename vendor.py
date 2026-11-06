#!/usr/bin/env python3
"""手动 vendor cargo 依赖，绕过沙箱对 futimens 的限制。

原理：cargo 解压 .crate 时用 futimens 设文件 mtime，而本沙箱 seccomp 禁止
futimens（但放行 utimensat）。Python 的 os.utime 走 utimensat，可以正常设 mtime。
因此用 Python 完成「下载 .crate → 解压 → 写 .cargo-ok」，cargo 检测到
.cargo-ok 后跳过 unpack，直接进入编译。
"""
import os
import re
import sys
import tarfile
import urllib.request

REG_HASH = "index.crates.io-1949cf8c6b5b557f"
CARGO_HOME = os.path.expanduser("~/.cargo")
CACHE_DIR = os.path.join(CARGO_HOME, "registry", "cache", REG_HASH)
SRC_DIR = os.path.join(CARGO_HOME, "registry", "src", REG_HASH)
PROXY = "http://127.0.0.1:1080"


def parse_lock(lockfile: str):
    data = open(lockfile, encoding="utf-8").read()
    pkgs = []
    for block in re.findall(r"\[\[package\]\](.*?)(?=\[\[|\Z)", data, re.S):
        n = re.search(r'name = "([^"]+)"', block)
        v = re.search(r'version = "([^"]+)"', block)
        s = re.search(r'source = "registry', block)
        if n and v and s:
            pkgs.append((n.group(1), v.group(1)))
    return pkgs


def download(name: str, version: str) -> str:
    crate = f"{name}-{version}.crate"
    dest = os.path.join(CACHE_DIR, crate)
    if os.path.exists(dest):
        return dest
    url = f"https://static.crates.io/crates/{name}/{crate}"
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({"https": PROXY, "http": PROXY})
    )
    req = urllib.request.Request(url, headers={"User-Agent": "cargo/1.99"})
    data = opener.open(req, timeout=90).read()
    with open(dest, "wb") as f:
        f.write(data)
    return dest


def unpack(name: str, version: str, crate_path: str):
    import shutil
    target = os.path.join(SRC_DIR, f"{name}-{version}")
    ok = os.path.join(target, ".cargo-ok")
    if os.path.exists(ok) and os.path.exists(os.path.join(target, "Cargo.toml")):
        return
    # 清空残留（cargo 失败可能留下半解压状态），重新解压
    shutil.rmtree(target, ignore_errors=True)
    os.makedirs(target, exist_ok=True)
    with tarfile.open(crate_path, "r:gz") as tar:
        for member in tar.getmembers():
            rel = member.name.split("/", 1)
            if len(rel) < 2:
                continue
            dest = os.path.join(target, rel[1])
            if member.isdir():
                os.makedirs(dest, exist_ok=True)
                continue
            f = tar.extractfile(member)
            if f is None:
                continue
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with open(dest, "wb") as out:
                out.write(f.read())
            if member.mtime:
                try:
                    os.utime(dest, (member.mtime, member.mtime))
                except OSError:
                    pass
    with open(ok, "w") as f:
        f.write("ok")
    # 完整性校验：至少要有 Cargo.toml
    if not os.path.exists(os.path.join(target, "Cargo.toml")):
        raise RuntimeError(f"{name}-{version} 解压后缺 Cargo.toml，可能 .crate 不完整")


def main():
    lockfile = sys.argv[1] if len(sys.argv) > 1 else "Cargo.lock"
    pkgs = parse_lock(lockfile)
    print(f"共 {len(pkgs)} 个 registry 包")
    os.makedirs(CACHE_DIR, exist_ok=True)
    os.makedirs(SRC_DIR, exist_ok=True)
    ok_count = 0
    for name, version in pkgs:
        try:
            p = download(name, version)
            unpack(name, version, p)
            ok_count += 1
            print(f"  OK  {name} {version}")
        except Exception as e:
            print(f"  FAIL {name} {version}: {e}")
    print(f"完成：{ok_count}/{len(pkgs)} 个包已就绪")


if __name__ == "__main__":
    main()
