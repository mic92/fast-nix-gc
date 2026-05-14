//! NAR hash of a path; must match Nix's hashPath() so .links/ interoperates.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use harmonia_nar::NarByteStream;
use harmonia_utils_hash::fmt::CommonHash;
use harmonia_utils_hash::{Algorithm, Context as HashCtx};
use std::path::Path;

/// SHA-256 over the NAR serialisation, bare nix32 — the .links filename.
pub async fn nar_hash_nix32(path: &Path) -> Result<String> {
    let mut stream = NarByteStream::new(path.to_path_buf());
    let mut ctx = HashCtx::new(Algorithm::SHA256);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("dumping NAR for {}", path.display()))?;
        ctx.update(&chunk);
    }
    Ok(ctx.finish().as_base32().as_bare().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    // Reference values from `nix hash path --type sha256 --base32`.
    #[test]
    fn matches_nix_hash_path_regular() {
        let d = tempdir().unwrap();
        let p = d.path().join("f");
        fs::write(&p, "hello world\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(
            run(nar_hash_nix32(&p)).unwrap(),
            "00zns3gj9hwz2a4b0i07y7nmxybq59lh24bl3xsxblcl6333mjil"
        );
    }

    #[test]
    fn matches_nix_hash_path_executable() {
        let d = tempdir().unwrap();
        let p = d.path().join("f");
        fs::write(&p, "hello world\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o555)).unwrap();
        assert_eq!(
            run(nar_hash_nix32(&p)).unwrap(),
            "08dz2j85pn9szqrsnm3r7snlrhwr6wfvs6rl384j8kksb5qv818b"
        );
    }

    #[test]
    fn matches_nix_hash_path_symlink() {
        let d = tempdir().unwrap();
        let p = d.path().join("sym");
        std::os::unix::fs::symlink("../foo", &p).unwrap();
        assert_eq!(
            run(nar_hash_nix32(&p)).unwrap(),
            "1ix20zzkrny4jdydlpfpvi1c1lnrrpm3av3fdjczxq1xah7kawn4"
        );
    }
}
