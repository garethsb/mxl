// SPDX-FileCopyrightText: 2025 2025 Contributors to the Media eXchange Layer project.
// SPDX-License-Identifier: Apache-2.0

use std::str::FromStr;

include!(concat!(env!("OUT_DIR"), "/constants.rs"));

pub fn get_mxl_so_path() -> std::path::PathBuf {
    // Resolve via LD_LIBRARY_PATH / the system loader (libloading dlopen).
    // With mxl-not-built, libmxl is built separately and may be installed anywhere
    // (e.g. a container path under /opt), not only under the compile-time build tree.
    "libmxl.so".into()
}

pub fn get_mxl_repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from_str(MXL_REPO_ROOT).expect("build error: 'MXL_REPO_ROOT' is invalid")
}
