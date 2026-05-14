//! Profile generation cleanup (--delete-old / --delete-older-than).

use anyhow::{Context, Result, bail};
use chrono::{Duration, Local};
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Parse Nix-style time specs like "30d", "4h", "2w", "1m".
pub fn parse_older_than(spec: &str) -> Result<SystemTime> {
    if spec.len() < 2 {
        bail!("invalid time spec '{}', expected e.g. '30d'", spec);
    }
    // split_at must land on a char boundary; suffix is always ASCII.
    let (num_str, unit) = spec.split_at(spec.len() - 1);
    let num: i64 = num_str.parse().context("invalid number in time spec")?;
    let dur = match unit {
        "h" => Duration::hours(num),
        "d" => Duration::days(num),
        "w" => Duration::weeks(num),
        "m" => Duration::days(num * 30),
        _ => bail!("unknown time unit '{}', use h/d/w/m", unit),
    };
    Ok(SystemTime::from(Local::now() - dur))
}

fn find_generation_links(profile: &Path) -> Result<Vec<(PathBuf, u64)>> {
    let parent = profile.parent().unwrap_or(Path::new("."));
    let profile_name = profile
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let mut gens = Vec::new();
    let entries = match fs::read_dir(parent) {
        Ok(e) => e,
        Err(_) => return Ok(gens),
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_prefix(&format!("{}-", profile_name))
            && let Some(num_str) = rest.strip_suffix("-link")
            && let Ok(r#gen) = num_str.parse::<u64>()
        {
            gens.push((entry.path(), r#gen));
        }
    }
    gens.sort_by_key(|(_, g)| *g);
    Ok(gens)
}

fn current_generation(profile: &Path) -> Result<Option<u64>> {
    let target = match fs::read_link(profile) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let name = target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let profile_name = profile
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    if let Some(rest) = name.strip_prefix(&format!("{}-", profile_name))
        && let Some(num_str) = rest.strip_suffix("-link")
        && let Ok(r#gen) = num_str.parse::<u64>()
    {
        return Ok(Some(r#gen));
    }
    Ok(None)
}

fn delete_old_generations(profile: &Path, dry_run: bool) -> Result<()> {
    let current = current_generation(profile)?;
    let gens = find_generation_links(profile)?;

    for (path, r#gen) in &gens {
        if Some(*r#gen) == current {
            continue;
        }
        if dry_run {
            log::info!("would remove: {}", path.display());
        } else {
            log::info!("removing: {}", path.display());
            fs::remove_file(path).ok();
        }
    }
    Ok(())
}

fn delete_generations_older_than(profile: &Path, cutoff: SystemTime, dry_run: bool) -> Result<()> {
    let current = current_generation(profile)?;
    let gens = find_generation_links(profile)?;

    for (path, r#gen) in &gens {
        if Some(*r#gen) == current {
            continue;
        }
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if mtime < cutoff {
            if dry_run {
                log::info!("would remove (old): {}", path.display());
            } else {
                log::info!("removing (old): {}", path.display());
                fs::remove_file(path).ok();
            }
        }
    }
    Ok(())
}

pub fn remove_old_generations(
    dir: &Path,
    delete_older_than: Option<SystemTime>,
    dry_run: bool,
) -> Result<()> {
    let entries: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.flatten().collect(),
        Err(_) => return Ok(()),
    };

    let can_write = nix::unistd::access(dir, nix::unistd::AccessFlags::W_OK).is_ok();

    entries.par_iter().try_for_each(|entry| -> Result<()> {
        let path = entry.path();
        let ft = match fs::symlink_metadata(&path) {
            Ok(m) => m.file_type(),
            Err(_) => return Ok(()),
        };

        if ft.is_symlink() && can_write {
            let link_target = match fs::read_link(&path) {
                Ok(t) => t,
                Err(_) => return Ok(()),
            };
            if link_target.to_string_lossy().contains("link") {
                log::info!("removing old generations of profile {}", path.display());
                if let Some(cutoff) = delete_older_than {
                    delete_generations_older_than(&path, cutoff, dry_run)?;
                } else {
                    delete_old_generations(&path, dry_run)?;
                }
            }
        } else if ft.is_dir() {
            remove_old_generations(&path, delete_older_than, dry_run)?;
        }
        Ok(())
    })?;

    Ok(())
}

pub fn profile_dirs(state_dir: &Path) -> BTreeSet<PathBuf> {
    let mut dirs = BTreeSet::new();

    if let Ok(user) = std::env::var("USER") {
        dirs.insert(state_dir.join("profiles/per-user").join(&user));
    }

    dirs.insert(state_dir.join("profiles"));

    if let Ok(home) = std::env::var("HOME") {
        let default_profile = PathBuf::from(&home).join(".nix-profile");
        if let Some(parent) = default_profile.parent() {
            dirs.insert(parent.to_path_buf());
        }
    }

    dirs
}
