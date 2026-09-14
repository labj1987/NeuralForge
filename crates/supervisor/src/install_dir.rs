//! Where the helper executable and its vendored DXVK DLL live, relative to this
//! binary's own location — matches the AppImage layout the plan's "Build & packaging"
//! section describes (`usr/lib/neuralforge/helper/neuralforge-helper.exe`,
//! `usr/lib/neuralforge/dxvk/...`), not upstream's RPM tree.

use std::path::PathBuf;

fn candidate_install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(explicit) = std::env::var("NEURALFORGE_INSTALL_DIR") {
        dirs.push(PathBuf::from(explicit));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            dirs.push(bin_dir.join("../lib/neuralforge"));
            dirs.push(bin_dir.join("../lib64/neuralforge"));
            dirs.push(bin_dir.to_path_buf());
        }
    }
    dirs.push(PathBuf::from("/usr/lib/neuralforge"));
    dirs.push(PathBuf::from("/usr/lib64/neuralforge"));
    dirs
}

pub fn helper_exe() -> Option<PathBuf> {
    for dir in candidate_install_dirs() {
        let candidate = dir.join("helper/neuralforge-helper.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn dxvk_dll() -> Option<PathBuf> {
    for dir in candidate_install_dirs() {
        let candidate = dir.join("dxvk/vulkan-1.dll");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
