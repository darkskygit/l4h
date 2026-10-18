// Embeds the dom0 guest images named by $L4H_DOM0_KERNEL / $L4H_DOM0_INITRD,
// the host components ($L4H_VMM_ELF, $L4H_BLKD_ELF, $L4H_NETD_ELF; built by
// tools/build-host.sh) -- empty placeholders when unset -- and the dom0
// command line.
use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    for (var, file) in [
        ("L4H_DOM0_KERNEL", "dom0-kernel"),
        ("L4H_DOM0_INITRD", "dom0-initrd"),
        ("L4H_VMM_ELF", "l4h-vmm.elf"),
        ("L4H_BLKD_ELF", "l4h-blkd.elf"),
        ("L4H_NETD_ELF", "l4h-netd.elf"),
    ] {
        println!("cargo:rerun-if-env-changed={var}");
        let data = match env::var(var) {
            Ok(path) if !path.is_empty() => {
                println!("cargo:rerun-if-changed={path}");
                fs::read(&path).unwrap_or_else(|e| panic!("{var}={path}: {e}"))
            }
            _ => Vec::new(),
        };
        fs::write(out.join(file), data).unwrap();
    }
    println!("cargo:rerun-if-env-changed=L4H_DOM0_CMDLINE");
    let cmdline = env::var("L4H_DOM0_CMDLINE").unwrap_or_else(|_| "console=hvc0".into());
    fs::write(out.join("dom0-cmdline"), cmdline).unwrap();
    println!("cargo:rerun-if-env-changed=L4H_DOM0_MEM_MB");
    println!("cargo:rerun-if-env-changed=L4H_DOM0_VCPUS");
    println!("cargo:rerun-if-env-changed=L4H_WATCHDOG_SECS");
    println!("cargo:rerun-if-env-changed=L4H_DOM0_DISKS");
    println!("cargo:rerun-if-env-changed=L4H_DOM0_NICS");
    println!("cargo:rerun-if-env-changed=L4H_DOM0_INTERNAL_SWITCHES");
}
