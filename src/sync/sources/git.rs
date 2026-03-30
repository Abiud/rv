use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use fs_err as fs;
use log::warn;

use crate::cache::Cache;
use crate::consts::DESCRIPTION_FILENAME;
use crate::git::url::GitUrl;
use crate::git::{GitReference, GitRemote};
use crate::library::LocalMetadata;
use crate::lockfile::Source;
use crate::sync::LinkMode;
use crate::sync::errors::SyncError;
use crate::{Cancellation, CommandExecutor, RCmd, ResolvedDependency};

#[allow(clippy::too_many_arguments)]
pub(crate) fn install_package(
    pkg: &ResolvedDependency,
    library_dirs: &[&Path],
    cache: &Cache,
    r_cmd: &impl RCmd,
    git_exec: &(impl CommandExecutor + Clone + 'static),
    configure_args: &[String],
    strip: bool,
    cancellation: Arc<Cancellation>,
) -> Result<(), SyncError> {
    let (local_paths, global_paths) = cache.get_package_paths(&pkg.source, None, None);

    // We will have the source version since we needed to clone it to get the DESCRIPTION file
    if !pkg.cache_status.binary_available() {
        let repo_url = pkg.source.git_url().unwrap();
        let sha = pkg.source.sha();
        // TODO: this won't work if multiple projects are trying to checkout different refs
        // on the same user at the same time
        let remote = GitRemote::new(repo_url);
        remote.checkout(
            &local_paths.source,
            &GitReference::Commit(sha),
            git_exec.clone(),
        )?;
        // If we have a directory, don't forget to set it before building it
        let (source_path, sub_dir) = match &pkg.source {
            Source::Git {
                directory: Some(dir),
                ..
            }
            | Source::RUniverse {
                directory: Some(dir),
                ..
            } => (local_paths.source, Some(dir)),
            _ => (local_paths.source, None),
        };

        let output = r_cmd.install(
            &source_path,
            sub_dir,
            library_dirs,
            &local_paths.binary,
            cancellation,
            &pkg.env_vars,
            configure_args,
            strip,
        )?;

        // Write Remote* fields to the installed DESCRIPTION in the binary cache.
        // Must happen after R CMD INSTALL, which regenerates DESCRIPTION (adding Built: etc).
        // Tools like rsconnect, sessioninfo, and renv read these to identify package sources.
        let installed_pkg_dir = local_paths.binary.join(pkg.name.as_ref());
        write_remote_fields(&installed_pkg_dir, &pkg.source);

        let log_path = cache.local().get_build_log_path(&pkg.source, None, None);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
            let mut f = fs::File::create(log_path)?;
            f.write_all(output.as_bytes())?;
        }

        let metadata = LocalMetadata::Sha(sha.to_owned());
        metadata.write(installed_pkg_dir)?;
    }

    // Link from global cache if available there, otherwise from local cache
    let binary_path = if pkg.cache_status.global_binary_available() {
        global_paths.unwrap().binary
    } else {
        local_paths.binary
    };

    // And then we always link the binary folder into the staging library
    LinkMode::link_files(None, &pkg.name, binary_path, library_dirs.first().unwrap())?;
    Ok(())
}

/// Append Remote* fields to the DESCRIPTION file in the source directory before
/// R CMD INSTALL. This ensures the installed binary includes provenance metadata
/// that tools like rsconnect, sessioninfo, and renv use to identify package sources.
fn write_remote_fields(pkg_dir: &Path, source: &Source) {
    let desc_path = pkg_dir.join(DESCRIPTION_FILENAME);
    if !desc_path.exists() {
        warn!("DESCRIPTION not found at {}, skipping Remote* fields", desc_path.display());
        return;
    }

    let fields = match source {
        Source::Git {
            git,
            sha,
            directory,
            tag,
            branch,
        } => build_remote_fields(git, sha, directory.as_deref(), tag.as_deref(), branch.as_deref()),
        Source::RUniverse {
            git,
            sha,
            directory,
            ..
        } => build_remote_fields(git, sha, directory.as_deref(), None, None),
        _ => return,
    };

    match fs::read_to_string(&desc_path) {
        Ok(mut desc) => {
            if desc.contains("\nRemoteType:") {
                return;
            }
            if !desc.ends_with('\n') {
                desc.push('\n');
            }
            desc.push_str(&fields);
            desc.push('\n');
            if let Err(e) = fs::write(&desc_path, desc) {
                warn!("Failed to write Remote* fields to {}: {e}", desc_path.display());
            }
        }
        Err(e) => {
            warn!("Failed to read {}: {e}", desc_path.display());
        }
    }
}

/// Build the Remote* field block for a git-sourced package.
fn build_remote_fields(
    git_url: &GitUrl,
    sha: &str,
    directory: Option<&str>,
    tag: Option<&str>,
    branch: Option<&str>,
) -> String {
    let url_str = git_url.url();

    // Determine RemoteRef: prefer tag, then branch, then fall back to SHA
    let remote_ref = tag
        .or(branch)
        .unwrap_or(sha);

    let mut fields = if let Some((host, username, repo)) = parse_github_url(git_url) {
        format!(
            "RemoteType: github\n\
             RemoteHost: {host}\n\
             RemoteUsername: {username}\n\
             RemoteRepo: {repo}\n\
             RemoteUrl: {url_str}\n\
             RemoteRef: {remote_ref}\n\
             RemoteSha: {sha}"
        )
    } else {
        format!(
            "RemoteType: git\n\
             RemoteUrl: {url_str}\n\
             RemoteRef: {remote_ref}\n\
             RemoteSha: {sha}"
        )
    };

    if let Some(dir) = directory {
        fields.push_str(&format!("\nRemoteSubdir: {dir}"));
    }

    fields
}

/// Extract (api_host, username, repo) from a GitHub HTTPS URL.
/// Handles `https://github.com/owner/repo` and `https://github.com/owner/repo.git`.
fn parse_github_url(git_url: &GitUrl) -> Option<(String, String, String)> {
    if let GitUrl::Http(url) = git_url {
        let host = url.host_str()?;
        if host != "github.com" {
            return None;
        }
        let path = url.path().trim_start_matches('/').trim_end_matches(".git");
        let mut parts = path.splitn(3, '/');
        let username = parts.next()?;
        let repo = parts.next()?;
        // Reject if there are extra path segments beyond owner/repo
        if parts.next().is_some() || username.is_empty() || repo.is_empty() {
            return None;
        }
        return Some((
            "api.github.com".to_string(),
            username.to_string(),
            repo.to_string(),
        ));
    }
    None
}
