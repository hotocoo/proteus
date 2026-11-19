#!/usr/bin/env python3
"""Copies a dylib and every Homebrew library it needs, however deep, into one folder, and points
them at each other through @loader_path, so the folder works on a Mac without Homebrew.

usage: scripts/bundle-dylib.py <dylib> <out folder>

Prints the Homebrew packages the copies came from, one per line, so their licences can be shipped.
System libraries (/usr/lib, /System) are left as references. Each copy is signed again (ad hoc),
because changing a library breaks its signature.
"""
import os
import shutil
import subprocess
import sys

HOMEBREW_PREFIXES = ("/opt/homebrew/", "/usr/local/opt/", "/usr/local/Cellar/")


def dependencies(path):
    """The Homebrew libraries `path` links, as written in it, without its own install name."""
    printed = subprocess.run(["otool", "-L", path], check=True, capture_output=True, text=True)
    own = subprocess.run(["otool", "-D", path], check=True, capture_output=True, text=True)
    own_name = own.stdout.strip().splitlines()[-1]
    found = []
    for line in printed.stdout.splitlines()[1:]:
        name = line.strip().split(" (")[0]
        if name.startswith(HOMEBREW_PREFIXES) and name != own_name:
            found.append(name)
    return found


def package(path):
    """The Homebrew package a library file belongs to: the folder after Cellar/ or opt/."""
    real = os.path.realpath(path)
    parts = real.split(os.sep)
    if "Cellar" in parts:
        return parts[parts.index("Cellar") + 1]
    return os.path.basename(path)


def main():
    root, out = sys.argv[1], sys.argv[2]
    os.makedirs(out, exist_ok=True)
    copied = {}
    queue = [root]
    while queue:
        path = queue.pop()
        real = os.path.realpath(path)
        if real in copied:
            continue
        name = os.path.basename(path)
        target = os.path.join(out, name)
        shutil.copyfile(real, target)
        os.chmod(target, 0o755)
        copied[real] = (target, dependencies(real))
        queue.extend(copied[real][1])
    packages = set()
    for real, (target, deps) in copied.items():
        packages.add(package(real))
        subprocess.run(
            ["install_name_tool", "-id", "@loader_path/" + os.path.basename(target), target],
            check=True, capture_output=True,
        )
        for dep in deps:
            new = "@loader_path/" + os.path.basename(dep)
            subprocess.run(
                ["install_name_tool", "-change", dep, new, target],
                check=True, capture_output=True,
            )
        subprocess.run(["codesign", "--sign", "-", "--force", target], check=True,
                       capture_output=True)
    for name in sorted(packages):
        print(name)


if __name__ == "__main__":
    main()
