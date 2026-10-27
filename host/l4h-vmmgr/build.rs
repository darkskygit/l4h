// Embeds guests' l4h-vmm ($L4H_VMM_ELF, built by tools/build-host.sh), an
// empty placeholder when unset.
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=L4H_VERSION");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-env-changed=L4H_VMM_ELF");
    let data = match env::var("L4H_VMM_ELF") {
        Ok(path) if !path.is_empty() => {
            println!("cargo:rerun-if-changed={path}");
            fs::read(&path).unwrap_or_else(|e| panic!("L4H_VMM_ELF={path}: {e}"))
        }
        _ => Vec::new(),
    };
    fs::write(out.join("l4h-vmm.elf"), data).unwrap();
}
