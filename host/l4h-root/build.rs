// Embeds the dom0 guest images named by $L4H_DOM0_KERNEL / $L4H_DOM0_INITRD
// (empty placeholders when unset) and the dom0 command line.
use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    for (var, file) in [("L4H_DOM0_KERNEL", "dom0-kernel"), ("L4H_DOM0_INITRD", "dom0-initrd")] {
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
}
