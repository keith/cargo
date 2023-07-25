//! ```text
//! NAME
//!         xtask-bump-check
//!
//! SYNOPSIS
//!         xtask-bump-check --baseline-rev <REV> --head-rev <REV>
//!
//! DESCRIPTION
//!         Checks if there is any member got changed since a base commit
//!         but forgot to bump its version.
//! ```

use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::path::Path;
use std::task;

use cargo::core::dependency::Dependency;
use cargo::core::registry::PackageRegistry;
use cargo::core::Package;
use cargo::core::QueryKind;
use cargo::core::Registry;
use cargo::core::SourceId;
use cargo::core::Workspace;
use cargo::util::command_prelude::*;
use cargo::util::ToSemver;
use cargo::CargoResult;

pub fn cli() -> clap::Command {
    clap::Command::new("xtask-bump-check")
        .arg(
            opt(
                "verbose",
                "Use verbose output (-vv very verbose/build.rs output)",
            )
            .short('v')
            .action(ArgAction::Count)
            .global(true),
        )
        .arg_quiet()
        .arg(
            opt("color", "Coloring: auto, always, never")
                .value_name("WHEN")
                .global(true),
        )
        .arg(
            opt("baseline-rev", "Git revision to lookup for a baseline")
                .action(ArgAction::Set)
                .required(true),
        )
        .arg(
            opt("head-rev", "The HEAD Git revision")
                .action(ArgAction::Set)
                .required(true),
        )
        .arg(flag("frozen", "Require Cargo.lock and cache are up to date").global(true))
        .arg(flag("locked", "Require Cargo.lock is up to date").global(true))
        .arg(flag("offline", "Run without accessing the network").global(true))
        .arg(multi_opt("config", "KEY=VALUE", "Override a configuration value").global(true))
        .arg(
            Arg::new("unstable-features")
                .help("Unstable (nightly-only) flags to Cargo, see 'cargo -Z help' for details")
                .short('Z')
                .value_name("FLAG")
                .action(ArgAction::Append)
                .global(true),
        )
}

pub fn exec(args: &clap::ArgMatches, config: &mut cargo::util::Config) -> cargo::CliResult {
    config_configure(config, args)?;

    bump_check(args, config)?;

    Ok(())
}

fn config_configure(config: &mut Config, args: &ArgMatches) -> CliResult {
    let verbose = args.verbose();
    // quiet is unusual because it is redefined in some subcommands in order
    // to provide custom help text.
    let quiet = args.flag("quiet");
    let color = args.get_one::<String>("color").map(String::as_str);
    let frozen = args.flag("frozen");
    let locked = args.flag("locked");
    let offline = args.flag("offline");
    let mut unstable_flags = vec![];
    if let Some(values) = args.get_many::<String>("unstable-features") {
        unstable_flags.extend(values.cloned());
    }
    let mut config_args = vec![];
    if let Some(values) = args.get_many::<String>("config") {
        config_args.extend(values.cloned());
    }
    config.configure(
        verbose,
        quiet,
        color,
        frozen,
        locked,
        offline,
        &None,
        &unstable_flags,
        &config_args,
    )?;
    Ok(())
}

/// Turns an arg into a commit object.
fn arg_to_commit<'a>(
    args: &clap::ArgMatches,
    repo: &'a git2::Repository,
    name: &str,
) -> CargoResult<git2::Commit<'a>> {
    let arg = args.get_one::<String>(name).map(String::as_str).unwrap();
    Ok(repo.revparse_single(arg)?.peel_to_commit()?)
}

/// Checkouts a temporary workspace to do further version comparsions.
fn checkout_ws<'cfg, 'a>(
    ws: &Workspace<'cfg>,
    repo: &'a git2::Repository,
    referenced_commit: &git2::Commit<'a>,
) -> CargoResult<Workspace<'cfg>> {
    let repo_path = repo.path().as_os_str().to_str().unwrap();
    // Put it under `target/cargo-<short-id>`
    let short_id = &referenced_commit.id().to_string()[..7];
    let checkout_path = ws.target_dir().join(format!("cargo-{short_id}"));
    let checkout_path = checkout_path.as_path_unlocked();
    let _ = fs::remove_dir_all(checkout_path);
    let new_repo = git2::build::RepoBuilder::new()
        .clone_local(git2::build::CloneLocal::Local)
        .clone(repo_path, checkout_path)
        .unwrap();
    let obj = new_repo.find_object(referenced_commit.id(), None)?;
    new_repo.reset(&obj, git2::ResetType::Hard, None)?;
    Workspace::new(&checkout_path.join("Cargo.toml"), ws.config())
}

/// Get the current beta and stable branch in cargo repository.
///
/// Assumptions:
///
/// * The repository contains the full history of `origin/rust-1.*.0` branches.
/// * The version part of `origin/rust-1.*.0` always ends with a zero.
/// * The maximum version is for beta channel, and the second one is for stable.
fn beta_and_stable_branch(repo: &git2::Repository) -> CargoResult<[git2::Branch<'_>; 2]> {
    let mut release_branches = Vec::new();
    for branch in repo.branches(Some(git2::BranchType::Remote))? {
        let (branch, _) = branch?;
        let Some(version) = branch.name()?.and_then(|n| n.strip_prefix("origin/rust-")) else {
            continue;
        };
        release_branches.push((version.to_semver()?, branch));
    }
    release_branches.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let beta = release_branches.pop().unwrap();
    let stable = release_branches.pop().unwrap();

    assert_eq!(beta.0.major, 1);
    assert_eq!(beta.0.patch, 0);
    assert_eq!(stable.0.major, 1);
    assert_eq!(stable.0.patch, 0);
    assert_ne!(beta.0.minor, stable.0.minor);

    Ok([beta.1, stable.1])
}

/// Gets the referenced commit to compare if version bump needed.
///
/// * When merging into nightly, check the version with beta branch
/// * When merging into beta, check the version with stable branch
/// * When merging into stable, check against crates.io registry directly
fn get_referenced_commit<'a>(
    repo: &'a git2::Repository,
    base: &git2::Commit<'a>,
) -> CargoResult<Option<git2::Commit<'a>>> {
    let [beta, stable] = beta_and_stable_branch(&repo)?;
    let rev_id = base.id();
    let stable_commit = stable.get().peel_to_commit()?;
    let beta_commit = beta.get().peel_to_commit()?;

    let commit = if rev_id == stable_commit.id() {
        None
    } else if rev_id == beta_commit.id() {
        Some(stable_commit)
    } else {
        Some(beta_commit)
    };

    Ok(commit)
}

/// Lists all changed workspace members between two commits.
///
/// Assumption: Paths of workspace members. See `member_dirs`.
fn changed<'r, 'ws>(
    ws: &'ws Workspace<'_>,
    repo: &'r git2::Repository,
    base_commit: &git2::Commit<'r>,
    head: &git2::Commit<'r>,
) -> CargoResult<HashMap<&'ws str, &'ws Package>> {
    let member_dirs = ["crates", "credential", "benches"];
    let ws_members: HashMap<&str, &Package> =
        HashMap::from_iter(ws.members().map(|m| (m.name().as_str(), m)));
    let base_tree = base_commit.as_object().peel_to_tree()?;
    let head_tree = head.as_object().peel_to_tree()?;
    let diff = repo
        .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Default::default())
        .unwrap();

    let mut changed_members = HashMap::new();

    for delta in diff.deltas() {
        let mut insert_changed = |path: &Path| {
            if !member_dirs.iter().any(|dir| path.starts_with(dir)) {
                return;
            }
            let Some(member) = path.components().nth(1) else {
                log::trace!("skipping {path:?}, not a valid member path");
                return;
            };
            let name = member.as_os_str().to_str().unwrap();
            let Some((name, pkg)) = ws_members.get_key_value(name) else {
                log::trace!("skipping {name:?}, not found in workspace");
                return;
            };
            if pkg.publish() == &Some(vec![]) {
                log::trace!("skipping {name}, `publish = false`");
                return;
            }
            changed_members.insert(*name, *pkg);
        };

        insert_changed(delta.old_file().path().unwrap());
        insert_changed(delta.new_file().path().unwrap());
    }

    Ok(changed_members)
}

/// Compares version against published crates on crates.io.
///
/// Assumption: We always release a version larger than all existing versions.
fn check_crates_io<'a>(
    config: &Config,
    changed_members: &HashMap<&str, &'a Package>,
    needs_bump: &mut Vec<&'a Package>,
) -> CargoResult<()> {
    let source_id = SourceId::crates_io(config)?;
    let mut registry = PackageRegistry::new(config)?;
    let _lock = config.acquire_package_cache_lock()?;
    registry.lock_patches();
    config.shell().status(
        "BumpCheck",
        format_args!("compare against `{}`", source_id.display_registry_name()),
    )?;
    for member in changed_members.values() {
        let name = member.name();
        let current = member.version();
        let version_req = format!("<={current}");
        let query = Dependency::parse(name, Some(&version_req), source_id)?;
        let possibilities = loop {
            // Exact to avoid returning all for path/git
            match registry.query_vec(&query, QueryKind::Exact) {
                task::Poll::Ready(res) => {
                    break res?;
                }
                task::Poll::Pending => registry.block_until_ready()?,
            }
        };
        let max_version = possibilities.iter().map(|s| s.version()).max();
        if max_version >= Some(current) {
            needs_bump.push(member);
        }
    }

    Ok(())
}

/// Main entry of `xtask-bump-check`.
///
/// Assumption: version number are incremental. We never have point release for old versions.
fn bump_check(args: &clap::ArgMatches, config: &mut cargo::util::Config) -> CargoResult<()> {
    let ws = args.workspace(config)?;
    let repo = git2::Repository::open(ws.root()).unwrap();
    let base_commit = arg_to_commit(args, &repo, "baseline-rev")?;
    let head_commit = arg_to_commit(args, &repo, "head-rev")?;
    let referenced_commit = get_referenced_commit(&repo, &base_commit)?;
    let changed_members = changed(&ws, &repo, &base_commit, &head_commit)?;

    let mut needs_bump = Vec::new();

    check_crates_io(config, &changed_members, &mut needs_bump)?;

    if let Some(commit) = referenced_commit.as_ref() {
        config.shell().status(
            "BumpCheck",
            format_args!("compare against `{}`", commit.id()),
        )?;
        for member in checkout_ws(&ws, &repo, commit)?.members() {
            let name = member.name().as_str();
            let Some(changed) = changed_members.get(name) else {
                log::trace!("skpping {name}, may be removed or not published");
                continue;
            };

            if changed.version() <= member.version() {
                needs_bump.push(*changed);
            }
        }
    }

    if needs_bump.is_empty() {
        config
            .shell()
            .status("BumpCheck", "no version bump needed for member crates.")?;
        return Ok(());
    }

    needs_bump.sort();
    needs_bump.dedup();

    let mut msg = String::new();
    msg.push_str("Detected changes in these crates but no version bump found:\n");
    for pkg in needs_bump {
        writeln!(&mut msg, "  {}@{}", pkg.name(), pkg.version())?;
    }
    msg.push_str("\nPlease bump at least one patch version in each corresponding Cargo.toml.");
    anyhow::bail!(msg)
}

#[test]
fn verify_cli() {
    cli().debug_assert();
}
