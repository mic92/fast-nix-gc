use anyhow::{Context, Result, bail};
use fast_nix_gc::{db, format_size, gc, profiles};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

struct Args {
    delete_old: bool,
    delete_older_than: Option<String>,
    dry_run: bool,
    ensure_free: Option<u64>,
    keep_recent: Option<String>,
    store_dir: PathBuf,
    state_dir: PathBuf,
}

/// Parse a size like "50G", "512M", "1024K", or plain bytes.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1024u64),
        Some('M' | 'm') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some('T' | 't') => (&s[..s.len() - 1], 1024u64.pow(4)),
        _ => (s, 1),
    };
    let n: f64 = num.parse().with_context(|| format!("invalid size '{s}'"))?;
    Ok((n * mult as f64) as u64)
}

/// Free bytes on the filesystem containing `path`.
fn available_bytes(path: &Path) -> Result<u64> {
    let st =
        nix::sys::statvfs::statvfs(path).with_context(|| format!("statvfs {}", path.display()))?;
    // statvfs field types differ between Linux (u64) and macOS (u32).
    Ok(st.blocks_available() as u64 * st.fragment_size() as u64)
}

fn parse_args() -> Result<Args> {
    let mut pargs = pico_args::Arguments::from_env();

    if pargs.contains("--help") {
        eprintln!("Usage: fast-nix-gc [OPTIONS]");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  -d, --delete-old             Remove old profile generations");
        eprintln!("      --delete-older-than SPEC  Delete generations older than SPEC (e.g. 30d)");
        eprintln!("      --dry-run                 Show what would be done");
        eprintln!("      --ensure-free SIZE           Free until SIZE is available (e.g. 50G)");
        eprintln!("      --keep-recent SPEC        Keep paths registered within SPEC (e.g. 7d)");
        eprintln!("      --store-dir PATH          Nix store directory [default: /nix/store]");
        eprintln!("      --state-dir PATH          Nix state directory [default: /nix/var/nix]");
        std::process::exit(0);
    }

    let delete_older_than: Option<String> = pargs.opt_value_from_str("--delete-older-than")?;
    let delete_old =
        pargs.contains("-d") || pargs.contains("--delete-old") || delete_older_than.is_some();

    Ok(Args {
        delete_old,
        delete_older_than,
        dry_run: pargs.contains("--dry-run"),
        ensure_free: pargs.opt_value_from_fn("--ensure-free", parse_size)?,
        keep_recent: pargs.opt_value_from_str("--keep-recent")?,
        store_dir: pargs
            .opt_value_from_str("--store-dir")?
            .unwrap_or_else(|| PathBuf::from("/nix/store")),
        state_dir: pargs
            .opt_value_from_str("--state-dir")?
            .unwrap_or_else(|| PathBuf::from("/nix/var/nix")),
    })
}

/// Minimal stderr logger: `[LEVEL] message`. Level controlled by
/// RUST_LOG=error|warn|info|debug|trace (default: info).
struct StderrLogger(log::LevelFilter);

impl log::Log for StderrLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.0
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{:5}] {}", record.level(), record.args());
        }
    }
    fn flush(&self) {}
}

fn main() -> Result<()> {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(log::LevelFilter::Info);
    log::set_boxed_logger(Box::new(StderrLogger(level))).unwrap();
    log::set_max_level(level);

    let args = parse_args()?;

    if args.ensure_free.is_some() && args.dry_run {
        bail!("--ensure-free cannot be combined with --dry-run");
    }

    let max_freed = if let Some(target) = args.ensure_free {
        let avail = available_bytes(&args.store_dir)?;
        if avail >= target {
            println!("{} already free, nothing to do", format_size(avail));
            return Ok(());
        }
        let need = target - avail;
        log::info!(
            "freeing at least {} to reach {} free",
            format_size(need),
            format_size(target)
        );
        Some(need)
    } else {
        None
    };

    if args.delete_old {
        let cutoff = args
            .delete_older_than
            .as_deref()
            .map(profiles::parse_older_than)
            .transpose()?;

        profiles::profile_dirs(&args.state_dir)
            .par_iter()
            .try_for_each(|dir| profiles::remove_old_generations(dir, cutoff, args.dry_run))?;
    }

    let keep_recent_after = args
        .keep_recent
        .as_deref()
        .map(profiles::parse_older_than)
        .transpose()?
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });

    let store = db::NixDb::open(&args.store_dir, &args.state_dir)?;
    let opts = gc::GcOptions {
        dry_run: args.dry_run,
        max_freed,
        keep_recent_after,
    };
    let (bytes_freed, paths_deleted) = gc::collect_garbage(&store, &opts)?;

    if args.dry_run {
        println!(
            "{paths_deleted} store paths would be deleted (~{})",
            format_size(bytes_freed)
        );
    } else {
        println!(
            "{paths_deleted} store paths deleted, {} freed",
            format_size(bytes_freed)
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("123").unwrap(), 123);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("2k").unwrap(), 2048);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("3m").unwrap(), 3 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("1T").unwrap(), 1024u64.pow(4));
        assert_eq!(parse_size("1.5K").unwrap(), 1536);
        assert_eq!(parse_size(" 4M ").unwrap(), 4 * 1024 * 1024);
        assert!(parse_size("abc").is_err());
    }
}
