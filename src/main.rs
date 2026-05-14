use anyhow::{Result, bail};
use fast_nix_gc::{db, format_size, gc, profiles};
use rayon::prelude::*;
use std::path::PathBuf;

struct Args {
    delete_old: bool,
    delete_older_than: Option<String>,
    dry_run: bool,
    max_freed: Option<u64>,
    store_dir: PathBuf,
    state_dir: PathBuf,
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
        eprintln!("      --max-freed BYTES         Maximum bytes to free");
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
        max_freed: pargs.opt_value_from_str("--max-freed")?,
        store_dir: pargs
            .opt_value_from_str("--store-dir")?
            .unwrap_or_else(|| PathBuf::from("/nix/store")),
        state_dir: pargs
            .opt_value_from_str("--state-dir")?
            .unwrap_or_else(|| PathBuf::from("/nix/var/nix")),
    })
}


fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .format_target(false)
        .init();

    let args = parse_args()?;

    if args.max_freed.is_some() && args.dry_run {
        bail!("options --max-freed and --dry-run cannot be combined");
    }

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

    let store = db::NixDb::open(&args.store_dir, &args.state_dir)?;
    let (bytes_freed, paths_deleted) = gc::collect_garbage(&store, args.dry_run, args.max_freed)?;

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
