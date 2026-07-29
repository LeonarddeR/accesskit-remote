//! WSLg window-id enrichment.
//!
//! Weston logs one `ClientGetAppidReq` line per toplevel it hands an appId to,
//! naming the Wayland window id. Tailing that log yields native window ids the
//! AT-SPI source cannot see. The logged pid belongs to the WSL VM's global
//! namespace rather than the user distro, so correlation is by appId only.

use accesskit_remote::WindowId;
use accesskit_remote_server::{SourceEvent, TreeSource, WindowDescriptor};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How recently an entry must have arrived for reactive assignment: a window
/// maps and gets its log line at most a couple of seconds before the AT-SPI
/// source announces it, while entries older than this belong to windows from
/// before the current poll cycle.
const FRESH_WINDOW: Duration = Duration::from_secs(10);

/// One `ClientGetAppidReq` line worth of state: an appId and its window id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppidEntry {
    pub app_id: String,
    pub window_id: u64,
}

/// Parses a weston `ClientGetAppidReq` line, tolerating any log prefix.
/// Lines without both a non-empty `appId:` and a `WindowId:0x<hex>` yield
/// `None`.
pub fn parse_appid_line(line: &str) -> Option<AppidEntry> {
    let request = line.split_once("ClientGetAppidReq:")?.1;
    let app_id = field(request, "appId:")?;
    if app_id.is_empty() {
        return None;
    }
    let window_id = u64::from_str_radix(field(request, "WindowId:0x")?, 16).ok()?;
    Some(AppidEntry {
        app_id: app_id.to_owned(),
        window_id,
    })
}

/// The whitespace-delimited token directly after `key`.
fn field<'a>(request: &'a str, key: &str) -> Option<&'a str> {
    let value = request.split_once(key)?.1;
    let end = value.find(char::is_whitespace).unwrap_or(value.len());
    Some(&value[..end])
}

/// Window ids seen in the log but not yet handed to a window, kept in
/// per-appId arrival order. Weston logs one line per toplevel in launch
/// order, so the earliest unconsumed id for an appId belongs to that app's
/// next window.
#[derive(Debug, Default)]
pub struct AppidLedger {
    queues: HashMap<String, VecDeque<(u64, Instant)>>,
    seen: HashSet<u64>,
}

impl AppidLedger {
    /// Records an entry, ignoring window ids seen before.
    pub fn push(&mut self, entry: AppidEntry) {
        self.push_at(entry, Instant::now());
    }

    /// Records an entry as having arrived at `at`, ignoring window ids seen
    /// before.
    pub fn push_at(&mut self, entry: AppidEntry, at: Instant) {
        if !self.seen.insert(entry.window_id) {
            return;
        }
        self.queues
            .entry(entry.app_id)
            .or_default()
            .push_back((entry.window_id, at));
    }

    /// Takes the earliest unconsumed window id for `app_id`.
    pub fn assign(&mut self, app_id: &str) -> Option<u64> {
        self.queues.get_mut(app_id)?.pop_front().map(|(id, _)| id)
    }

    /// Takes the earliest unconsumed window id for `app_id` that arrived
    /// within [`FRESH_WINDOW`] of `now`. Older entries belong to windows from
    /// before this poll cycle and are left for the initial-state count gate.
    pub fn assign_fresh(&mut self, app_id: &str, now: Instant) -> Option<u64> {
        let queue = self.queues.get_mut(app_id)?;
        let index = queue
            .iter()
            .position(|(_, at)| now.saturating_duration_since(*at) <= FRESH_WINDOW)?;
        queue.remove(index).map(|(id, _)| id)
    }

    /// Takes the one unconsumed window id that arrived within
    /// [`FRESH_WINDOW`] of `now` when exactly one such entry exists across
    /// all appIds.
    pub fn assign_sole_fresh(&mut self, now: Instant) -> Option<u64> {
        let mut found: Option<(String, usize)> = None;
        for (app_id, queue) in &self.queues {
            for (index, (_, at)) in queue.iter().enumerate() {
                if now.saturating_duration_since(*at) <= FRESH_WINDOW {
                    if found.is_some() {
                        return None;
                    }
                    found = Some((app_id.clone(), index));
                }
            }
        }
        let (app_id, index) = found?;
        self.queues.get_mut(&app_id)?.remove(index).map(|(id, _)| id)
    }

    /// Takes the one unconsumed window id when exactly one exists across all
    /// appIds, which is the only case where an unkeyed window can be matched
    /// without ambiguity.
    pub fn assign_sole_entry(&mut self) -> Option<u64> {
        if self.total_unconsumed() != 1 {
            return None;
        }
        self.queues
            .values_mut()
            .find_map(VecDeque::pop_front)
            .map(|(id, _)| id)
    }

    /// How many window ids for `app_id` are still unconsumed.
    pub fn unconsumed(&self, app_id: &str) -> usize {
        self.queues.get(app_id).map_or(0, VecDeque::len)
    }

    /// How many window ids are still unconsumed across all appIds.
    pub fn total_unconsumed(&self) -> usize {
        self.queues.values().map(VecDeque::len).sum()
    }
}

/// The weston log the tail reads when the environment says nothing.
const DEFAULT_LOG_PATH: &str = "/mnt/wslg/weston.log";

/// Overrides [`DEFAULT_LOG_PATH`]; set but empty disables the tail entirely.
const LOG_PATH_VAR: &str = "ACCESSKIT_REMOTED_WESTON_LOG";

/// An append-only reader over the weston log, resuming from the byte offset
/// it last stopped at.
#[derive(Debug)]
pub struct WestonLogTail {
    path: PathBuf,
    offset: u64,
    /// Bytes after the last newline of the previous read, prepended to the
    /// next one so a line split across reads still parses.
    partial: Vec<u8>,
}

impl WestonLogTail {
    /// Opens the configured log, or `None` when it is disabled by an empty
    /// override or cannot be read.
    pub fn open_default() -> Option<Self> {
        Self::open(log_path(std::env::var_os(LOG_PATH_VAR))?)
    }

    /// Opens `path`, or `None` when it cannot be read.
    pub fn open(path: impl Into<PathBuf>) -> Option<Self> {
        let path = path.into();
        File::open(&path).ok()?;
        Some(Self {
            path,
            offset: 0,
            partial: Vec::new(),
        })
    }

    /// Feeds every line appended since the last poll into `ledger`. A file
    /// shorter than the saved offset was replaced by a VM restart, so reading
    /// restarts from zero.
    pub fn poll(&mut self, ledger: &mut AppidLedger) {
        let Ok(mut file) = File::open(&self.path) else {
            return;
        };
        match file.metadata() {
            Ok(metadata) if metadata.len() < self.offset => {
                self.offset = 0;
                self.partial.clear();
            }
            Ok(_) => {}
            Err(_) => return,
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut fresh = Vec::new();
        let Ok(read) = file.read_to_end(&mut fresh) else {
            return;
        };
        self.offset += read as u64;
        self.partial.extend_from_slice(&fresh);

        let mut start = 0;
        while let Some(newline) = self.partial[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + newline;
            let line = String::from_utf8_lossy(&self.partial[start..end]);
            if let Some(entry) = parse_appid_line(&line) {
                ledger.push(entry);
            }
            start = end + 1;
        }
        self.partial.drain(..start);
    }
}

/// Resolves the log path from the override variable's value.
fn log_path(var: Option<OsString>) -> Option<PathBuf> {
    match var {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(value.into()),
        None => Some(DEFAULT_LOG_PATH.into()),
    }
}

/// Wraps a tree source, filling in `native_window_id` from window ids the
/// weston log reveals.
pub struct WslgSource<S> {
    inner: S,
    /// `None` in tests, which seed the ledger directly.
    tail: Option<WestonLogTail>,
    ledger: AppidLedger,
}

impl<S: TreeSource> WslgSource<S> {
    pub fn new(inner: S, tail: WestonLogTail) -> Self {
        Self {
            inner,
            tail: Some(tail),
            ledger: AppidLedger::default(),
        }
    }

    #[cfg(test)]
    fn with_ledger(inner: S, ledger: AppidLedger) -> Self {
        Self {
            inner,
            tail: None,
            ledger,
        }
    }

    fn poll_log(&mut self) {
        if let Some(tail) = self.tail.as_mut() {
            tail.poll(&mut self.ledger);
        }
    }
}

impl<S: TreeSource> TreeSource for WslgSource<S> {
    fn initial_state(
        &mut self,
    ) -> (Vec<(WindowDescriptor, accesskit::TreeUpdate)>, Option<WindowId>) {
        self.poll_log();
        let (mut windows, focus) = self.inner.initial_state();

        let mut counts: HashMap<&str, usize> = HashMap::new();
        for (descriptor, _) in &windows {
            if let Some(app_id) = descriptor.app.app_id.as_deref() {
                *counts.entry(app_id).or_default() += 1;
            }
        }
        // Decided before any assignment, so an app whose queue matches keeps
        // matching as that queue drains.
        let matched: HashSet<String> = counts
            .iter()
            .filter(|(app_id, count)| self.ledger.unconsumed(app_id) == **count)
            .map(|(app_id, _)| (*app_id).to_owned())
            .collect();
        let sole = windows.len() == 1 && self.ledger.total_unconsumed() == 1;

        for (descriptor, _) in windows.iter_mut() {
            let native_window_id = match descriptor.app.app_id.as_deref() {
                Some(app_id) if matched.contains(app_id) => self.ledger.assign(app_id),
                None if sole => self.ledger.assign_sole_entry(),
                _ => None,
            };
            assign_native_id(descriptor, native_window_id);
        }
        (windows, focus)
    }

    fn perform(&mut self, window: WindowId, request: &accesskit::ActionRequest) {
        self.inner.perform(window, request);
    }

    fn poll_events(&mut self) -> Vec<SourceEvent> {
        self.poll_log();
        let now = Instant::now();
        let mut events = self.inner.poll_events();
        for event in &mut events {
            if let SourceEvent::WindowAdded { descriptor, .. } = event {
                let native_window_id = match descriptor.app.app_id.as_deref() {
                    Some(app_id) => self.ledger.assign_fresh(app_id, now),
                    None => self.ledger.assign_sole_fresh(now),
                };
                assign_native_id(descriptor, native_window_id);
            }
        }
        events
    }
}

/// Records a matched window id on `descriptor` and reports the match.
fn assign_native_id(descriptor: &mut WindowDescriptor, native_window_id: Option<u64>) {
    let Some(native_window_id) = native_window_id else {
        return;
    };
    descriptor.native_window_id = Some(native_window_id);
    eprintln!(
        "accesskit_remoted: wslg matched window {} (app {}) to native window id {native_window_id:#x}",
        descriptor.id.0,
        descriptor.app.app_id.as_deref().unwrap_or("<none>")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit_remote::AppInfo;

    const TEXT_EDITOR: &str = "org.gnome.TextEditor";
    const CALCULATOR: &str = "org.gnome.Calculator";
    const SAMPLE: &str = "[19:52:15.560] Client: ClientGetAppidReq: pid:8652 appId:org.gnome.TextEditor WindowId:0x8b";

    fn entry(app_id: &str, window_id: u64) -> AppidEntry {
        AppidEntry {
            app_id: app_id.into(),
            window_id,
        }
    }

    fn ledger_with(entries: impl IntoIterator<Item = AppidEntry>) -> AppidLedger {
        let mut ledger = AppidLedger::default();
        for entry in entries {
            ledger.push(entry);
        }
        ledger
    }

    #[test]
    fn parses_client_get_appid_req_line() {
        assert_eq!(
            parse_appid_line(SAMPLE),
            Some(AppidEntry {
                app_id: "org.gnome.TextEditor".into(),
                window_id: 0x8b,
            })
        );
    }

    #[test]
    fn ignores_noise_and_unrelated_lines() {
        for line in [
            "[19:48:29.479] Client: ClientGetAppidReq: WindowId:0x79 does not have appId, or not top level window.",
            "[19:48:29.479] Client: ClientGetAppidReq: pid:8652 appId: WindowId:0x79",
            "[19:48:29.479] Client: ClientGetAppidReq: pid:8652 appId:org.gnome.TextEditor",
            "[19:52:15.560] Output repaint window is 7 ms",
            "",
        ] {
            assert_eq!(parse_appid_line(line), None, "expected no entry for {line:?}");
        }
    }

    #[test]
    fn ledger_assigns_fifo_per_app_id() {
        let mut ledger = ledger_with([
            entry(TEXT_EDITOR, 0x8b),
            entry(CALCULATOR, 0x8c),
            entry(TEXT_EDITOR, 0x8d),
        ]);
        assert_eq!(ledger.unconsumed(TEXT_EDITOR), 2);
        assert_eq!(ledger.total_unconsumed(), 3);
        assert_eq!(ledger.assign(TEXT_EDITOR), Some(0x8b));
        assert_eq!(ledger.assign(TEXT_EDITOR), Some(0x8d));
        assert_eq!(ledger.assign(TEXT_EDITOR), None);
        assert_eq!(ledger.assign(CALCULATOR), Some(0x8c));
        assert_eq!(ledger.total_unconsumed(), 0);
    }

    #[test]
    fn ledger_dedupes_repeated_window_ids() {
        let mut ledger = ledger_with([entry(TEXT_EDITOR, 0x8b), entry(TEXT_EDITOR, 0x8b)]);
        assert_eq!(ledger.unconsumed(TEXT_EDITOR), 1);
        assert_eq!(ledger.assign(TEXT_EDITOR), Some(0x8b));
        // A re-read of the log must not resurrect an already consumed id.
        ledger.push(entry(TEXT_EDITOR, 0x8b));
        assert_eq!(ledger.unconsumed(TEXT_EDITOR), 0);
        assert_eq!(ledger.assign(TEXT_EDITOR), None);
    }

    #[test]
    fn ledger_none_for_unknown_app_id() {
        let mut ledger = ledger_with([entry(TEXT_EDITOR, 0x8b)]);
        assert_eq!(ledger.assign("libreoffice-writer"), None);
        assert_eq!(ledger.unconsumed("libreoffice-writer"), 0);
        assert_eq!(ledger.total_unconsumed(), 1);
    }

    #[test]
    fn reactive_assignment_ignores_stale_entries() {
        let now = Instant::now();
        let old = now - FRESH_WINDOW - Duration::from_secs(50);
        let mut ledger = AppidLedger::default();
        ledger.push_at(entry(TEXT_EDITOR, 0x10), old);
        ledger.push_at(entry(TEXT_EDITOR, 0x20), now);
        assert_eq!(ledger.assign_fresh(TEXT_EDITOR, now), Some(0x20));
        assert_eq!(ledger.assign_fresh(TEXT_EDITOR, now), None);
        // The stale entry stays for the initial-state count gate.
        assert_eq!(ledger.unconsumed(TEXT_EDITOR), 1);
        assert_eq!(ledger.assign(TEXT_EDITOR), Some(0x10));
    }

    #[test]
    fn sole_fresh_assignment_requires_exactly_one_fresh() {
        let now = Instant::now();
        let old = now - FRESH_WINDOW - Duration::from_secs(50);
        let mut ledger = AppidLedger::default();
        ledger.push_at(entry(TEXT_EDITOR, 0x10), old);
        ledger.push_at(entry(CALCULATOR, 0x11), old);
        ledger.push_at(entry("libreoffice-writer", 0x30), now);
        assert_eq!(ledger.assign_sole_fresh(now), Some(0x30));
        assert_eq!(ledger.assign_sole_fresh(now), None);
        assert_eq!(ledger.total_unconsumed(), 2);

        let mut two_fresh = AppidLedger::default();
        two_fresh.push_at(entry(TEXT_EDITOR, 0x40), now);
        two_fresh.push_at(entry(CALCULATOR, 0x41), now);
        assert_eq!(two_fresh.assign_sole_fresh(now), None);
    }

    #[test]
    fn window_added_pairs_with_fresh_entries_only() {
        let now = Instant::now();
        let old = now - FRESH_WINDOW - Duration::from_secs(50);
        let mut ledger = AppidLedger::default();
        ledger.push_at(entry(TEXT_EDITOR, 0x10), old);
        ledger.push_at(entry(TEXT_EDITOR, 0x20), now);
        let stub = StubSource {
            windows: Vec::new(),
            events: vec![window_added(1, Some(TEXT_EDITOR))],
        };
        let mut source = WslgSource::with_ledger(stub, ledger);
        let events = source.poll_events();
        assert_eq!(added_native_ids(&events), vec![Some(0x20)]);
    }

    #[test]
    fn sole_entry_assignment_requires_exactly_one() {
        let mut empty = AppidLedger::default();
        assert_eq!(empty.assign_sole_entry(), None);

        let mut ledger = ledger_with([entry(TEXT_EDITOR, 0x8b), entry(CALCULATOR, 0x8c)]);
        assert_eq!(ledger.assign_sole_entry(), None);
        assert_eq!(ledger.total_unconsumed(), 2);

        assert_eq!(ledger.assign(TEXT_EDITOR), Some(0x8b));
        assert_eq!(ledger.assign_sole_entry(), Some(0x8c));
        assert_eq!(ledger.assign_sole_entry(), None);
    }

    /// A log file of its own, removed when the test ends.
    struct TempLog(PathBuf);

    impl TempLog {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("accesskit-remoted-{name}.log"));
            std::fs::write(&path, b"").unwrap();
            Self(path)
        }

        fn append(&self, text: &str) {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.0)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        }

        fn replace_with(&self, text: &str) {
            std::fs::write(&self.0, text.as_bytes()).unwrap();
        }
    }

    impl Drop for TempLog {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn tail_reads_appended_lines_and_carries_partial_ones() {
        let log = TempLog::new("appends");
        let mut tail = WestonLogTail::open(&log.0).unwrap();
        let mut ledger = AppidLedger::default();

        log.append(&format!(
            "{SAMPLE}\n[19:52:15.900] Client: ClientGetAppidReq: pid:1 appId:{CALCULATOR} Win"
        ));
        tail.poll(&mut ledger);
        assert_eq!(ledger.unconsumed(TEXT_EDITOR), 1);
        assert_eq!(
            ledger.unconsumed(CALCULATOR),
            0,
            "the truncated line must wait for its newline"
        );

        log.append("dowId:0x8c\n");
        tail.poll(&mut ledger);
        assert_eq!(ledger.assign(CALCULATOR), Some(0x8c));

        tail.poll(&mut ledger);
        assert_eq!(
            ledger.total_unconsumed(),
            1,
            "a poll with nothing appended must add nothing"
        );
    }

    #[test]
    fn tail_restarts_at_zero_when_the_log_shrinks() {
        let log = TempLog::new("shrink");
        let mut tail = WestonLogTail::open(&log.0).unwrap();
        let mut ledger = AppidLedger::default();

        log.append(&format!("{SAMPLE}\n"));
        tail.poll(&mut ledger);
        assert_eq!(ledger.assign(TEXT_EDITOR), Some(0x8b));

        log.replace_with(&format!("ClientGetAppidReq: appId:{CALCULATOR} WindowId:0x2\n"));
        tail.poll(&mut ledger);
        assert_eq!(ledger.assign(CALCULATOR), Some(0x2));
    }

    #[test]
    fn log_path_honours_the_override_variable() {
        assert_eq!(log_path(None), Some(PathBuf::from(DEFAULT_LOG_PATH)));
        assert_eq!(
            log_path(Some(OsString::from("/tmp/elsewhere.log"))),
            Some(PathBuf::from("/tmp/elsewhere.log"))
        );
        assert_eq!(
            log_path(Some(OsString::new())),
            None,
            "an empty override disables the tail"
        );
    }

    #[test]
    fn open_returns_none_for_an_unreadable_log() {
        let missing = std::env::temp_dir().join("accesskit-remoted-does-not-exist.log");
        assert!(WestonLogTail::open(missing).is_none());
    }

    #[derive(Default)]
    struct StubSource {
        windows: Vec<(WindowDescriptor, accesskit::TreeUpdate)>,
        events: Vec<SourceEvent>,
    }

    impl TreeSource for StubSource {
        fn initial_state(
            &mut self,
        ) -> (Vec<(WindowDescriptor, accesskit::TreeUpdate)>, Option<WindowId>) {
            (std::mem::take(&mut self.windows), None)
        }

        fn perform(&mut self, _window: WindowId, _request: &accesskit::ActionRequest) {}

        fn poll_events(&mut self) -> Vec<SourceEvent> {
            std::mem::take(&mut self.events)
        }
    }

    fn descriptor(id: u64, app_id: Option<&str>) -> WindowDescriptor {
        WindowDescriptor {
            id: WindowId(id),
            title: "window".into(),
            app: AppInfo {
                app_id: app_id.map(Into::into),
                ..AppInfo::default()
            },
            native_window_id: None,
        }
    }

    fn tree() -> accesskit::TreeUpdate {
        accesskit::TreeUpdate {
            nodes: vec![(
                accesskit::NodeId(0),
                accesskit::Node::new(accesskit::Role::Window),
            )],
            tree: Some(accesskit::Tree::new(accesskit::NodeId(0))),
            tree_id: accesskit::TreeId::ROOT,
            focus: accesskit::NodeId(0),
        }
    }

    fn window_added(id: u64, app_id: Option<&str>) -> SourceEvent {
        SourceEvent::WindowAdded {
            descriptor: descriptor(id, app_id),
            tree: tree(),
        }
    }

    fn added_native_ids(events: &[SourceEvent]) -> Vec<Option<u64>> {
        events
            .iter()
            .filter_map(|event| match event {
                SourceEvent::WindowAdded { descriptor, .. } => Some(descriptor.native_window_id),
                _ => None,
            })
            .collect()
    }

    fn initial_native_ids(windows: &[(WindowDescriptor, accesskit::TreeUpdate)]) -> Vec<Option<u64>> {
        windows
            .iter()
            .map(|(descriptor, _)| descriptor.native_window_id)
            .collect()
    }

    #[test]
    fn enriches_window_added_from_ledger() {
        let inner = StubSource {
            events: vec![
                window_added(1, Some(TEXT_EDITOR)),
                SourceEvent::FocusChanged(Some(WindowId(1))),
            ],
            ..StubSource::default()
        };
        let mut source = WslgSource::with_ledger(
            inner,
            ledger_with([entry(TEXT_EDITOR, 0x8b), entry(CALCULATOR, 0x8c)]),
        );

        let events = source.poll_events();
        assert_eq!(added_native_ids(&events), vec![Some(0x8b)]);
        assert!(
            matches!(events[1], SourceEvent::FocusChanged(Some(WindowId(1)))),
            "events other than WindowAdded pass through untouched"
        );
        assert_eq!(source.ledger.unconsumed(CALCULATOR), 1);
    }

    #[test]
    fn window_added_without_app_id_takes_the_sole_entry() {
        let mut ambiguous = WslgSource::with_ledger(
            StubSource {
                events: vec![window_added(1, None)],
                ..StubSource::default()
            },
            ledger_with([entry(TEXT_EDITOR, 0x8b), entry(CALCULATOR, 0x8c)]),
        );
        assert_eq!(added_native_ids(&ambiguous.poll_events()), vec![None]);
        assert_eq!(ambiguous.ledger.total_unconsumed(), 2);

        let mut sole = WslgSource::with_ledger(
            StubSource {
                events: vec![window_added(1, None)],
                ..StubSource::default()
            },
            ledger_with([entry("libreoffice-writer", 0x90)]),
        );
        assert_eq!(added_native_ids(&sole.poll_events()), vec![Some(0x90)]);
    }

    #[test]
    fn initial_state_skips_app_on_count_mismatch() {
        let inner = StubSource {
            windows: vec![(descriptor(1, Some(TEXT_EDITOR)), tree())],
            ..StubSource::default()
        };
        let mut source = WslgSource::with_ledger(
            inner,
            ledger_with([entry(TEXT_EDITOR, 0x8b), entry(TEXT_EDITOR, 0x8d)]),
        );

        let (windows, _) = source.initial_state();
        assert_eq!(initial_native_ids(&windows), vec![None]);
        assert_eq!(
            source.ledger.total_unconsumed(),
            2,
            "stale lines must leave the queue untouched"
        );
    }

    #[test]
    fn initial_state_assigns_when_counts_match() {
        let inner = StubSource {
            windows: vec![
                (descriptor(1, Some(TEXT_EDITOR)), tree()),
                (descriptor(2, Some(TEXT_EDITOR)), tree()),
                (descriptor(3, Some(CALCULATOR)), tree()),
            ],
            ..StubSource::default()
        };
        let mut source = WslgSource::with_ledger(
            inner,
            ledger_with([
                entry(TEXT_EDITOR, 0x8b),
                entry(TEXT_EDITOR, 0x8d),
                entry(CALCULATOR, 0x8c),
            ]),
        );

        let (windows, _) = source.initial_state();
        assert_eq!(
            initial_native_ids(&windows),
            vec![Some(0x8b), Some(0x8d), Some(0x8c)]
        );
        assert_eq!(source.ledger.total_unconsumed(), 0);
    }

    #[test]
    fn initial_state_uses_the_sole_entry_for_a_lone_unkeyed_window() {
        let mut source = WslgSource::with_ledger(
            StubSource {
                windows: vec![(descriptor(1, None), tree())],
                ..StubSource::default()
            },
            ledger_with([entry("libreoffice-writer", 0x90)]),
        );
        let (windows, _) = source.initial_state();
        assert_eq!(initial_native_ids(&windows), vec![Some(0x90)]);

        let mut crowded = WslgSource::with_ledger(
            StubSource {
                windows: vec![(descriptor(1, None), tree()), (descriptor(2, None), tree())],
                ..StubSource::default()
            },
            ledger_with([entry("libreoffice-writer", 0x91)]),
        );
        let (windows, _) = crowded.initial_state();
        assert_eq!(initial_native_ids(&windows), vec![None, None]);
    }
}
