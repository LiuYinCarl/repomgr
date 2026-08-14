//! repomgr — a tiny TUI for managing the git repositories in the current
//! directory.
//!
//! Architecture:
//!   main.rs  app state + event loop + key handling
//!   view.rs  all drawing (list/info panes, modals, help)
//!   git.rs   shelling out to the `git` CLI

mod git;
mod view;

use std::collections::{HashMap, HashSet};
use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime};

use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
        ExecutableCommand,
    },
    prelude::*,
};

use view::View;

/// How often the background watcher stats repositories for external changes.
/// Pure filesystem metadata checks, no git processes involved.
const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browse,
    Help,
    InputClone,
    Working,
    Status,
    Log,
    Branches,
    UpdateResult,
    Message,
}

/// A git operation that the event loop runs synchronously (drawn first as a
/// "working…" modal so the user sees that something is happening).
enum PendingOp {
    /// Single repository to update synchronously, captured when the
    /// operation was started. Batch updates go through [`BatchUpdate`].
    Update(PathBuf),
    Clone,
}

/// An in-flight concurrent batch update. A bounded pool of worker threads
/// (sized like the prefetch workers) runs `git pull` for the targets and
/// sends each result back with its original list index, so the final
/// report keeps the list order regardless of completion order.
struct BatchUpdate {
    targets: Vec<PathBuf>,
    rx: mpsc::Receiver<(usize, Result<String, String>)>,
    /// Per-target result slot, filled as workers report back.
    results: Vec<Option<Result<String, String>>>,
    completed: usize,
}

/// What the event loop should do after a key was handled.
enum KeyAction {
    None,
    Quit,
}

/// One entry in the info cache. `manual` marks entries refreshed by the user
/// (update / `R` / post-clone): the background prefetch must never overwrite
/// those with older data.
struct CacheEntry {
    info: git::RepoInfo,
    manual: bool,
    /// Change signal (repo dir / HEAD / config / packed-refs / refs subtree
    /// mtimes) captured when this entry was last loaded; the watcher reloads
    /// the entry when it changes.
    mtime: Option<Vec<SystemTime>>,
}

/// Display name for a repository path: its directory name, sanitized.
fn repo_name(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    git::sanitize(&name)
}

fn cache_entry(info: git::RepoInfo, manual: bool, path: &Path) -> CacheEntry {
    CacheEntry {
        info,
        manual,
        mtime: git::repo_mtime(path),
    }
}

struct App {
    root: PathBuf,
    repos: Vec<PathBuf>,
    selected: usize,
    /// Indices marked with Space for a batch update. Cleared on rescan
    /// (indices would no longer match) and after an update.
    marked: HashSet<usize>,
    /// Whether the initial directory scan has finished. The first frame is
    /// drawn before the scan so the screen never stays blank while git
    /// queries run at startup.
    scanned: bool,
    mode: Mode,
    /// Mode to return to when closing the help modal.
    prev_mode: Mode,
    /// Operation waiting to run in the event loop, if any.
    pending: Option<PendingOp>,
    /// Concurrent batch update in flight, if any.
    batch: Option<BatchUpdate>,
    /// Background-prefetched info, keyed by repository path.
    cache: Arc<Mutex<HashMap<PathBuf, CacheEntry>>>,
    /// Clone URL input buffer.
    input: String,
    /// Scroll offset for the info panel and modals.
    scroll: u16,
    modal_title: String,
    modal_text: Vec<String>,
    /// One-shot message shown in the status bar (e.g. "opened in Finder").
    status_msg: Option<String>,
}

impl App {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            repos: Vec::new(),
            selected: 0,
            marked: HashSet::new(),
            scanned: false,
            mode: Mode::Browse,
            prev_mode: Mode::Browse,
            pending: None,
            batch: None,
            cache: Arc::new(Mutex::new(HashMap::new())),
            input: String::new(),
            scroll: 0,
            modal_title: String::new(),
            modal_text: Vec::new(),
            status_msg: None,
        }
    }

    fn current_path(&self) -> Option<&PathBuf> {
        self.repos.get(self.selected)
    }

    fn current_name(&self) -> Option<String> {
        self.current_path().map(|p| repo_name(p))
    }

    fn rescan(&mut self) {
        self.repos = git::discover_repos(&self.root);
        self.marked.clear();
        if self.repos.is_empty() {
            self.selected = 0;
            if let Ok(mut cache) = self.cache.lock() {
                cache.clear();
            }
            return;
        }
        self.selected = self.selected.min(self.repos.len() - 1);
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
        self.start_prefetch();
    }

    fn current_info(&self) -> Option<git::RepoInfo> {
        let path = self.current_path()?;
        let cache = self.cache.lock().ok()?;
        cache.get(path).map(|entry| entry.info.clone())
    }

    /// Synchronously reload one repository and mark its cache entry as
    /// manually refreshed so the background worker leaves it alone.
    fn manual_refresh(&mut self, path: &Path) {
        let info = git::load_info(path);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(path.to_path_buf(), cache_entry(info, true, path));
        }
    }

    /// Manually refresh the selected repository.
    fn manual_refresh_current(&mut self) {
        let Some(path) = self.current_path().cloned() else {
            return;
        };
        self.manual_refresh(&path);
    }

    /// Watch repositories for external changes (a `git pull`/`commit`/… in
    /// another terminal, for example). Cheap metadata stats every
    /// [`REFRESH_INTERVAL`]; only repos whose mtime moved are reloaded.
    fn start_watcher(&self) {
        let cache = Arc::clone(&self.cache);
        std::thread::spawn(move || loop {
            std::thread::sleep(REFRESH_INTERVAL);
            let paths: Vec<PathBuf> = {
                let Ok(cache) = cache.lock() else {
                    continue;
                };
                cache.keys().cloned().collect()
            };
            for path in paths {
                let Some(mtime) = git::repo_mtime(&path) else {
                    // Repository disappeared (e.g. deleted externally).
                    if let Ok(mut cache) = cache.lock() {
                        cache.remove(&path);
                    }
                    continue;
                };
                let changed = {
                    let Ok(cache) = cache.lock() else {
                        continue;
                    };
                    match cache.get(&path) {
                        Some(entry) => entry.mtime != Some(mtime),
                        // Entry vanished (cache cleared by a rescan); skip.
                        None => false,
                    }
                };
                if changed {
                    let info = git::load_info(&path);
                    if let Ok(mut cache) = cache.lock() {
                        if let Some(entry) = cache.get_mut(&path) {
                            entry.info = info;
                            // Capture the mtime *after* the reload so our own
                            // (read-only) git queries can never re-trigger it.
                            entry.mtime = git::repo_mtime(&path);
                        }
                    }
                }
            }
        });
    }

    /// Prefetch every repository's info on background threads. The repo list
    /// is split into a bounded number of slices (one worker per slice, sized
    /// by CPU count), so a slow repository does not stall the ones behind it.
    /// Each repository is handled by exactly one worker, keeping the git
    /// calls per repository serialized. The UI never blocks while this runs;
    /// the info panel shows "loading…" until entries arrive.
    fn start_prefetch(&self) {
        let repos = self.repos.clone();
        let cache = Arc::clone(&self.cache);
        if repos.is_empty() {
            return;
        }
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 8)
            .min(repos.len());
        let chunk = repos.len().div_ceil(workers);

        std::thread::spawn(move || {
            let handles: Vec<_> = repos
                .chunks(chunk)
                .map(|slice| {
                    let slice = slice.to_vec();
                    let cache = Arc::clone(&cache);
                    std::thread::spawn(move || {
                        for path in slice {
                            let info = git::load_info(&path);
                            if let Ok(mut cache) = cache.lock() {
                                let manual = cache.get(&path).is_some_and(|entry| entry.manual);
                                if !manual {
                                    cache.insert(path.clone(), cache_entry(info, false, &path));
                                }
                            }
                        }
                    })
                })
                .collect();
            for handle in handles {
                let _ = handle.join();
            }
        });
    }

    fn move_selection(&mut self, delta: isize) {
        if self.repos.is_empty() {
            return;
        }
        let len = self.repos.len() as isize;
        self.selected = ((self.selected as isize + delta).clamp(0, len - 1)) as usize;
        self.status_msg = None;
        self.scroll = 0;
    }

    fn select_index(&mut self, index: usize) {
        if self.repos.is_empty() {
            return;
        }
        self.selected = index.min(self.repos.len() - 1);
        self.status_msg = None;
        self.scroll = 0;
    }

    fn open_text(&mut self, mode: Mode, title: impl Into<String>, text: impl Into<String>) {
        self.mode = mode;
        self.status_msg = None;
        self.modal_title = git::sanitize(&title.into());
        self.modal_text = text.into().lines().map(git::sanitize).collect();
        self.scroll = 0;
    }

    /// Toggle the batch-update mark on the current repository and move to
    /// the next row, so a range can be marked with repeated presses.
    fn toggle_mark(&mut self) {
        if self.repos.is_empty() {
            return;
        }
        if !self.marked.insert(self.selected) {
            self.marked.remove(&self.selected);
        }
        self.move_selection(1);
    }

    fn start_update(&mut self) {
        // Marked repositories take precedence; without marks, update the
        // current row only.
        let targets: Vec<PathBuf> = if self.marked.is_empty() {
            self.current_path().cloned().into_iter().collect()
        } else {
            let mut indices: Vec<usize> = self.marked.iter().copied().collect();
            indices.sort_unstable();
            indices
                .iter()
                .filter_map(|&i| self.repos.get(i).cloned())
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        self.mode = Mode::Working;
        self.status_msg = None;
        self.scroll = 0;
        if targets.len() == 1 {
            // Single repository: keep the plain synchronous path.
            self.modal_title = git::sanitize(&format!(" Updating {} ", repo_name(&targets[0])));
            self.modal_text = vec![
                "running: git pull --ff-only".into(),
                String::new(),
                "please wait…".into(),
            ];
            self.pending = Some(PendingOp::Update(targets.into_iter().next().unwrap()));
        } else {
            self.start_batch_update(targets);
        }
    }

    /// Update several repositories concurrently on worker threads. The
    /// event loop polls [`App::poll_batch`] for results and shows progress
    /// in the working modal.
    fn start_batch_update(&mut self, targets: Vec<PathBuf>) {
        let total = targets.len();
        self.modal_title = git::sanitize(&format!(" Updating {total} repositories "));
        self.modal_text = vec![
            "running: git pull --ff-only (concurrently)".into(),
            String::new(),
            format!("updated 0/{total} repositories…"),
        ];

        let (tx, rx) = mpsc::channel();
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 8)
            .min(total);
        // Workers pull target indices from a shared counter until the
        // list is exhausted.
        let next = Arc::new(Mutex::new(0usize));
        let shared = Arc::new(targets.clone());
        for _ in 0..workers {
            let tx = tx.clone();
            let next = Arc::clone(&next);
            let targets = Arc::clone(&shared);
            std::thread::spawn(move || loop {
                let index = {
                    let mut next = next.lock().unwrap();
                    if *next >= targets.len() {
                        break;
                    }
                    let index = *next;
                    *next += 1;
                    index
                };
                let result = git::update_repo(&targets[index]);
                if tx.send((index, result)).is_err() {
                    // Receiver gone (app quit mid-batch): stop early.
                    break;
                }
            });
        }

        self.batch = Some(BatchUpdate {
            results: vec![None; targets.len()],
            rx,
            completed: 0,
            targets,
        });
    }

    /// Drain finished batch-update results. While workers are still
    /// running this only refreshes the progress text; once all targets
    /// have reported, it assembles the result modal in list order.
    fn poll_batch(&mut self) {
        let mut refreshed = Vec::new();
        let (done, total) = {
            let Some(batch) = &mut self.batch else {
                return;
            };
            while let Ok((index, result)) = batch.rx.try_recv() {
                refreshed.push(batch.targets[index].clone());
                batch.results[index] = Some(result);
                batch.completed += 1;
            }
            (batch.completed, batch.targets.len())
        };
        // Refresh finished repos after releasing the batch borrow.
        for path in refreshed {
            self.manual_refresh(&path);
        }
        if done < total {
            self.modal_text = vec![
                "running: git pull --ff-only (concurrently)".into(),
                String::new(),
                format!("updated {done}/{total} repositories…"),
            ];
            return;
        }
        let batch = self.batch.take().expect("batch finished above");
        let mut failures = 0;
        let mut sections = Vec::new();
        for (path, result) in batch.targets.iter().zip(batch.results) {
            let result = result.expect("all results collected");
            if result.is_err() {
                failures += 1;
            }
            let body = result.unwrap_or_else(|err| err);
            sections.push(format!("== {} ==\n{body}", repo_name(path)));
        }
        let title = if failures == 0 {
            format!(" Updated {} repositories ", batch.targets.len())
        } else {
            format!(
                " Updated {} repositories ({} failed) ",
                batch.targets.len(),
                failures
            )
        };
        self.open_text(Mode::UpdateResult, title, sections.join("\n\n"));
        self.marked.clear();
    }

    fn finish_update(&mut self, path: &Path) {
        let name = repo_name(path);
        match git::update_repo(path) {
            Ok(output) => self.open_text(Mode::UpdateResult, format!(" Updated {name} "), output),
            Err(err) => self.open_text(Mode::UpdateResult, format!(" Update failed: {name} "), err),
        }
        self.manual_refresh(path);
        self.marked.clear();
    }

    fn start_clone(&mut self) {
        let url = self.input.trim().to_string();
        self.mode = Mode::Working;
        self.status_msg = None;
        self.modal_title = git::sanitize(&format!(" Cloning {url} "));
        self.modal_text = vec![
            "running: git clone …".into(),
            String::new(),
            "please wait…".into(),
        ];
        self.scroll = 0;
        self.pending = Some(PendingOp::Clone);
    }

    fn finish_clone(&mut self) {
        let url = self.input.trim().to_string();
        let before = self.repos.clone();
        match git::clone_repo(&self.root, &url) {
            Ok(output) => {
                self.rescan();
                if let Some(index) = self.repos.iter().position(|p| !before.contains(p)) {
                    self.selected = index;
                    self.manual_refresh_current();
                }
                self.open_text(Mode::Message, " Clone complete ", output);
            }
            Err(err) => self.open_text(Mode::Message, " Clone failed ", err),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyAction {
        // Ctrl-C always quits, no matter which view is active.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return KeyAction::Quit;
        }
        match self.mode {
            Mode::Browse => self.handle_browse_key(key),
            Mode::Help
            | Mode::Status
            | Mode::Log
            | Mode::Branches
            | Mode::UpdateResult
            | Mode::Message => self.handle_modal_key(key),
            Mode::InputClone => self.handle_input_key(key),
            Mode::Working => KeyAction::None,
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> KeyAction {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => KeyAction::Quit,
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                KeyAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                KeyAction::None
            }
            KeyCode::Char('g') => {
                self.select_index(0);
                KeyAction::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.select_index(self.repos.len().saturating_sub(1));
                KeyAction::None
            }
            KeyCode::Char(' ') => {
                self.toggle_mark();
                KeyAction::None
            }
            KeyCode::Char('A') => {
                if self.marked.is_empty() {
                    self.status_msg = Some("no marks to clear".into());
                } else {
                    self.marked.clear();
                    self.status_msg = Some("marks cleared".into());
                }
                KeyAction::None
            }
            KeyCode::Char('u') => {
                self.start_update();
                KeyAction::None
            }
            KeyCode::Char('s') | KeyCode::Enter => self.show_status(),
            KeyCode::Char('b') => self.show_log(),
            KeyCode::Char('l') => self.show_branches(),
            KeyCode::Char('n') => {
                self.input.clear();
                self.scroll = 0;
                self.mode = Mode::InputClone;
                KeyAction::None
            }
            KeyCode::Char('o') => {
                self.status_msg = None;
                if let Some(path) = self.current_path() {
                    match git::open_in_file_manager(path) {
                        Ok(()) => self.status_msg = Some("opened in file manager".into()),
                        Err(err) => self.open_text(Mode::Message, " Cannot open folder ", err),
                    }
                }
                KeyAction::None
            }
            KeyCode::Char('O') => {
                self.status_msg = None;
                let Some(info) = self.current_info() else {
                    self.open_text(
                        Mode::Message,
                        " Info not ready ",
                        "repository info is still loading, try again in a moment",
                    );
                    return KeyAction::None;
                };
                let Some((_, remote)) = info.remote else {
                    self.open_text(
                        Mode::Message,
                        " No remote ",
                        "this repository has no configured remote",
                    );
                    return KeyAction::None;
                };
                let browser = git::browser_url(&remote);
                match git::open_url(&browser) {
                    Ok(()) => self.status_msg = Some(format!("opened {browser}")),
                    Err(err) => self.open_text(Mode::Message, " Cannot open remote ", err),
                }
                KeyAction::None
            }
            KeyCode::Char('r') => {
                self.rescan();
                self.status_msg = Some(format!("rescanned: {} repos", self.repos.len()));
                KeyAction::None
            }
            KeyCode::Char('R') => {
                self.manual_refresh_current();
                self.status_msg = Some("info reloaded".into());
                KeyAction::None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(3);
                KeyAction::None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(3);
                KeyAction::None
            }
            KeyCode::Char('h') | KeyCode::Char('?') => {
                self.open_help();
                KeyAction::None
            }
            _ => KeyAction::None,
        }
    }

    fn show_status(&mut self) -> KeyAction {
        if let Some(path) = self.current_path().cloned() {
            let name = self.current_name().unwrap_or_default();
            self.open_text(
                Mode::Status,
                format!(" {name} · git status "),
                git::status_short(&path),
            );
        }
        KeyAction::None
    }

    fn show_log(&mut self) -> KeyAction {
        if let Some(path) = self.current_path().cloned() {
            let name = self.current_name().unwrap_or_default();
            self.open_text(
                Mode::Log,
                format!(" {name} · git log "),
                git::recent_log(&path),
            );
        }
        KeyAction::None
    }

    fn show_branches(&mut self) -> KeyAction {
        if let Some(path) = self.current_path().cloned() {
            let name = self.current_name().unwrap_or_default();
            self.open_text(
                Mode::Branches,
                format!(" {name} · git branch "),
                git::branch_list(&path),
            );
        }
        KeyAction::None
    }

    fn open_help(&mut self) {
        self.prev_mode = self.mode;
        self.scroll = 0;
        self.mode = Mode::Help;
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> KeyAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
                self.status_msg = None;
                KeyAction::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                KeyAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                KeyAction::None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                KeyAction::None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                KeyAction::None
            }
            KeyCode::Char('h') | KeyCode::Char('?') => {
                if self.mode == Mode::Help {
                    self.mode = self.prev_mode;
                } else {
                    self.open_help();
                }
                KeyAction::None
            }
            _ => KeyAction::None,
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> KeyAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
                KeyAction::None
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                KeyAction::None
            }
            KeyCode::Backspace => {
                self.input.pop();
                KeyAction::None
            }
            KeyCode::Enter => {
                if self.input.trim().is_empty() {
                    KeyAction::None
                } else {
                    self.start_clone();
                    KeyAction::None
                }
            }
            _ => KeyAction::None,
        }
    }

    fn run(&mut self, mut terminal: Terminal<impl Backend>) -> io::Result<()> {
        loop {
            terminal.draw(|f| View::draw(self, f))?;
            if !self.scanned {
                self.scanned = true;
                self.rescan();
                self.start_watcher();
                continue;
            }
            // Poll with a timeout so the loop keeps waking up while the
            // background prefetch fills the cache: fresh entries appear
            // within one poll interval even without any key press.
            if event::poll(Duration::from_millis(100))? {
                let action = match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
                    _ => KeyAction::None,
                };
                if matches!(action, KeyAction::Quit) {
                    break;
                }
            }
            if let Some(op) = self.pending.take() {
                // Draw the "working…" frame before running the potentially
                // slow git operation, so the UI gives feedback first.
                terminal.draw(|f| View::draw(self, f))?;
                match op {
                    PendingOp::Update(path) => self.finish_update(&path),
                    PendingOp::Clone => self.finish_clone(),
                }
            }
            if self.batch.is_some() {
                self.poll_batch();
            }
        }
        Ok(())
    }
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout()))
}

/// Restores the terminal even when `main` returns early or panics.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

fn main() -> io::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    if !root.is_dir() {
        eprintln!("repomgr: not a directory: {}", root.display());
        std::process::exit(1);
    }
    let root = root.canonicalize().unwrap_or(root);

    // Guard is active before `init_terminal` so a partially-initialized
    // terminal (raw mode on, alternate screen not yet entered) is still
    // restored on failure or panic.
    let _guard = TerminalGuard;
    let terminal = init_terminal()?;
    let mut app = App::new(root);
    app.run(terminal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("repomgr-main-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_src(dir: &Path) {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q", "-b", "main", "."]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "hi").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
    }

    fn clone_into(root: &Path, src: &Path, name: &str) {
        let out = std::process::Command::new("git")
            .args(["clone", "-q", src.to_str().unwrap(), name])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A root with clones of a source repo living outside the root, so
    /// `git pull --ff-only` has an upstream and succeeds.
    fn make_root(tag: &str, names: &[&str]) -> (PathBuf, PathBuf) {
        let src = temp_dir(&format!("{tag}-src"));
        init_src(&src);
        let root = temp_dir(tag);
        for name in names {
            clone_into(&root, &src, name);
        }
        (root, src)
    }

    #[test]
    fn batch_update_pulls_all_marked_repos() {
        let (root, src) = make_root("batch", &["a", "b", "c"]);
        let mut app = App::new(root.clone());
        app.rescan();
        assert_eq!(app.repos.len(), 3);
        app.marked.extend(0..3);

        app.start_update();
        assert!(app.batch.is_some(), "multi-target update should batch");
        assert!(app.pending.is_none(), "batch must not use the sync path");

        for _ in 0..1000 {
            app.poll_batch();
            if app.batch.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.batch.is_none(), "batch should finish within 10s");
        assert_eq!(app.mode, Mode::UpdateResult);

        // The report keeps one section per repo, in list order.
        let text = app.modal_text.join("\n");
        let mut last = 0;
        for name in ["a", "b", "c"] {
            let at = text
                .find(&format!("== {name} =="))
                .unwrap_or_else(|| panic!("missing section for {name}: {text}"));
            assert!(at >= last, "sections out of order: {text}");
            last = at;
        }
        assert!(app.marked.is_empty(), "marks are consumed by the update");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn single_update_stays_synchronous() {
        let (root, src) = make_root("single", &["a"]);
        let mut app = App::new(root.clone());
        app.rescan();

        app.start_update();
        assert!(app.batch.is_none(), "single update must not batch");
        let Some(PendingOp::Update(path)) = app.pending.take() else {
            panic!("single update should use the sync pending path");
        };

        app.finish_update(&path);
        assert_eq!(app.mode, Mode::UpdateResult);
        assert!(app.modal_title.contains("Updated a"));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&src);
    }
}
