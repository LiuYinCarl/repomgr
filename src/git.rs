//! Thin wrappers around the `git` command-line tool.
//!
//! We deliberately shell out to the user's `git` instead of linking a git
//! library: it keeps the dependency tree tiny (only ratatui) and
//! automatically respects the user's global git configuration.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// Timeout for local info queries (`status`, `log`, `branch`, …). These
/// must be fast; a hung filesystem (e.g. a dead NFS mount) should freeze
/// the UI for at most this long. Network operations (`pull`, `clone`) have
/// no timeout — they may legitimately take minutes.
const INFO_TIMEOUT: Duration = Duration::from_secs(20);

struct GitOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_git(dir: &Path, args: &[&str], timeout: Option<Duration>) -> Result<GitOutput, String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;

    // Drain stdout/stderr on reader threads so the child can never block on
    // a full pipe while we are polling for its exit status.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });

    let status = wait_with_timeout(&mut child, timeout)?;
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

fn wait_with_timeout(
    child: &mut Child,
    timeout: Option<Duration>,
) -> Result<std::process::ExitStatus, String> {
    let deadline = timeout.map(|t| Instant::now() + t);
    let timeout_secs = timeout.map(|t| t.as_secs()).unwrap_or_default();
    loop {
        match child
            .try_wait()
            .map_err(|e| format!("cannot wait on git: {e}"))?
        {
            Some(status) => return Ok(status),
            None => {
                let Some(deadline) = deadline else {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    // Reap promptly, but never block forever: a process stuck
                    // in uninterruptible I/O (e.g. hung NFS) ignores SIGKILL
                    // until the I/O returns, and wait() would hang with it.
                    let reap_deadline = Instant::now() + Duration::from_secs(1);
                    while Instant::now() < reap_deadline {
                        if let Ok(Some(_)) = child.try_wait() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    return Err(format!(
                        "git did not finish within {timeout_secs}s (killed)"
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn join_output(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (true, false) => stderr.to_string(),
        (false, true) => stdout.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

/// Strip terminal control characters (C0/C1 control codes) from text that
/// came from the outside world (git output, directory names, URLs).
///
/// Without this, a crafted commit subject, file name or repository
/// directory could inject escape sequences into the terminal. Applied per
/// line so line structure survives (`\n` is never passed in).
pub fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Run `git <args>` inside `dir`, returning trimmed stdout on success and a
/// combined stdout+stderr message on failure.
pub fn git_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = run_git(dir, args, Some(INFO_TIMEOUT))?;
    if out.status.success() {
        Ok(out.stdout.trim().to_string())
    } else {
        Err(join_output(&out.stdout, &out.stderr))
    }
}

/// Like [`git_in`], but keeps stderr on success too. Used for user-visible
/// operations such as `pull` and `clone`, whose progress output lives on
/// stderr.
pub fn git_verbose(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = run_git(dir, args, None)?;
    let text = join_output(&out.stdout, &out.stderr);
    if out.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

/// List immediate subdirectories of `root` that contain a `.git` entry
/// (a directory for regular clones, a file for worktrees/submodules).
pub fn discover_repos(root: &Path) -> Vec<PathBuf> {
    let mut repos = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return repos;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.join(".git").exists() {
            repos.push(path);
        }
    }
    repos.sort();
    repos
}

/// Cheap change signal for a repository: a sorted multiset of mtimes from
/// the repo directory, `.git/HEAD`, `.git/config`, `packed-refs` and the
/// whole `refs` subtree (files *and* directories, so creating, deleting or
/// rewriting any ref is detected).
///
/// Real git operations (commit, pull, fetch, checkout, branch, stash, …)
/// touch at least one of these, while our own read-only info queries never
/// do — they only briefly touch the `.git` directory itself (e.g. an index
/// lock), which is why that directory's own mtime is deliberately not part
/// of the signal. A handful of stats, no git processes.
pub fn repo_mtime(path: &Path) -> Option<Vec<SystemTime>> {
    let dir = std::fs::metadata(path).ok()?.modified().ok()?;
    let mut signal = vec![dir];
    let git_path = path.join(".git");
    let meta = std::fs::metadata(&git_path).ok()?;
    if meta.is_dir() {
        let head = std::fs::metadata(git_path.join("HEAD"))
            .ok()?
            .modified()
            .ok()?;
        let config = std::fs::metadata(git_path.join("config"))
            .ok()?
            .modified()
            .ok()?;
        signal.push(head);
        signal.push(config);
        if let Ok(packed) = std::fs::metadata(git_path.join("packed-refs")) {
            if let Ok(mtime) = packed.modified() {
                signal.push(mtime);
            }
        }
        collect_mtimes(&git_path.join("refs"), &mut signal);
    } else {
        // Worktree/submodule: `.git` is a pointer file, not a directory.
        let git_file = meta.modified().ok()?;
        signal.push(git_file);
        signal.push(git_file);
        signal.push(git_file);
    }
    signal.sort();
    signal.dedup();
    Some(signal)
}

fn collect_mtimes(dir: &Path, out: &mut Vec<SystemTime>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if let Ok(mtime) = meta.modified() {
            out.push(mtime);
        }
        if meta.is_dir() {
            collect_mtimes(&entry.path(), out);
        }
    }
}

/// Snapshot of the interesting facts about one repository.
#[derive(Debug, Clone, Default)]
pub struct RepoInfo {
    /// Current branch name, or a detached-HEAD description.
    pub branch: String,
    /// (name, URL) of the first configured remote, if any.
    pub remote: Option<(String, String)>,
    /// Commits ahead of the upstream tracking branch.
    pub ahead: Option<usize>,
    /// Commits behind the upstream tracking branch.
    pub behind: Option<usize>,
    /// Number of changed/untracked files in the working tree.
    pub dirty: usize,
    /// Number of untracked files.
    pub untracked: usize,
    /// Number of stashes.
    pub stashes: usize,
    /// Number of local branches.
    pub local_branches: usize,
    /// One-line description of the most recent commit.
    pub last_commit: String,
}

/// Gather the info shown in the right-hand panel. Every query degrades
/// gracefully so a fresh or detached repository never crashes the UI.
pub fn load_info(path: &Path) -> RepoInfo {
    let remote = git_in(path, &["remote", "-v"])
        .ok()
        .and_then(|out| {
            out.lines().next().map(|line| {
                let mut parts = line.split_whitespace();
                let name = sanitize(parts.next().unwrap_or(""));
                let url = sanitize(parts.next().unwrap_or(""));
                (name, url)
            })
        })
        .filter(|(_, url)| !url.is_empty());

    let (ahead, behind) = match git_in(
        path,
        &["rev-list", "--left-right", "--count", "@{u}...HEAD"],
    ) {
        Ok(counts) => {
            let mut parts = counts.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some(behind), Some(ahead)) => (ahead.parse().ok(), behind.parse().ok()),
                _ => (None, None),
            }
        }
        Err(_) => (None, None),
    };

    let status = git_in(path, &["status", "--porcelain"]).unwrap_or_default();
    let dirty = status.lines().count();
    let untracked = status.lines().filter(|line| line.starts_with("??")).count();

    let stashes = git_in(path, &["stash", "list"])
        .map(|out| out.lines().count())
        .unwrap_or(0);

    let heads = git_in(
        path,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
    .unwrap_or_default();
    let local_branches = heads.lines().filter(|line| !line.is_empty()).count();

    let last_commit = git_in(path, &["log", "-1", "--pretty=format:%h %s (%ar)"])
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| sanitize(&s))
        .unwrap_or_else(|| "(no commits)".to_string());

    RepoInfo {
        branch: sanitize(&current_branch(path)),
        remote,
        ahead,
        behind,
        dirty,
        untracked,
        stashes,
        local_branches,
        last_commit,
    }
}

fn current_branch(path: &Path) -> String {
    if let Ok(branch) = git_in(path, &["branch", "--show-current"]) {
        if !branch.is_empty() {
            return branch;
        }
    }
    match git_in(path, &["rev-parse", "--short", "HEAD"]) {
        Ok(hash) => format!("(detached @ {hash})"),
        Err(_) => "(no commits)".to_string(),
    }
}

/// Fetch and fast-forward the current branch to its upstream.
pub fn update_repo(path: &Path) -> Result<String, String> {
    git_verbose(path, &["pull", "--ff-only"])
}

/// Clone `url` into `root`.
pub fn clone_repo(root: &Path, url: &str) -> Result<String, String> {
    // `--` keeps URLs that start with `-` from being parsed as options.
    git_verbose(root, &["clone", "--", url])
}

pub fn status_short(path: &Path) -> String {
    git_in(path, &["status", "--short", "--branch"])
        .map(|out| out.lines().map(sanitize).collect::<Vec<_>>().join("\n"))
        .unwrap_or_else(|e| format!("error: {e}"))
}

pub fn recent_log(path: &Path) -> String {
    git_in(path, &["log", "--oneline", "-15", "--decorate"])
        .map(|out| out.lines().map(sanitize).collect::<Vec<_>>().join("\n"))
        .unwrap_or_else(|e| format!("error: {e}"))
}

pub fn branch_list(path: &Path) -> String {
    git_in(path, &["branch", "-vv"])
        .map(|out| out.lines().map(sanitize).collect::<Vec<_>>().join("\n"))
        .unwrap_or_else(|e| format!("error: {e}"))
}

/// Open a folder in the system file manager (`open` on macOS, `xdg-open`
/// on Linux).
pub fn open_in_file_manager(path: &Path) -> Result<(), String> {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    run_external(cmd, &[path.to_string_lossy().as_ref()])
}

/// Open a URL in the default browser.
pub fn open_url(url: &str) -> Result<(), String> {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    run_external(cmd, &[url])
}

fn run_external(cmd: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| format!("cannot run {cmd}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} exited with {status}"))
    }
}

/// Turn an scp-style remote URL (`git@host:path`) into an https URL that a
/// browser can open. Plain https URLs pass through unchanged.
pub fn browser_url(remote: &str) -> String {
    let remote = remote.trim();
    if let Some(rest) = remote.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            return format!("https://{host}/{path}");
        }
    }
    if let Some(rest) = remote.strip_prefix("ssh://") {
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        return format!("https://{rest}");
    }
    if let Some(rest) = remote.strip_prefix("git://") {
        return format!("https://{rest}");
    }
    remote.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("repomgr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main", "."]);
        git(dir, &["config", "user.email", "t@t"]);
        git(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "hi").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-qm", "init"]);
    }

    #[test]
    fn discovers_only_git_dirs() {
        let dir = temp_dir("discover");
        std::fs::create_dir_all(dir.join("repo_a/.git")).unwrap();
        std::fs::create_dir_all(dir.join("plain")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden/.git")).unwrap();

        let repos = discover_repos(&dir);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].file_name().unwrap(), "repo_a");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_url_converts_scp_style() {
        assert_eq!(
            browser_url("git@github.com:user/repo.git"),
            "https://github.com/user/repo.git"
        );
        assert_eq!(
            browser_url("https://github.com/user/repo.git"),
            "https://github.com/user/repo.git"
        );
        assert_eq!(
            browser_url("ssh://git@github.com/user/repo.git"),
            "https://github.com/user/repo.git"
        );
        assert_eq!(
            browser_url("ssh://github.com/user/repo.git"),
            "https://github.com/user/repo.git"
        );
        assert_eq!(
            browser_url("git://git.sv.gnu.org/emacs.git"),
            "https://git.sv.gnu.org/emacs.git"
        );
    }

    #[test]
    fn sanitize_strips_control_chars() {
        assert_eq!(sanitize("a\u{1b}[31mb\u{1b}[0mc"), "a[31mb[0mc");
        assert_eq!(sanitize("plain text"), "plain text");
        assert_eq!(sanitize("line\nfeed"), "linefeed");
    }

    #[test]
    fn load_info_reads_branch_dirty_and_sanitizes_subject() {
        let dir = temp_dir("loadinfo");
        init_repo(&dir);

        std::fs::write(dir.join("f.txt"), "dirty").unwrap();
        let info = load_info(&dir);
        assert_eq!(info.branch, "main");
        assert_eq!(info.dirty, 1);
        assert_eq!(info.local_branches, 1);

        // A crafted commit subject must not carry control bytes through.
        let _ = Command::new("git")
            .args(["commit", "-qam", "\u{1b}[31mred\u{1b}[0m subject"])
            .current_dir(&dir)
            .status();
        let info = load_info(&dir);
        assert!(
            !info.last_commit.contains('\u{1b}'),
            "control chars must be stripped, got: {info:?}"
        );
        assert!(info.last_commit.contains("red"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_repo_fails_without_upstream() {
        let dir = temp_dir("noupstream");
        init_repo(&dir);
        let result = update_repo(&dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("upstream"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clone_repo_creates_discoverable_repo() {
        let root = temp_dir("clone");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        init_repo(&src);

        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        clone_repo(&target, src.to_str().unwrap()).unwrap();

        let repos = discover_repos(&target);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].file_name().unwrap(), "src");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_info_reports_ahead_and_behind() {
        let root = temp_dir("aheadbehind");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        init_repo(&src);

        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        clone_repo(&target, src.to_str().unwrap()).unwrap();
        let clone = target.join("src");
        git(&clone, &["config", "user.email", "t@t"]);
        git(&clone, &["config", "user.name", "t"]);

        // A local-only commit is ahead of the (stale) tracking ref.
        std::fs::write(clone.join("local.txt"), "x").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-qm", "local"]);
        let info = load_info(&clone);
        assert_eq!(info.ahead, Some(1));
        assert_eq!(info.behind, Some(0));

        // A remote commit + fetch makes both directions non-zero.
        std::fs::write(src.join("remote.txt"), "x").unwrap();
        git(&src, &["add", "."]);
        git(&src, &["commit", "-qm", "remote"]);
        git(&clone, &["fetch", "-q", "origin"]);
        let info = load_info(&clone);
        assert_eq!(info.ahead, Some(1));
        assert_eq!(info.behind, Some(1));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn last_commit_survives_non_utf8_subject() {
        use std::os::unix::ffi::OsStrExt;

        let dir = temp_dir("nonutf8subject");
        init_repo(&dir);
        std::fs::write(dir.join("f.txt"), "changed").unwrap();
        let msg = std::ffi::OsStr::from_bytes(b"bad \xff subject");
        let status = Command::new("git")
            .arg("commit")
            .arg("-qam")
            .arg(msg)
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success());

        let info = load_info(&dir);
        assert!(
            info.last_commit.contains("subject"),
            "non-UTF-8 subject must survive lossily, got: {:?}",
            info.last_commit
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_with_timeout_kills_slow_child() {
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep should exist on macOS/Linux");
        let started = Instant::now();
        let result = wait_with_timeout(&mut child, Some(Duration::from_millis(300)));
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must not block waiting for the child to die"
        );
        assert!(result.unwrap_err().contains("killed"));
    }

    #[test]
    fn repo_mtime_changes_on_commit() {
        let dir = temp_dir("mtime");
        init_repo(&dir);
        let before = repo_mtime(&dir).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(dir.join("f.txt"), "changed").unwrap();
        git(&dir, &["commit", "-qam", "second"]);
        let after = repo_mtime(&dir).unwrap();
        assert_ne!(before, after, "a commit must change the repo mtime");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_only_queries_do_not_change_repo_mtime() {
        let dir = temp_dir("stable-mtime");
        init_repo(&dir);
        let before = repo_mtime(&dir).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let _ = load_info(&dir);
        let after = repo_mtime(&dir).unwrap();
        assert_eq!(
            before, after,
            "read-only info queries must not disturb the change signal"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
