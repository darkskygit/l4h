#!/usr/bin/env python3
"""Preflight and transactional installation; normally invoked by tools/install.sh.

Only the standard library is needed. Probes are read-only; all persistent writes
live in commit(). Keep the device predicates in sync with blkd.rs and netd.rs.
"""

import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import struct
import subprocess
import sys
import tempfile


ESP_TYPE = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
EFI_GLOBAL = "8be4df61-93ca-11d2-aa0d-00e098032b8c"
LOADER = r"\EFI\L4H\l4h.efi"
STATE = Path("/var/lib/l4h/install.json")
CPU_FLAGS = set("lm nx syscall sse sse2 apic msr pae pse pdpe1gb fsgsbase xsave xsaveopt avx constant_tsc".split())


class Incompatible(RuntimeError):
    pass


def need(condition, message):
    if not condition:
        raise Incompatible(message)


def run(*args):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, env=dict(os.environ, LC_ALL="C"))
    need(result.returncode == 0, "{}: {}".format(" ".join(map(str, args)), result.stderr.strip()))
    return result.stdout.strip()


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def boot_state(text):
    def field(name):
        match = re.search(r"^" + name + r":\s*([0-9A-Fa-f,]+)$", text, re.M)
        return match[1].upper() if match else None
    matches = list(re.finditer(r"^Boot([0-9A-Fa-f]{4})(\*?)\s+(.+)$", text, re.M))
    entries = {m[1].upper(): m[3] for m in matches}
    order = field("BootOrder")
    need(order and re.fullmatch(r"[0-9A-F]{4}(,[0-9A-F]{4})*", order),
         "firmware has no readable BootOrder; configure a native Linux boot entry first")
    return {"order": order, "next": field("BootNext"), "current": field("BootCurrent"),
            "entries": entries, "active": sorted(m[1].upper() for m in matches if m[2])}


def l4h_entry(boot):
    entries = [num for num, text in boot["entries"].items()
               if text.split()[0] == "L4H" and LOADER.lower() in text.lower()]
    need(len(entries) <= 1, "multiple L4H boot entries; resolve them with efibootmgr first")
    return entries[0] if entries else None


def owns_entry(boot, entry, plan):
    text = boot["entries"].get(entry, "")
    disk_path = "HD({esp_part},GPT,{esp_partuuid},".format(**plan).lower()
    return (bool(text) and text.split()[0] == "L4H" and LOADER.lower() in text.lower()
            and disk_path in text.lower())


class Preflight:
    def __init__(self, args, root=Path("/"), command=run):
        self.args, self.root, self.run = args, root, command

    def path(self, path):
        return self.root / str(path).lstrip("/")

    def read(self, path):
        return self.path(path).read_text().strip()

    def mounts(self, *args):
        return json.loads(self.run("findmnt", "--json", "--list", *args,
                                   "--output", "SOURCE,TARGET,FSTYPE,OPTIONS,MAJ:MIN"))["filesystems"]

    def mount(self, path):
        entries = self.mounts("--target", str(path))
        need(len(entries) == 1, "cannot identify mount for " + str(path))
        return entries[0]

    def hardware(self):
        need(platform.system() == "Linux" and platform.machine() == "x86_64",
             "only x86_64 Linux is supported")
        need(self.read("/sys/firmware/efi/fw_platform_size") == "64", "64-bit UEFI is required")
        variables = self.mount("/sys/firmware/efi/efivars")
        need(variables["fstype"] == "efivarfs" and "rw" in variables["options"].split(","),
             "efivarfs must be mounted read-write at /sys/firmware/efi/efivars")
        try:
            secure = self.path("/sys/firmware/efi/efivars/SecureBoot-" + EFI_GLOBAL).read_bytes()
        except FileNotFoundError:
            # Non-Secure-Boot-capable firmware (e.g. plain OVMF) has no
            # variable. This is Linux's efi_get_secureboot_mode() rule too.
            secure = b"\0" * 5
        need(len(secure) == 5 and secure[4] == 0,
             "disable Secure Boot in firmware first; the L4H release is unsigned")
        need(self.path("/sys/firmware/acpi/tables/APIC").is_file(), "ACPI MADT (APIC) is required")
        processors = []
        for block in self.read("/proc/cpuinfo").split("\n\n"):
            fields = dict(line.split(":", 1) for line in block.splitlines() if ":" in line)
            fields = {key.strip(): value.strip() for key, value in fields.items()}
            if "processor" in fields:
                processors.append(fields)
        need(processors, "cannot read CPU features")
        flags = set.intersection(*(set(cpu.get("flags", "").split()) for cpu in processors))
        need(CPU_FLAGS <= flags, "CPU is missing required features: " + " ".join(sorted(CPU_FLAGS - flags)))
        need(all(cpu.get("vendor_id") in ("GenuineIntel", "AuthenticAMD") for cpu in processors),
             "only Intel and AMD CPUs are supported by this installer")
        need(self.read("/sys/devices/system/cpu/vulnerabilities/meltdown") == "Not affected",
             "L4H requires a CPU unaffected by Meltdown; a Linux mitigation is insufficient")
        if any(cpu["vendor_id"] == "GenuineIntel" for cpu in processors):
            need("arch_capabilities" in flags, "Intel CPU must expose IA32_ARCH_CAPABILITIES / RDCL_NO")
        variant = self.args.variant or ("generic" if {"pcid", "x2apic"} <= flags else "xapic-nopcid")
        if variant == "generic":
            need({"pcid", "x2apic"} <= flags, "generic requires both PCID and x2APIC")
        need(all(int(cpu.get("apicid", "256")) < 256 for cpu in processors),
             "APIC IDs above 255 are not supported by the shipped interrupt routing")
        total = int(re.search(r"^MemTotal:\s+(\d+)", self.read("/proc/meminfo"), re.M)[1]) // 1024
        memory = self.args.memory if self.args.memory is not None else min(total // 2, total - 2048, 8192)
        need(total >= 3072 and 1024 <= memory <= min(total - 2048, 61440),
             "need at least 3 GiB usable RAM (normally 4 GiB installed) and dom0 RAM of 1024..61440 MiB, leaving 2 GiB for L4H")
        vcpus = self.args.vcpus if self.args.vcpus is not None else min(len(processors), 4)
        need(1 <= vcpus <= min(len(processors), 64), "dom0 vCPUs must fit the available CPUs (maximum 64)")
        return variant, memory, vcpus

    def pci(self, sys_path):
        path = self.path(sys_path).resolve()
        pci = next((p for p in (path, *path.parents)
                    if re.fullmatch(r"[0-9a-f]{4}:[0-9a-f]{2}:[0-9a-f]{2}\.[0-7]", p.name)), None)
        need(pci is not None, str(sys_path) + ": requires a supported PCI device")
        need(pci.name.startswith("0000:"), "only PCI segment 0000 is supported")
        number = lambda name: int((pci / name).read_text().strip(), 16)
        config = (pci / "config").read_bytes()
        need(len(config) >= 64, "cannot read PCI capabilities for " + pci.name)
        # Walk the conventional capability list, with cycle and bounds checks.
        offset, seen, msix, modern, virtio = config[0x34] & ~3, set(), 0, False, False
        while offset:
            need(0x40 <= offset <= min(len(config) - 4, 0xFC) and offset not in seen,
                 "invalid or unreadable PCI capability list for " + pci.name)
            seen.add(offset)
            if config[offset] == 0x11:
                msix = (int.from_bytes(config[offset + 2:offset + 4], "little") & 0x7FF) + 1
            if config[offset] == 9:
                virtio = True
                modern |= config[offset + 3] == 1
            offset = config[offset + 1] & ~3
        bars = []
        for line in (pci / "resource").read_text().splitlines()[:6]:
            start, end, flags = (int(x, 16) for x in line.split())
            bars.append((end - start + 1 if start else 0, bool(flags & 0x100)))
        need(len(bars) == 6, "cannot read PCI BARs for " + pci.name)
        return dict(bdf=pci.name[5:], vendor=number("vendor"), device=number("device"),
                    cls=number("class"), msix=msix, modern=modern, bars=bars,
                    legacy=not virtio and bars[0][1] and bars[0][0] >= 0x20)

    def disk(self, block):
        if block["type"] == "part":
            block = self.blocks.get(block["pkname"])
        need(block is not None and block["type"] == "disk",
             "only plain disks/partitions are supported (no LVM, dm-crypt, RAID or multipath)")
        name = Path(block["name"]).name
        pci = self.pci("/sys/class/block/" + name)
        vendor, device = pci["vendor"], pci["device"]
        virtio = vendor == 0x1AF4 and (
            device in (0x1001, 0x1042) and pci["modern"] or
            device in (0x1004, 0x1048) and (pci["modern"] or device == 0x1004 and pci["legacy"]))
        nvme = pci["cls"] == 0x010802 and pci["bars"][0][0] >= 0x2000
        need(pci["msix"] and (virtio or nvme), block["name"] +
             ": L4H needs NVMe, modern virtio-blk or virtio-scsi with MSI-X; SATA/SAS/USB are unsupported")
        if nvme:
            need(self.read("/sys/class/block/" + name + "/nsid") == "1",
                 block["name"] + ": L4H supports only NVMe namespace 1")
        return pci["bdf"]

    def stable_device(self, spec):
        for prefix, key in (("UUID=", "uuid"), ("PARTUUID=", "partuuid"),
                            ("LABEL=", "label"), ("PARTLABEL=", "partlabel"),
                            ("/dev/disk/by-uuid/", "uuid"), ("/dev/disk/by-partuuid/", "partuuid")):
            if spec.startswith(prefix):
                value = spec[len(prefix):]
                matches = [b for b in self.blocks.values() if b.get(key) == value]
                need(len(matches) == 1, "missing or ambiguous device: " + spec)
                return matches[0]
        raise Incompatible("fstab must use UUID/PARTUUID or unique labels for block devices; unstable source: " + spec)

    def storage(self):
        data = json.loads(self.run("lsblk", "--json", "--list", "--paths", "--output",
                                  "NAME,TYPE,PKNAME,FSTYPE,UUID,PARTUUID,LABEL,PARTLABEL,PARTTYPE,PTTYPE,MAJ:MIN"))
        self.blocks = {b["name"]: b for b in data["blockdevices"]}
        root_mount = self.mount("/")
        matches = [b for b in self.blocks.values() if b["maj:min"] == root_mount["maj:min"]]
        need(len(matches) == 1 and root_mount["fstype"] in ("ext4", "xfs"),
             "root must be a plain ext4 or XFS disk/partition")
        root = matches[0]
        need("rw" in root_mount["options"].split(","), "root must be writable")
        root_bdf = self.disk(root)
        need(root.get("uuid") and re.fullmatch(r"[0-9a-fA-F-]+", root["uuid"]), "root needs a filesystem UUID")
        need(sum(b.get("uuid") == root["uuid"] for b in self.blocks.values()) == 1,
             "root filesystem UUID is duplicated")
        for path in ("/usr", "/etc", "/lib/modules", "/sbin/init"):
            mount = self.mount(path)
            need(mount["maj:min"] == root_mount["maj:min"] and mount["target"] == "/",
                 path + " must be on the root filesystem, without a separate or bind mount")
        for path in ("/lib/modules", "/dev", "/proc", "/sys", "/run"):
            need(self.path(path).is_dir(), "missing root directory: " + path)
        need(os.access(self.path("/sbin/init"), os.X_OK), "no executable /sbin/init")
        crypttab = self.path("/etc/crypttab")
        need(not crypttab.exists() or not any(line.strip() and not line.lstrip().startswith("#")
                                             for line in crypttab.read_text().splitlines()),
             "crypttab entries need a custom initramfs; automatic encrypted-volume migration is unsupported")
        fstab = self.mounts("--fstab")
        need(any(m["target"] == "/" for m in fstab), "fstab needs a persistent root entry")
        for mount in fstab:
            spec, kind, target = mount["source"], mount["fstype"], mount["target"]
            if kind in ("proc", "sysfs", "devtmpfs", "tmpfs", "devpts", "cgroup2"):
                continue
            if kind == "swap" and spec.startswith("/") and not spec.startswith("/dev/"):
                need(self.path(spec).is_file() and self.mount(spec)["maj:min"] == root_mount["maj:min"],
                     "swap file must be on root: " + spec)
                continue
            need(kind in ("ext4", "xfs", "vfat", "swap"), "unsupported fstab filesystem: " + kind)
            block = self.stable_device(spec)
            self.disk(block)
            need(block.get("fstype") == kind, "fstab filesystem differs from the disk: " + spec)
            if target == "/":
                need(block["name"] == root["name"], "fstab root differs from the mounted root")
        # Dom0 can hold 16 devices total, including its vNIC. Count every disk
        # L4H could attach, not just fstab entries, to avoid silently losing root.
        disks = [b for b in self.blocks.values() if b["type"] == "disk"]
        need(len(disks) <= 15, "automatic installation supports at most 15 disks")
        candidates = []
        for mount in self.mounts():
            if mount["fstype"] != "vfat":
                continue
            block = next((b for b in self.blocks.values() if b["maj:min"] == mount["maj:min"]), {})
            if (block.get("parttype") or "").lower() == ESP_TYPE:
                candidates.append((mount, block))
        if self.args.esp:
            candidates = [(m, b) for m, b in candidates if m["target"] == str(Path(self.args.esp).resolve())]
        need(len(candidates) == 1, "need one mounted GPT EFI system partition; use --esp to select it")
        esp, partition = candidates[0]
        need("rw" in esp["options"].split(","), "EFI system partition is read-only")
        disk = self.blocks.get(partition["pkname"], {})
        need(partition["type"] == "part" and disk.get("type") == "disk" and disk.get("pttype") == "gpt",
             "ESP must be a partition on a plain GPT disk")
        self.disk(partition)
        partnum = self.read("/sys/class/block/" + Path(partition["name"]).name + "/partition")
        need(partnum.isdigit() and int(partnum) > 0, "cannot determine the ESP partition number")
        partuuid = partition.get("partuuid") or ""
        need(re.fullmatch(r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}", partuuid),
             "cannot determine the ESP partition UUID")
        return dict(root_uuid=root["uuid"], root_type=root_mount["fstype"], boot_disk=root_bdf,
                    esp=esp["target"], esp_disk=disk["name"], esp_part=partnum, esp_partuuid=partuuid)

    def network(self):
        # One physical NIC retains its MAC; initramfs preserves its Linux name.
        # Bonds, bridges, VLANs and multiple NICs need an explicit migration plan.
        links = json.loads(self.run("ip", "-json", "link", "show"))
        links = [link for link in links if link["ifname"] != "lo"]
        need(len(links) == 1, "automatic migration requires exactly one physical NIC (no bonds, bridges or VLANs)")
        link = links[0]
        name = link["ifname"]
        need(re.fullmatch(r"[A-Za-z0-9_-]{1,15}", name), "unsupported interface name: " + name)
        need(link.get("link_type") == "ether" and "master" not in link and
             not self.path("/sys/class/net/" + name + "/wireless").exists(), "need a plain Ethernet uplink")
        pci = self.pci("/sys/class/net/" + name + "/device")
        vendor, device = pci["vendor"], pci["device"]
        mem = lambda bar: pci["bars"][bar][0] > 0 and not pci["bars"][bar][1]
        supported = (
            vendor == 0x1AF4 and device in (0x1000, 0x1041) and pci["msix"] >= 1
            and (pci["modern"] or device == 0x1000 and pci["legacy"]) or
            vendor == 0x1AE0 and device == 0x0042 and mem(0) and mem(2) and pci["msix"] >= 3 or
            vendor == 0x8086 and device in (0x1563, 0x15D1) and mem(0) and pci["msix"] >= 2 or
            vendor == 0x1D0F and device in (0x0EC2, 0x1EC2, 0xEC20, 0xEC21, 0x0051) and mem(0) and pci["msix"] >= 2)
        need(supported, name + ": supported uplinks are virtio-net, gVNIC, ENA and Intel X550")
        assigned = self.path("/sys/class/net/" + name + "/addr_assign_type")
        need(assigned.read_text().strip() == "0", "uplink MAC has been changed; restore its permanent MAC first")
        return name, pci["bdf"]

    def check(self):
        variant, memory, vcpus = self.hardware()
        plan = self.storage()
        name, uplink = self.network()
        need(self.read("/proc/1/comm") == "systemd", "automatic migration currently requires systemd as PID 1")
        if self.run("systemctl", "get-default") == "graphical.target":
            # Debian cloud images use graphical.target even without a display
            # manager. Reject a real graphical login, not the target's name.
            need(self.run("systemctl", "show", "--property=LoadState", "--value", "display-manager.service")
                 in ("not-found", "masked"),
                 "L4H has no native GPU/USB console; configure a headless system first")
        apparmor = self.path("/sys/kernel/security/apparmor/profiles")
        need(not apparmor.exists() or not apparmor.read_text().strip(),
             "the shipped kernel has no AppArmor; active profiles cannot be preserved")
        need(not list(self.path("/var/lib/dkms").glob("*/[0-9]*")),
             "DKMS modules cannot be reused with L4H's paravirtual kernel")
        selinux = self.path("/sys/fs/selinux/enforce")
        lsm = ""
        if selinux.exists():
            mode = selinux.read_text().strip()
            need(mode in ("0", "1"), "cannot read SELinux mode")
            lsm = " lsm=selinux enforcing=" + mode
        boot = boot_state(self.run("efibootmgr", "--verbose"))
        existing = l4h_entry(boot)
        need(boot["current"] in boot["entries"] and boot["current"] != existing and
             boot["current"] in boot["order"].split(",") and boot["current"] in boot["active"],
             "run this installer from native Linux with its boot entry in BootOrder")
        need(boot["next"] is None or boot["next"] == existing,
             "BootNext is already scheduled for another entry; clear it before installing")
        if existing:
            need(owns_entry(boot, existing, plan), "existing L4H boot entry points to a different ESP")
            need(self.path(STATE).is_file(), "existing L4H boot entry is not managed by this installer")
            saved = json.loads(self.path(STATE).read_text())
            need(saved.get("esp") == plan["esp"] and saved.get("entry") == existing,
                 "existing L4H install is on a different ESP or boot entry")
        else:
            need(not self.path(STATE).exists() and not self.path(plan["esp"] + "/l4h").exists() and
                 not self.path(plan["esp"] + "/EFI/L4H").exists(),
                 "unmanaged L4H files exist; preserve or remove them before installing")
        plan.update(variant=variant, memory=memory, vcpus=vcpus, ifname=name,
                    uplink=uplink, lsm=lsm, boot=boot, entry=existing)
        return plan


def configuration(plan):
    return ("# Generated by the L4H Linux installer.\n"
            "dom0_mem_mb={memory}\ndom0_vcpus={vcpus}\ndom0_io_pool_mb=0\n"
            "dom0_disks=15\ndom0_nics=1\nboot_disk={boot_disk}\nuplink_nic={uplink}\n"
            'dom0_cmdline="console=hvc0 root=UUID={root_uuid} rootfstype={root_type} '
            'ro net.ifnames=0 l4h.ifname={ifname}{lsm}"\n').format(**plan)


def verify_efi(path):
    with path.open("rb") as stream:
        header = stream.read(64)
        need(len(header) == 64 and header[:2] == b"MZ", "release is not a PE executable")
        stream.seek(struct.unpack_from("<I", header, 60)[0])
        pe = stream.read(94)
    need(len(pe) == 94 and pe[:6] == b"PE\0\0\x64\x86" and pe[24:26] == b"\x0b\x02"
         and struct.unpack_from("<H", pe, 92)[0] == 10, "release is not an x86_64 EFI application")


def download_efi(args, plan, dest):
    name = "l4h-" + plan["variant"] + ".efi"
    sums = [line.split() for line in args.checksums.read_text().splitlines()]
    hashes = [line[0] for line in sums if len(line) == 2 and line[1] == name]
    need(len(hashes) == 1 and re.fullmatch(r"[a-f0-9]{64}", hashes[0]), "missing/duplicate EFI checksum")
    run("curl", "--fail", "--silent", "--show-error", "--location", "--proto", "=https",
        "--proto-redir", "=https", "--retry", "3", "--output", str(dest),
        "https://github.com/losfair/l4h/releases/download/" + args.release + "/" + name)
    need(digest(dest) == hashes[0], "EFI download checksum mismatch")
    verify_efi(dest)


def safe_path(path):
    for part in (path, *path.parents):
        need(not part.is_symlink(), "refusing symlink in install path: " + str(part))
    need(not path.exists() or path.is_file(), "not a regular file: " + str(path))


def atomic_copy(source, dest, mode=0o644):
    safe_path(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix=".l4h-", dir=dest.parent)
    try:
        with os.fdopen(fd, "wb") as out, source.open("rb") as stream:
            shutil.copyfileobj(stream, out)
            out.flush()
            os.fsync(out.fileno())
        os.chmod(name, mode)
        os.replace(name, dest)
    finally:
        Path(name).unlink(missing_ok=True)


def commit(plan, package, efi, release, make_default, root=Path("/"), command=run):
    """Back up every destination, publish files, then change NVRAM; undo on failure."""
    path = lambda name: root / str(name).lstrip("/")
    esp = Path(plan["esp"])
    entries = plan["boot"]["entries"]
    entry = plan["entry"] or next(("%04X" % n for n in range(65536) if "%04X" % n not in entries), None)
    need(entry is not None, "no free EFI boot entry")
    with tempfile.TemporaryDirectory() as temp:
        config = Path(temp) / "l4h.conf"
        config.write_text(configuration(plan))
        state = Path(temp) / "install.json"
        state.write_text(json.dumps(dict(esp=plan["esp"], entry=entry, release=release), indent=2) + "\n")
        files = [(efi, esp / "EFI/L4H/l4h.efi", 0o644),
                 (package / "install/initrd", esp / "l4h/dom0/initrd", 0o644),
                 (config, esp / "l4h/l4h.conf", 0o644),
                 (package / "bin/l4hctl", Path("/usr/local/bin/l4hctl"), 0o755),
                 (package / "share/licenses/l4hctl/LICENSE", Path("/usr/local/share/licenses/l4hctl/LICENSE"), 0o644),
                 (state, STATE, 0o600)]
        for source, target, _ in files:
            need(source.is_file() and source.stat().st_size > 0, "release file missing/empty: " + str(source))
            safe_path(path(target))
        missing_dirs = {parent for _, target, _ in files for parent in path(target).parents
                        if not parent.exists()}
        need(shutil.disk_usage(path(esp)).free > sum(s.stat().st_size for s, t, _ in files if t.is_relative_to(esp)) + 16 * 1024**2,
             "not enough free space on the ESP (payload plus 16 MiB required)")
        # A private persistent backup permits recovery even if rollback itself
        # fails (e.g. the firmware refuses further writes or the disk fills).
        backup_base = path("/var/lib/l4h/backups")
        safe_path(backup_base / "placeholder")
        backup_base.mkdir(parents=True, exist_ok=True, mode=0o700)
        backup = Path(tempfile.mkdtemp(prefix=datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ-"), dir=backup_base))
        manifest = []
        for _, target, _ in files:
            dest = path(target)
            saved = backup / str(len(manifest))
            if dest.exists():
                shutil.copy2(dest, saved)
            manifest.append(dict(path=str(target), saved=saved.name if dest.exists() else None,
                                 mode=dest.stat().st_mode & 0o777 if dest.exists() else 0o644))
        (backup / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        (backup / "boot.json").write_text(json.dumps(plan["boot"], indent=2) + "\n")
        os.sync()
        changed, nvram = [], False
        try:
            # Re-read immediately before publishing in case another boot tool ran.
            need(boot_state(command("efibootmgr", "--verbose")) == plan["boot"],
                 "EFI boot state changed during installation; rerun the installer")
            for index, (source, target, mode) in enumerate(files):
                if target == STATE:
                    continue
                changed.append(index)
                atomic_copy(source, path(target), mode)
            os.sync()
            nvram = True
            if plan["entry"] is None:
                command("efibootmgr", "--create-only", "--bootnum", entry, "--disk", plan["esp_disk"],
                        "--part", plan["esp_part"], "--label", "L4H", "--loader", LOADER)
            current = boot_state(command("efibootmgr", "--verbose"))
            need(l4h_entry(current) == entry and owns_entry(current, entry, plan) and current["order"] == plan["boot"]["order"],
                 "firmware did not create the L4H entry without changing BootOrder")
            order = ",".join([entry] + [n for n in plan["boot"]["order"].split(",") if n != entry])
            if make_default:
                command("efibootmgr", "--bootorder", order)
            else:
                command("efibootmgr", "--bootnext", entry)
            final = boot_state(command("efibootmgr", "--verbose"))
            need(final["order"] == (order if make_default else plan["boot"]["order"]) and
                 final["next"] == (plan["boot"]["next"] if make_default else entry) and entry in final["active"],
                 "firmware did not retain the requested boot settings")
            changed.append(len(files) - 1)
            atomic_copy(state, path(STATE), 0o600)
            os.sync()
        except BaseException:
            failures = []
            if nvram:
                commands = []
                if plan["entry"] is None:
                    # Creation can fail because a different boot tool took the
                    # chosen number. Never remove that tool's unrelated entry.
                    try:
                        if owns_entry(boot_state(command("efibootmgr", "--verbose")), entry, plan):
                            commands.append(("efibootmgr", "--bootnum", entry, "--delete-bootnum"))
                    except Exception as error:
                        failures.append("cannot determine whether entry was created: " + str(error))
                commands.append(("efibootmgr", "--bootorder", plan["boot"]["order"]))
                commands.append(("efibootmgr", "--bootnext", plan["boot"]["next"]) if plan["boot"]["next"]
                                else ("efibootmgr", "--delete-bootnext"))
                for cmd in commands:
                    try:
                        command(*cmd)
                    except Exception as error:
                        failures.append(str(error))
            for index in reversed(changed):
                item = manifest[index]
                try:
                    if item["saved"]:
                        atomic_copy(backup / item["saved"], path(item["path"]), item["mode"])
                    else:
                        path(item["path"]).unlink(missing_ok=True)
                except Exception as error:
                    failures.append(str(error))
            for directory in sorted(missing_dirs, key=lambda p: len(p.parts), reverse=True):
                try:
                    directory.rmdir()
                except OSError:
                    pass  # Keep backup directories and anything another tool created.
            os.sync()
            print("l4h: installation failed; backup: " + str(backup), file=sys.stderr)
            if failures:
                print("l4h: rollback incomplete: " + "; ".join(failures), file=sys.stderr)
            raise
    print("Installed {} as Boot{}. Backup: {}".format(release, entry, backup))
    print("BootOrder: " + final["order"])
    print("L4H is the default boot entry." if make_default else
          "L4H is selected for the next boot only; the native Linux BootOrder is preserved.")
    print("Reboot when ready. The existing Linux userspace will run as dom0; no reboot was performed.")


def arguments(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--release")
    parser.add_argument("--checksums", type=Path)
    parser.add_argument("--esp")
    parser.add_argument("--memory", type=int)
    parser.add_argument("--vcpus", type=int)
    parser.add_argument("--variant", choices=("generic", "xapic-nopcid"))
    parser.add_argument("--make-default", action="store_true")
    return parser.parse_args(argv)


def main():
    args = arguments()
    need(os.geteuid() == 0, "run as root to read PCI configuration and EFI variables")
    package = Path(__file__).resolve().parent.parent
    plan = Preflight(args).check()
    print("Compatible: {} on {}, root UUID={}, uplink {} ({}), dom0 {} MiB / {} CPUs".format(
        plan["variant"], plan["esp"], plan["root_uuid"], plan["ifname"], plan["uplink"], plan["memory"], plan["vcpus"]))
    print("Firmware behavior and application compatibility still require a boot test.")
    if args.check:
        return
    need(args.release and re.fullmatch(r"v[A-Za-z0-9][A-Za-z0-9._-]*", args.release) and args.checksums,
         "install through tools/install.sh with a verified release package")
    with tempfile.TemporaryDirectory() as work:
        efi = Path(work) / "l4h.efi"
        download_efi(args, plan, efi)
        # Serialize installers, then repeat preflight before any persistent write.
        with open("/run/l4h-install.lock", "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            need(Preflight(args).check() == plan, "system changed during download; rerun the installer")
            commit(plan, package, efi, args.release, args.make_default)


if __name__ == "__main__":
    try:
        main()
    except (Incompatible, OSError, ValueError, KeyError) as error:
        sys.exit("l4h: " + str(error))
