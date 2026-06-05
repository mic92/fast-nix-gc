//! Temp-root client, protocol-compatible with Nix's `addTempRoot`
//! (gc.cc): per-process `temproots/<pid>` file holding NUL-terminated
//! store paths, registered either under a momentary shared `gc.lock` or
//! through the running GC's `gc-socket`.

use anyhow::{Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

pub struct TempRoots {
    /// `temproots/<pid>`; we hold the exclusive flock for our lifetime so
    /// a concurrent GC treats the file as live.
    file: fs::File,
    /// `gc.lock` fd; locked shared only for the duration of each `add`.
    gc_lock: fs::File,
    socket_path: PathBuf,
    socket: Option<UnixStream>,
}

fn flock(f: &fs::File, op: i32) -> std::io::Result<()> {
    loop {
        if unsafe { nix::libc::flock(f.as_raw_fd(), op) } == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(nix::libc::EINTR) {
            return Err(e);
        }
    }
}

impl TempRoots {
    /// Create (or take over) this process's temp roots file, mirroring
    /// Nix's `createTempRootsFile`.
    pub fn create(state_dir: &Path) -> Result<TempRoots> {
        use std::os::unix::fs::OpenOptionsExt;

        let dir = state_dir.join("temproots");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(std::process::id().to_string());

        let file = loop {
            // An existing file with our pid must be stale (no two live
            // processes share a pid).
            let _ = fs::remove_file(&path);
            let f = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            flock(&f, nix::libc::LOCK_EX).with_context(|| format!("locking {}", path.display()))?;
            // Non-empty means the GC wrote its "d" marker and unlinked the
            // file before we got the lock; retry on a fresh inode.
            if f.metadata()?.len() == 0 {
                break f;
            }
        };

        let gc_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(state_dir.join("gc.lock"))
            .with_context(|| format!("opening {}", state_dir.join("gc.lock").display()))?;

        Ok(TempRoots {
            file,
            gc_lock,
            socket_path: state_dir.join("gc-socket/socket"),
            socket: None,
        })
    }

    /// Register `store_path` (full path) as a temp root. After this
    /// returns, no GC will delete the path while this process lives.
    pub fn add(&mut self, store_path: &str) -> Result<()> {
        loop {
            // Shared gc.lock acquirable means no GC is running; holding it
            // across the file write keeps one from starting mid-register.
            if flock(&self.gc_lock, nix::libc::LOCK_SH | nix::libc::LOCK_NB).is_ok() {
                let res = self.write_root(store_path);
                let _ = flock(&self.gc_lock, nix::libc::LOCK_UN);
                return res;
            }
            // GC running: hand the root to it over the gc-socket. The
            // socket may vanish at any time (GC finished); restart then.
            match self.notify_gc(store_path)? {
                true => return self.write_root(store_path),
                false => continue,
            }
        }
    }

    fn write_root(&mut self, store_path: &str) -> Result<()> {
        self.file
            .write_all(format!("{store_path}\0").as_bytes())
            .context("writing temp root")
    }

    /// Send the root to the running GC; Ok(false) means the GC went away
    /// and the caller should restart with the lock.
    fn notify_gc(&mut self, store_path: &str) -> Result<bool> {
        if self.socket.is_none() {
            match UnixStream::connect(&self.socket_path) {
                Ok(s) => self.socket = Some(s),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    // GC exited or hasn't created the socket yet.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    return Ok(false);
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("connecting to {}", self.socket_path.display()));
                }
            }
        }
        let sock = self.socket.as_mut().expect("socket set above");
        let gone = |e: &std::io::Error| {
            matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            )
        };
        let io = (|| -> std::io::Result<()> {
            sock.write_all(format!("{store_path}\n").as_bytes())?;
            let mut ack = [0u8; 1];
            sock.read_exact(&mut ack)?;
            if ack != [b'1'] {
                return Err(std::io::Error::other("unexpected gc-socket ack"));
            }
            Ok(())
        })();
        match io {
            Ok(()) => Ok(true),
            Err(e) if gone(&e) => {
                self.socket = None;
                Ok(false)
            }
            Err(e) => Err(e).context("talking to gc-socket"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_writes_nul_terminated_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tr = TempRoots::create(tmp.path()).unwrap();
        tr.add("/nix/store/aaa-x").unwrap();
        tr.add("/nix/store/bbb-y").unwrap();

        let path = tmp
            .path()
            .join("temproots")
            .join(std::process::id().to_string());
        let data = fs::read(&path).unwrap();
        assert_eq!(data, b"/nix/store/aaa-x\0/nix/store/bbb-y\0");
        // We hold the write lock, so the file reads as live to a GC.
        let f = fs::File::open(&path).unwrap();
        assert!(flock(&f, nix::libc::LOCK_EX | nix::libc::LOCK_NB).is_err());
    }

    #[test]
    fn add_uses_gc_socket_when_gc_holds_the_lock() {
        use std::io::BufRead;
        use std::os::unix::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();

        // Simulate a running GC: exclusive gc.lock + listening socket.
        let gc_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(tmp.path().join("gc.lock"))
            .unwrap();
        flock(&gc_lock, nix::libc::LOCK_EX).unwrap();
        fs::create_dir_all(tmp.path().join("gc-socket")).unwrap();
        let listener = UnixListener::bind(tmp.path().join("gc-socket/socket")).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream)
                .read_line(&mut line)
                .unwrap();
            writer.write_all(b"1").unwrap();
            line
        });

        let mut tr = TempRoots::create(tmp.path()).unwrap();
        tr.add("/nix/store/ccc-z").unwrap();

        assert_eq!(server.join().unwrap(), "/nix/store/ccc-z\n");
        let path = tmp
            .path()
            .join("temproots")
            .join(std::process::id().to_string());
        assert_eq!(fs::read(&path).unwrap(), b"/nix/store/ccc-z\0");
    }
}
