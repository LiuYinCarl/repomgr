//! repomgr — a tiny TUI for managing the git repositories in the current
//! directory.
//!
//! Architecture (inspired by basilk):
//!   main.rs  app state + event loop + key handling
//!   view.rs  all drawing (list/info panes, modals, help)
//!   git.rs   shelling out to the `git` CLI

mod git;
mod view;

use std::collections::HashMap;
use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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
    Update,
    Clone,
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
    /// Whether the initial directory scan has finished. The first frame is
    /// drawn before the scan so the screen never stays blank while git
    /// queries run at startup.
    scanned: bool,
    mode: Mode,
    /// Mode to return to when closing the help modal.
    prev_mode: Mode,
    /// Operation waiting to run in the event loop, if any.
    pending: Option<PendingOp>,
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
            scanned: false,
            mode: Mode::Browse,
            prev_mode: Mode::Browse,
            pending: None,
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
        self.current_path().map(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            git::sanitize(&name)
        })
    }

    fn rescan(&mut self) {
        self.repos = git::discover_repos(&self.root);
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

    /// Synchronously reload the selected repo and mark the cache entry as
    /// manually refreshed so the background worker leaves it alone.
    fn manual_refresh_current(&mut self) {
        let Some(path) = self.current_path().cloned() else {
            return;
        };
        let info = git::load_info(&path);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(path.clone(), cache_entry(info, true, &path));
        }
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

    fn start_update(&mut self) {
        let name = self.current_name().unwrap_or_default();
        self.mode = Mode::Working;
        self.status_msg = None;
        self.modal_title = git::sanitize(&format!(" Updating {name} "));
        self.modal_text = vec![
            "running: git pull --ff-only".into(),
            String::new(),
            "please wait…".into(),
        ];
        self.scroll = 0;
        self.pending = Some(PendingOp::Update);
    }

    fn finish_update(&mut self) {
        let name = self.current_name().unwrap_or_default();
        let result = self.current_path().map(|path| git::update_repo(path));
        match result {
            Some(Ok(output)) => {
                self.open_text(Mode::UpdateResult, format!(" Updated {name} "), output)
            }
            Some(Err(err)) => {
                self.open_text(Mode::UpdateResult, format!(" Update failed: {name} "), err)
            }
            None => self.mode = Mode::Browse,
        }
        self.manual_refresh_current();
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
            KeyCode::Char('u') => {
                if self.current_path().is_some() {
                    self.start_update();
                }
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
                    PendingOp::Update => self.finish_update(),
                    PendingOp::Clone => self.finish_clone(),
                }
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
