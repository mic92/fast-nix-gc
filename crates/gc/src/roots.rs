//! GC root discovery: gcroots/profiles directories, temp roots, and
//! per-process runtime roots (/proc on Linux, libproc on macOS).

use crate::HashSet;
use crate::db::BasenameIndex;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Find all GC root node indices by walking gcroots/profiles directories
/// and scanning running processes.
pub fn find_roots(state_dir: &Path, store_dir: &Path, idx: &BasenameIndex) -> Result<Vec<u32>> {
    let mut roots = HashSet::default();
    let store_prefix = store_dir.to_string_lossy().to_string();

    // Errors here must abort the GC: silently dropping a roots directory
    // (e.g. EACCES) would let the GC delete live paths.
    for dir in [state_dir.join("gcroots"), state_dir.join("profiles")] {
        find_roots_in_dir(&dir, &store_prefix, idx, &mut roots)
            .with_context(|| format!("scanning roots in {}", dir.display()))?;
    }

    // Also scan running processes for runtime roots.
    // Unchecked candidates — validate against the DB before trusting,
    // mirroring Nix's findRuntimeRoots.
    //
    // The kernel reports canonical (symlink-resolved) paths for fds and
    // mappings, but the DB stores the logical store path. Scan with both
    // prefixes and normalize back to logical before validating.
    let canonical_prefix = fs::canonicalize(store_dir)
        .ok()
        .map(|p| p.to_string_lossy().into_owned());

    let mut candidates = find_runtime_roots(&store_prefix);
    if let Some(canon) = &canonical_prefix
        && canon != &store_prefix
    {
        for c in find_runtime_roots(canon) {
            // Rebase canonical store path back to logical prefix.
            if let Some(rest) = c.strip_prefix(canon.as_str()) {
                candidates.insert(format!("{}{}", store_prefix, rest));
            }
        }
    }

    for candidate in candidates {
        if let Some(idx) = idx.idx_of(&candidate) {
            roots.insert(idx);
        }
    }

    Ok(roots.into_iter().collect())
}

fn find_roots_in_dir(
    dir: &Path,
    store_prefix: &str,
    idx: &BasenameIndex,
    roots: &mut HashSet<u32>,
) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        // A missing roots dir contributes no roots; anything else (EACCES,
        // EIO) hides an unknown number of roots and must fail the GC.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };

    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            // Entry removed while scanning (e.g. a stale auto link another
            // process cleaned up) — not a root anymore.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
        };

        if meta.file_type().is_symlink() {
            let target = match fs::read_link(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("readlink {}", path.display())),
            };
            let target_str = target.to_string_lossy();

            if is_in_store(store_prefix, &target_str) {
                // Direct root pointing into store
                if let Some(sp) = extract_store_path(store_prefix, &target_str)
                    && let Some(idx) = idx.idx_of(&sp)
                {
                    roots.insert(idx);
                }
            } else {
                // Indirect root: symlink -> symlink -> store.
                // Nix's findRoots resolves at most one extra hop. Use
                // symlink_metadata to avoid recursive follow / symlink loops.
                let abs_target = if target.is_absolute() {
                    target.clone()
                } else {
                    dir.join(&target)
                };
                // metadata() (stat, follows) returns ENOENT for dangling
                // links and ELOOP for cycles — both are "target gone".
                // Any other error (EACCES, EIO) says nothing about the
                // target's existence: removing the root then would let the
                // GC delete a live path.
                if let Err(e) = fs::metadata(&abs_target) {
                    let target_gone = e.kind() == std::io::ErrorKind::NotFound
                        || e.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32);
                    if !target_gone {
                        return Err(e).with_context(|| format!("stat {}", abs_target.display()));
                    }
                    // Component match, not substring: "gcroots/automatic"
                    // must not qualify.
                    if dir.ends_with("gcroots/auto") {
                        log::info!("removing stale link {}", path.display());
                        fs::remove_file(&path).ok();
                    }
                    continue;
                }
                if abs_target
                    .symlink_metadata()
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
                    && let Ok(target2) = fs::read_link(&abs_target)
                {
                    let t2_str = target2.to_string_lossy();
                    if is_in_store(store_prefix, &t2_str)
                        && let Some(sp) = extract_store_path(store_prefix, &t2_str)
                        && let Some(idx) = idx.idx_of(&sp)
                    {
                        roots.insert(idx);
                    }
                }
            }
        } else if meta.file_type().is_dir() {
            find_roots_in_dir(&path, store_prefix, idx, roots)?;
        } else if meta.file_type().is_file() {
            // Regular file root (e.g. in auto-roots)
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let candidate = format!("{}/{}", store_prefix, name);
            if let Some(idx) = idx.idx_of(&candidate) {
                roots.insert(idx);
            }
        }
    }
    Ok(())
}

/// Extract the top-level store path from a potentially deeper path.
/// e.g. "/nix/store/abc...-foo/bin/bar" -> "/nix/store/abc...-foo".
/// Validates the basename looks like a store path so we never treat
/// `..`, `.links`, or other directory entries as candidate roots.
fn extract_store_path(store_prefix: &str, full_path: &str) -> Option<String> {
    let rest = full_path.strip_prefix(store_prefix)?.strip_prefix('/')?;
    let name = rest.split('/').next()?;
    if !is_store_path_basename(name) {
        return None;
    }
    Some(format!("{store_prefix}/{name}"))
}

/// True if `name` matches the store-path basename grammar:
/// `<nix32hash>-<name>` where the hash is 32 chars of `[0-9a-z]`.
fn is_store_path_basename(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 34 || bytes[32] != b'-' {
        return false;
    }
    bytes[..32]
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name[33..].chars().all(is_store_path_char)
}

/// True for chars allowed in a Nix store path basename.
/// Mirrors Nix's storePathRegex: `[0-9a-z]+[0-9a-zA-Z+\-._?=]*`.
/// We accept the union since we extract only the first path component.
fn is_store_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_' | '?' | '=')
}

/// True if `path` is inside the store directory (not just a string prefix
/// like `/nix/store-other`). The next char after the prefix must be '/'.
fn is_in_store(store_prefix: &str, path: &str) -> bool {
    path.strip_prefix(store_prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Add an absolute path as an unchecked candidate root if it lies in the store.
fn add_unchecked(store_prefix: &str, target: &str, unchecked: &mut HashSet<String>) {
    if is_in_store(store_prefix, target)
        && let Some(sp) = extract_store_path(store_prefix, target)
    {
        unchecked.insert(sp);
    }
}

/// Scan a blob (e.g. environ) for embedded store paths using the
/// store-path char alphabet, not arbitrary delimiters.
fn scan_blob_for_store_paths(blob: &str, store_prefix: &str, unchecked: &mut HashSet<String>) {
    let prefix = format!("{}/", store_prefix);
    let mut search_from = 0;
    while let Some(idx) = blob[search_from..].find(&prefix) {
        let abs = search_from + idx;
        let after = abs + prefix.len();
        let end = blob[after..]
            .find(|c: char| !is_store_path_char(c))
            .map(|e| after + e)
            .unwrap_or(blob.len());
        // A bare prefix (end == after) is rejected by add_unchecked's
        // basename validation, no need to special-case it.
        add_unchecked(store_prefix, &blob[abs..end], unchecked);
        // end >= after > abs, so progress is guaranteed.
        search_from = end;
    }
}

/// Scan running processes for store paths they reference.
/// Mirrors Nix's `findRuntimeRootsUnchecked`. Returned candidate paths
/// are *unchecked* — caller must validate against the DB before trusting.
fn find_runtime_roots(store_prefix: &str) -> HashSet<String> {
    let mut unchecked = HashSet::default();
    runtime_roots::scan(store_prefix, &mut unchecked);
    unchecked
}

#[cfg(target_os = "linux")]
mod runtime_roots {
    use super::{add_unchecked, scan_blob_for_store_paths};
    use crate::HashSet;
    use std::fs;
    use std::path::Path;

    /// Read a /proc symlink, swallowing transient errors (process exited, no perms).
    fn read_proc_link(path: &Path, store_prefix: &str, unchecked: &mut HashSet<String>) {
        if let Ok(target) = fs::read_link(path)
            && target.is_absolute()
        {
            add_unchecked(store_prefix, &target.to_string_lossy(), unchecked);
        }
    }

    /// Read a /proc/sys file whose content is a path (e.g. /proc/sys/kernel/modprobe).
    fn read_file_root(path: &Path, store_prefix: &str, unchecked: &mut HashSet<String>) {
        if let Ok(content) = fs::read_to_string(path) {
            add_unchecked(store_prefix, content.trim(), unchecked);
        }
    }

    pub fn scan(store_prefix: &str, unchecked: &mut HashSet<String>) {
        let entries = match fs::read_dir("/proc") {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let pid = entry.file_name().to_string_lossy().to_string();
            if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let pid_dir = entry.path();

            read_proc_link(&pid_dir.join("exe"), store_prefix, unchecked);
            read_proc_link(&pid_dir.join("cwd"), store_prefix, unchecked);

            // /proc/<pid>/fd/*
            if let Ok(fds) = fs::read_dir(pid_dir.join("fd")) {
                for fd in fds.flatten() {
                    if !fd.file_name().to_string_lossy().starts_with('.') {
                        read_proc_link(&fd.path(), store_prefix, unchecked);
                    }
                }
            }

            // /proc/<pid>/maps: 6th whitespace-separated field is the mapped file.
            if let Ok(maps) = fs::read_to_string(pid_dir.join("maps")) {
                for line in maps.lines() {
                    if let Some(file) = line.split_whitespace().nth(5)
                        && file.starts_with('/')
                    {
                        add_unchecked(store_prefix, file, unchecked);
                    }
                }
            }

            // /proc/<pid>/environ
            if let Ok(env_data) =
                fs::read(pid_dir.join("environ")).map(|d| String::from_utf8_lossy(&d).into_owned())
            {
                scan_blob_for_store_paths(&env_data, store_prefix, unchecked);
            }
        }

        // Kernel helper paths can also pin store entries.
        for f in [
            "/proc/sys/kernel/modprobe",
            "/proc/sys/kernel/fbsplash",
            "/proc/sys/kernel/poweroff_cmd",
        ] {
            read_file_root(Path::new(f), store_prefix, unchecked);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn read_file_root_trims_and_extracts() {
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("modprobe");
            let sp = "/nix/store/00000000000000000000000000000000-modprobe";
            fs::write(&f, format!("{sp}/bin/modprobe\n")).unwrap();
            let mut set = HashSet::default();
            read_file_root(&f, "/nix/store", &mut set);
            assert!(set.contains(sp), "{set:?}");
        }

        #[test]
        fn read_file_root_ignores_non_store_paths() {
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("modprobe");
            fs::write(&f, "/sbin/modprobe\n").unwrap();
            let mut set = HashSet::default();
            read_file_root(&f, "/nix/store", &mut set);
            assert!(set.is_empty());
        }
    }
}

/// macOS: libproc syscalls instead of shelling out to lsof.
#[cfg(target_os = "macos")]
mod runtime_roots {
    use super::{add_unchecked, scan_blob_for_store_paths};
    use crate::HashSet;
    use std::ffi::CStr;
    use std::os::raw::{c_int, c_void};

    const PROC_ALL_PIDS: u32 = 1;
    const PROC_PIDLISTFDS: c_int = 1;
    const PROC_PIDVNODEPATHINFO: c_int = 9;
    const PROC_PIDREGIONPATHINFO: c_int = 8;
    const PROC_PIDFDVNODEPATHINFO: c_int = 1;
    const PROX_FDTYPE_VNODE: u32 = 1;
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;
    const MAXPATHLEN: usize = 1024;
    // sysctl
    const CTL_KERN: c_int = 1;
    const KERN_PROCARGS2: c_int = 49;
    const KERN_ARGMAX: c_int = 8;

    unsafe extern "C" {
        fn proc_listpids(
            type_: u32,
            typeinfo: u32,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
        fn proc_pidfdinfo(
            pid: c_int,
            fd: c_int,
            flavor: c_int,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
        fn sysctl(
            name: *mut c_int,
            namelen: u32,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcFdInfo {
        proc_fd: i32,
        proc_fdtype: u32,
    }

    /// Layout of the prefix of struct vnode_info_path / vnode_fdinfowithpath /
    /// proc_regionwithpathinfo: lots of opaque fields, then a NUL-terminated
    /// path at a known offset. We only read the path; using a fixed-size
    /// scratch buffer avoids replicating the full struct definitions.
    fn extract_cstr_path(buf: &[u8]) -> Option<String> {
        // Path is the trailing MAXPATHLEN bytes; find first NUL there.
        if buf.len() < MAXPATHLEN {
            return None;
        }
        let path_bytes = &buf[buf.len() - MAXPATHLEN..];
        let cstr = CStr::from_bytes_until_nul(path_bytes).ok()?;
        let s = cstr.to_str().ok()?;
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    fn list_pids() -> Vec<i32> {
        unsafe {
            let count = proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0);
            if count <= 0 {
                return Vec::new();
            }
            let mut pids = vec![0i32; count as usize / std::mem::size_of::<i32>()];
            let bytes = proc_listpids(
                PROC_ALL_PIDS,
                0,
                pids.as_mut_ptr() as *mut c_void,
                (pids.len() * std::mem::size_of::<i32>()) as c_int,
            );
            if bytes <= 0 {
                return Vec::new();
            }
            pids.truncate(bytes as usize / std::mem::size_of::<i32>());
            pids.retain(|&p| p > 0);
            pids
        }
    }

    fn pid_exe(pid: i32, store_prefix: &str, unchecked: &mut HashSet<String>) {
        let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
        let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
        if n > 0 {
            if let Ok(s) = std::str::from_utf8(&buf[..n as usize]) {
                add_unchecked(store_prefix, s, unchecked);
            }
        }
    }

    /// proc_vnodepathinfo holds cwd then root, each ending with a path.
    /// sizeof(struct vnode_info_path) = sizeof(struct vnode_info)=152 + MAXPATHLEN = 1176.
    const VNODE_INFO_PATH_SIZE: usize = 152 + MAXPATHLEN;

    fn pid_cwd_root(pid: i32, store_prefix: &str, unchecked: &mut HashSet<String>) {
        // struct proc_vnodepathinfo { vnode_info_path pvi_cdir; vnode_info_path pvi_rdir; }
        let mut buf = vec![0u8; VNODE_INFO_PATH_SIZE * 2];
        let n = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDVNODEPATHINFO,
                0,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as c_int,
            )
        };
        if n <= 0 {
            return;
        }
        for chunk in buf.chunks(VNODE_INFO_PATH_SIZE) {
            if let Some(p) = extract_cstr_path(chunk) {
                add_unchecked(store_prefix, &p, unchecked);
            }
        }
    }

    /// sizeof(struct vnode_fdinfowithpath) = sizeof(proc_fileinfo)=24
    /// + sizeof(vnode_info_path)=1176 = 1200.
    const VNODE_FDINFO_SIZE: usize = 24 + VNODE_INFO_PATH_SIZE;

    fn pid_fds(pid: i32, store_prefix: &str, unchecked: &mut HashSet<String>) {
        let n = unsafe { proc_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if n <= 0 {
            return;
        }
        let count = n as usize / std::mem::size_of::<ProcFdInfo>();
        let mut fds = vec![
            ProcFdInfo {
                proc_fd: 0,
                proc_fdtype: 0
            };
            count
        ];
        let n = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr() as *mut c_void,
                (fds.len() * std::mem::size_of::<ProcFdInfo>()) as c_int,
            )
        };
        if n <= 0 {
            return;
        }
        let count = n as usize / std::mem::size_of::<ProcFdInfo>();
        let mut buf = vec![0u8; VNODE_FDINFO_SIZE];
        for fd in &fds[..count] {
            if fd.proc_fdtype != PROX_FDTYPE_VNODE {
                continue;
            }
            buf.fill(0);
            let r = unsafe {
                proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as c_int,
                )
            };
            if r > 0 {
                if let Some(p) = extract_cstr_path(&buf) {
                    add_unchecked(store_prefix, &p, unchecked);
                }
            }
        }
    }

    /// sizeof(struct proc_regionwithpathinfo) = sizeof(proc_regioninfo)=96
    /// + sizeof(vnode_info_path)=1176 = 1272.
    const PROC_REGIONINFO_SIZE: usize = 96;
    const REGION_PATH_INFO_SIZE: usize = PROC_REGIONINFO_SIZE + VNODE_INFO_PATH_SIZE;
    /// proc_regioninfo trailing fields: pri_address (u64), pri_size (u64).
    const PRI_ADDRESS_OFFSET: usize = PROC_REGIONINFO_SIZE - 16;
    const PRI_SIZE_OFFSET: usize = PROC_REGIONINFO_SIZE - 8;

    fn pid_regions(pid: i32, store_prefix: &str, unchecked: &mut HashSet<String>) {
        let mut addr: u64 = 0;
        let mut buf = vec![0u8; REGION_PATH_INFO_SIZE];
        // Iterate region by region. Each call returns info for the region
        // containing/after `addr`; bump addr past it. Cap iterations to
        // avoid pathological loops.
        for _ in 0..8192 {
            buf.fill(0);
            let r = unsafe {
                proc_pidinfo(
                    pid,
                    PROC_PIDREGIONPATHINFO,
                    addr,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as c_int,
                )
            };
            if r <= 0 {
                break;
            }
            let pri_address = u64::from_ne_bytes(
                buf[PRI_ADDRESS_OFFSET..PRI_ADDRESS_OFFSET + 8]
                    .try_into()
                    .unwrap(),
            );
            let pri_size = u64::from_ne_bytes(
                buf[PRI_SIZE_OFFSET..PRI_SIZE_OFFSET + 8]
                    .try_into()
                    .unwrap(),
            );
            if let Some(p) = extract_cstr_path(&buf) {
                add_unchecked(store_prefix, &p, unchecked);
            }
            let next = pri_address.saturating_add(pri_size.max(4096));
            if next <= addr {
                break;
            }
            addr = next;
        }
    }

    fn pid_environ(pid: i32, argmax: usize, store_prefix: &str, unchecked: &mut HashSet<String>) {
        let mut mib = [CTL_KERN, KERN_PROCARGS2, pid];
        let mut buf = vec![0u8; argmax];
        let mut size = argmax;
        let r = unsafe {
            sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buf.as_mut_ptr() as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            return;
        }
        // KERN_PROCARGS2 returns argc(4 bytes) + exec_path + NULs + argv + envp.
        // Just scan whole blob for store path substrings.
        let size = size.min(buf.len());
        let blob = String::from_utf8_lossy(&buf[..size]);
        scan_blob_for_store_paths(&blob, store_prefix, unchecked);
    }

    fn kern_argmax() -> usize {
        let mut mib = [CTL_KERN, KERN_ARGMAX];
        let mut argmax: c_int = 0;
        let mut size = std::mem::size_of::<c_int>();
        let r = unsafe {
            sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                &mut argmax as *mut c_int as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if r == 0 && argmax > 0 {
            argmax as usize
        } else {
            // sane fallback
            256 * 1024
        }
    }

    pub fn scan(store_prefix: &str, unchecked: &mut HashSet<String>) {
        let argmax = kern_argmax();
        for pid in list_pids() {
            pid_exe(pid, store_prefix, unchecked);
            pid_cwd_root(pid, store_prefix, unchecked);
            pid_fds(pid, store_prefix, unchecked);
            pid_regions(pid, store_prefix, unchecked);
            pid_environ(pid, argmax, store_prefix, unchecked);
        }
    }
}

/// Other platforms: no runtime root detection.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod runtime_roots {
    use crate::HashSet;
    pub fn scan(_store_prefix: &str, _unchecked: &mut HashSet<String>) {}
}

/// Find temp roots from the temproots directory.
/// Each file is named by the PID that wrote it and contains NUL-terminated
/// store paths. A file whose owning process has died is stale: we can
/// acquire a write lock on it (the owner held one). Stale files are removed
/// and their roots ignored, mirroring Nix's `findTempRoots`.
pub fn find_temp_roots(state_dir: &Path) -> Result<HashSet<String>> {
    let mut roots = HashSet::default();
    let temp_dir = state_dir.join("temproots");

    let entries = match fs::read_dir(&temp_dir) {
        Ok(e) => e,
        Err(_) => return Ok(roots),
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Hidden files (e.g. portage's .keep) and non-PID names are not
        // temp root files.
        if name.starts_with('.') || !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let path = entry.path();
        let f = match fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            // Owner exited and the file was cleaned up meanwhile.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // Anything else (EACCES, EIO) hides live roots: deleting their
            // targets would yank paths from under a running builder.
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };

        // Owner holds a write lock while alive; if we can take it, it's stale.
        if let Ok(mut lock) =
            nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusiveNonblock)
        {
            log::info!("removing stale temporary roots file {}", path.display());
            fs::remove_file(&path).ok();
            // Nix protocol (gc.cc): write "d" after unlinking so a client
            // that re-acquires its fd sees the marker and recreates the
            // file instead of writing roots into an unlinked inode the GC
            // will never read.
            use std::io::Write;
            let _ = lock.write_all(b"d");
            // lock dropped here, releasing flock after unlink
            continue;
        }

        let contents = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;

        // Each path is NUL-terminated. A trailing run without a NUL is a
        // partial write from a live builder — drop it.
        let Some(end) = contents.iter().rposition(|&b| b == 0) else {
            continue;
        };
        for segment in contents[..end].split(|&b| b == 0) {
            if segment.is_empty() {
                continue;
            }
            if let Ok(s) = std::str::from_utf8(segment) {
                roots.insert(s.to_string());
            }
        }
    }

    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "abcdefghijklmnopqrstuvwxyz012345"; // 32 chars

    #[test]
    fn store_path_basename_grammar() {
        assert!(is_store_path_basename(&format!("{HASH}-foo")));
        assert!(is_store_path_basename(&format!("{HASH}-foo-1.2+x_y?z=")));
        // Too short / missing dash at position 32.
        assert!(!is_store_path_basename("short"));
        assert!(!is_store_path_basename(&format!("{HASH}xfoo")));
        // Uppercase or invalid chars in hash part.
        assert!(!is_store_path_basename(
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345-foo"
        ));
        // Invalid char in name part: both halves must hold.
        assert!(!is_store_path_basename(&format!("{HASH}-foo bar")));
        // .links and .. must never look like store paths.
        assert!(!is_store_path_basename(".links"));
        assert!(!is_store_path_basename(".."));
    }

    #[test]
    fn is_in_store_requires_separator() {
        assert!(is_in_store("/nix/store", "/nix/store/abc"));
        assert!(!is_in_store("/nix/store", "/nix/store"));
        assert!(!is_in_store("/nix/store", "/nix/store-other/abc"));
        assert!(!is_in_store("/nix/store", "/somewhere/else"));
    }

    #[test]
    fn extract_store_path_takes_first_component() {
        let sp = format!("/nix/store/{HASH}-foo");
        assert_eq!(
            extract_store_path("/nix/store", &format!("{sp}/bin/bar")),
            Some(sp.clone())
        );
        assert_eq!(extract_store_path("/nix/store", &sp), Some(sp));
        assert_eq!(
            extract_store_path("/nix/store", "/nix/store/.links/x"),
            None
        );
        assert_eq!(extract_store_path("/nix/store", "/nix/store"), None);
    }

    #[test]
    fn scan_blob_finds_embedded_store_paths() {
        let sp1 = format!("/nix/store/{HASH}-foo");
        let sp2 = format!("/nix/store/{HASH}-bar");
        // Paths embedded mid-blob with trailing junk, and one ending the blob.
        let blob = format!("PATH={sp1}/bin:other LD={sp2}");
        let mut set = HashSet::default();
        scan_blob_for_store_paths(&blob, "/nix/store", &mut set);
        assert_eq!(set.len(), 2, "{set:?}");
        assert!(set.contains(&sp1));
        assert!(set.contains(&sp2));
    }

    #[test]
    fn scan_blob_ignores_bare_prefix() {
        let mut set = HashSet::default();
        scan_blob_for_store_paths("x /nix/store/ y /nix/store::", "/nix/store", &mut set);
        assert!(set.is_empty(), "{set:?}");
    }

    #[test]
    fn temp_roots_reads_locked_skips_stale_and_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("temproots");
        fs::create_dir_all(&dir).unwrap();

        let sp1 = format!("/nix/store/{HASH}-live1");
        let sp2 = format!("/nix/store/{HASH}-live2");
        // Live file: owner (us) holds the write lock; trailing segment
        // without NUL is a partial write and must be dropped.
        let live = dir.join("12345");
        fs::write(&live, format!("{sp1}\0{sp2}\0partial")).unwrap();
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&live)
            .unwrap();
        let _lock =
            nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusiveNonblock).unwrap();

        // Stale file: no lock held, must be removed and ignored.
        let stale = dir.join("999");
        fs::write(&stale, format!("/nix/store/{HASH}-stale\0")).unwrap();

        // Hidden and non-PID files are not temp roots.
        fs::write(dir.join(".keep"), b"junk").unwrap();
        fs::write(dir.join("notapid"), format!("/nix/store/{HASH}-junk\0")).unwrap();

        // Keep an fd on the stale file to observe the "d" marker.
        let stale_fd = fs::File::open(&stale).unwrap();

        let roots = find_temp_roots(tmp.path()).unwrap();
        assert!(roots.contains(&sp1));
        assert!(roots.contains(&sp2));
        assert_eq!(roots.len(), 2, "{roots:?}");
        assert!(!stale.exists(), "stale temp roots file removed");
        // Nix clients detect deletion by reading back a "d" marker.
        {
            use std::io::{Read, Seek};
            let mut f = stale_fd;
            f.seek(std::io::SeekFrom::Start(0)).unwrap();
            let mut b = [0u8; 1];
            f.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"d", "missing deletion marker");
        }
        assert!(dir.join(".keep").exists());
        assert!(dir.join("notapid").exists());
    }

    #[test]
    fn temp_roots_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_temp_roots(tmp.path()).unwrap().is_empty());
    }
}
