//! Profile generation cleanup (--delete-old / --delete-older-than).

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Parse Nix-style time specs like "30d", "4h", "2w", "1m".
pub fn parse_older_than(spec: &str) -> Result<SystemTime> {
    if spec.len() < 2 {
        bail!("invalid time spec '{}', expected e.g. '30d'", spec);
    }
    // split_at must land on a char boundary; suffix is always ASCII.
    let (num_str, unit) = spec.split_at(spec.len() - 1);
    let num: u64 = num_str.parse().context("invalid number in time spec")?;
    let secs = match unit {
        "h" => num * 3600,
        "d" => num * 86400,
        "w" => num * 7 * 86400,
        "m" => num * 30 * 86400,
        _ => bail!("unknown time unit '{}', use h/d/w/m", unit),
    };
    Ok(SystemTime::now() - Duration::from_secs(secs))
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("readlink {}", profile.display())),
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
    // Fail closed: if we cannot tell which generation is active (profile
    // gone or pointing at something that isn't a generation link),
    // deleting "all but current" would delete the active system too.
    let Some(current) = current_generation(profile)? else {
        log::warn!(
            "cannot determine current generation of {}; skipping",
            profile.display()
        );
        return Ok(());
    };
    let gens = find_generation_links(profile)?;

    for (path, r#gen) in &gens {
        if *r#gen == current {
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
    // Same fail-closed rule as delete_old_generations.
    let Some(current) = current_generation(profile)? else {
        log::warn!(
            "cannot determine current generation of {}; skipping",
            profile.display()
        );
        return Ok(());
    };
    let gens = find_generation_links(profile)?;

    let mtime_of = |path: &Path| -> Option<SystemTime> {
        let meta = fs::symlink_metadata(path).ok()?;
        Some(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH))
    };

    // Like Nix (profiles.cc deleteGenerationsOlderThan): keep the newest
    // generation older than the cutoff. It was the active one at the
    // requested point in time, and the user wants to be able to roll back
    // to it.
    let newest_older = gens
        .iter()
        .rev()
        .find(|(path, _)| mtime_of(path).is_some_and(|t| t < cutoff))
        .map(|(_, g)| *g);

    for (path, r#gen) in &gens {
        if *r#gen == current || Some(*r#gen) == newest_older {
            continue;
        }
        let Some(mtime) = mtime_of(path) else {
            continue;
        };
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

/// Directories scanned for profiles, mirroring nix-collect-garbage:
/// the system profiles dir (recursed, so per-user is covered) and the
/// invoking user's XDG state profiles dir. Never the home directory
/// itself — remove_old_generations recurses, and treating arbitrary
/// `*-N-link` symlinks under $HOME as generations would delete user data.
pub fn profile_dirs(state_dir: &Path) -> BTreeSet<PathBuf> {
    let mut dirs = BTreeSet::new();

    dirs.insert(state_dir.join("profiles"));

    let xdg_state = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .ok()
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/state"))
        });
    if let Some(state_home) = xdg_state {
        dirs.insert(state_home.join("nix/profiles"));
    }

    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime};

    fn approx_secs_ago(t: SystemTime, secs: u64) {
        let elapsed = SystemTime::now().duration_since(t).unwrap();
        let want = Duration::from_secs(secs);
        let diff = elapsed.abs_diff(want);
        assert!(
            diff < Duration::from_secs(5),
            "expected ~{secs}s ago, got {elapsed:?}"
        );
    }

    #[test]
    fn parse_older_than_units() {
        approx_secs_ago(parse_older_than("2h").unwrap(), 2 * 3600);
        approx_secs_ago(parse_older_than("3d").unwrap(), 3 * 86400);
        approx_secs_ago(parse_older_than("2w").unwrap(), 2 * 7 * 86400);
        approx_secs_ago(parse_older_than("2m").unwrap(), 2 * 30 * 86400);
    }

    #[test]
    fn parse_older_than_rejects_invalid() {
        assert!(parse_older_than("").is_err());
        assert!(parse_older_than("d").is_err());
        assert!(parse_older_than("5x").is_err());
        assert!(parse_older_than("xd").is_err());
    }

    #[test]
    fn generation_links_and_current() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("system");

        // Generation links: system-1-link, system-3-link, system-2-link, plus noise.
        for n in [3u64, 1, 2] {
            let link = dir.path().join(format!("system-{n}-link"));
            symlink(format!("/nix/store/fake-{n}"), &link).unwrap();
        }
        symlink("/nix/store/x", dir.path().join("other-1-link")).unwrap();
        symlink("/nix/store/x", dir.path().join("system-foo-link")).unwrap();

        // Current generation -> system-2-link
        symlink(dir.path().join("system-2-link"), &profile).unwrap();

        let gens = find_generation_links(&profile).unwrap();
        let nums: Vec<u64> = gens.iter().map(|(_, g)| *g).collect();
        assert_eq!(nums, vec![1, 2, 3]);
        assert_eq!(gens[0].0, dir.path().join("system-1-link"));

        assert_eq!(current_generation(&profile).unwrap(), Some(2));
    }

    /// Build a temp profile dir with generations 1..=n and a `system` symlink
    /// pointing at `system-{current}-link`. Returns the temp dir and profile path.
    fn setup_profile(n: u64, current: u64) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        for i in 1..=n {
            let link = dir.path().join(format!("system-{i}-link"));
            symlink(format!("/nix/store/fake-{i}"), &link).unwrap();
        }
        let profile = dir.path().join("system");
        symlink(dir.path().join(format!("system-{current}-link")), &profile).unwrap();
        (dir, profile)
    }

    fn existing_gens(dir: &Path) -> Vec<u64> {
        find_generation_links(&dir.join("system"))
            .unwrap()
            .into_iter()
            .map(|(_, g)| g)
            .collect()
    }

    fn set_link_mtime(path: &Path, t: SystemTime) {
        use nix::sys::stat::{UtimensatFlags, utimensat};
        use nix::sys::time::TimeSpec;
        let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let ts = TimeSpec::new(d.as_secs() as i64, d.subsec_nanos() as i64);
        utimensat(
            nix::fcntl::AT_FDCWD,
            path,
            &ts,
            &ts,
            UtimensatFlags::NoFollowSymlink,
        )
        .unwrap();
    }

    #[test]
    fn delete_old_generations_keeps_only_current() {
        let (dir, profile) = setup_profile(3, 2);

        // Dry run leaves everything in place.
        delete_old_generations(&profile, true).unwrap();
        assert_eq!(existing_gens(dir.path()), vec![1, 2, 3]);

        delete_old_generations(&profile, false).unwrap();
        assert_eq!(existing_gens(dir.path()), vec![2]);
    }

    #[test]
    fn delete_generations_older_than_cutoff() {
        let (dir, profile) = setup_profile(4, 4);
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // gen i has mtime base + i*100s
        for i in 1u64..=4 {
            set_link_mtime(
                &dir.path().join(format!("system-{i}-link")),
                base + Duration::from_secs(i * 100),
            );
        }

        // Cutoff exactly at gen 2's mtime: gen 1 is the only strictly
        // older generation, and as the one active at the cutoff it is
        // kept for rollback (Nix semantics). Nothing goes.
        let cutoff = base + Duration::from_secs(200);
        delete_generations_older_than(&profile, cutoff, true).unwrap();
        assert_eq!(existing_gens(dir.path()), vec![1, 2, 3, 4]);
        delete_generations_older_than(&profile, cutoff, false).unwrap();
        assert_eq!(existing_gens(dir.path()), vec![1, 2, 3, 4]);

        // Cutoff between gen 3 and 4: gens 1-3 are older, gen 3 was
        // active at the cutoff and is kept; 1 and 2 go.
        let cutoff = base + Duration::from_secs(350);
        delete_generations_older_than(&profile, cutoff, false).unwrap();
        assert_eq!(existing_gens(dir.path()), vec![3, 4]);

        // Cutoff after all gens: the newest older one is the current
        // generation itself, so everything else goes.
        delete_generations_older_than(&profile, base + Duration::from_secs(10_000), false).unwrap();
        assert_eq!(existing_gens(dir.path()), vec![4]);
    }

    #[test]
    fn remove_old_generations_recurses_and_skips_non_link_targets() {
        let outer = tempfile::tempdir().unwrap();
        let nested = outer.path().join("per-user").join("alice");
        fs::create_dir_all(&nested).unwrap();
        for i in 1u64..=3 {
            symlink(
                format!("/nix/store/fake-{i}"),
                nested.join(format!("prof-{i}-link")),
            )
            .unwrap();
        }
        symlink(nested.join("prof-3-link"), nested.join("prof")).unwrap();
        // Symlink whose target does not contain "link": must be left alone.
        symlink("/nix/store/zzz", outer.path().join("plain")).unwrap();

        remove_old_generations(outer.path(), None, false).unwrap();

        let gens: Vec<u64> = find_generation_links(&nested.join("prof"))
            .unwrap()
            .into_iter()
            .map(|(_, g)| g)
            .collect();
        assert_eq!(gens, vec![3]);
        assert!(outer.path().join("plain").symlink_metadata().is_ok());
    }

    #[test]
    fn profile_dirs_includes_state_and_xdg_paths_but_not_home() {
        let state = Path::new("/var/state");
        let dirs = profile_dirs(state);
        assert!(dirs.contains(&state.join("profiles")));
        if let Ok(home) = std::env::var("HOME") {
            // $HOME itself must never be scanned recursively.
            assert!(!dirs.contains(&PathBuf::from(&home)));
            if std::env::var("XDG_STATE_HOME").is_err() {
                assert!(dirs.contains(&PathBuf::from(home).join(".local/state/nix/profiles")));
            }
        }
    }

    #[test]
    fn unparseable_current_generation_deletes_nothing() {
        // Profile pointing at a non-generation target: refusing to guess
        // protects the active generation from "delete all but current".
        let (dir, profile) = setup_profile(3, 2);
        fs::remove_file(&profile).unwrap();
        symlink("/nix/store/custom-env", &profile).unwrap();

        delete_old_generations(&profile, false).unwrap();
        assert_eq!(existing_gens(dir.path()), vec![1, 2, 3]);

        delete_generations_older_than(&profile, SystemTime::now(), false).unwrap();
        assert_eq!(existing_gens(dir.path()), vec![1, 2, 3]);
    }

    #[test]
    fn current_generation_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(current_generation(&dir.path().join("nope")).unwrap(), None);

        // Profile exists but doesn't point at a -link target.
        let profile = dir.path().join("system");
        symlink("/nix/store/whatever", &profile).unwrap();
        assert_eq!(current_generation(&profile).unwrap(), None);

        // No matching generation links anywhere.
        assert_eq!(find_generation_links(&profile).unwrap(), vec![]);
    }
}
