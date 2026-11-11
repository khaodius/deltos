//! Background work the person may want to watch or stop: statements that run, tables
//! that load and count, edits being saved. Whatever starts the work registers it with
//! [`start_task`], reports rows with [`progress`] and ends it with [`finish`]; the Tasks
//! panel and the title bar list it from here.
//!
//! Most work ends in a blink, so a task only shows once it has run for
//! [`VISIBLE_AFTER`], and a quick success is not kept in the finished list; a failure or
//! a cancel always is. The finished list keeps the last [`FINISHED_LIMIT`].

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deltos_grid::{GridCache, RowCount, Status};
use gpui::{App, AppContext, Context, Entity, Global, SharedString};

/// How long work runs before it is listed.
pub const VISIBLE_AFTER: Duration = Duration::from_millis(400);
/// How many finished tasks are kept.
pub const FINISHED_LIMIT: usize = 100;
/// How often the running tasks' clocks move on screen, while any is listed.
const TICK: Duration = Duration::from_secs(1);
/// How often a followed grid cache is looked at.
const FOLLOW_EVERY: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    Done,
    Failed(String),
    Cancelled,
}

/// Stops the work behind a task; it runs on the UI thread.
pub type CancelFn = Rc<dyn Fn(&mut App)>;

pub struct TaskEntry {
    pub id: TaskId,
    pub label: SharedString,
    /// The connection's name, when the work runs on one.
    pub connection: Option<SharedString>,
    pub started: Instant,
    pub ended: Option<Instant>,
    /// Rows read or written so far.
    pub rows: Option<u64>,
    pub outcome: Option<TaskOutcome>,
    cancel: Option<CancelFn>,
}

impl TaskEntry {
    pub fn is_running(&self) -> bool {
        self.outcome.is_none()
    }

    pub fn can_cancel(&self) -> bool {
        self.is_running() && self.cancel.is_some()
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.ended
            .unwrap_or(now)
            .saturating_duration_since(self.started)
    }

    /// Whether the task is listed: it ran long enough, or ended in something other than
    /// a success.
    pub fn is_visible(&self, now: Instant) -> bool {
        self.elapsed(now) >= VISIBLE_AFTER
            || matches!(
                self.outcome,
                Some(TaskOutcome::Failed(_) | TaskOutcome::Cancelled)
            )
    }
}

/// The tasks, oldest first. Free of the UI so it can be tested on its own.
#[derive(Default)]
pub struct Registry {
    entries: Vec<TaskEntry>,
    next: u64,
}

impl Registry {
    pub fn start(
        &mut self,
        label: SharedString,
        connection: Option<SharedString>,
        cancel: Option<CancelFn>,
        now: Instant,
    ) -> TaskId {
        self.next += 1;
        let id = TaskId(self.next);
        self.entries.push(TaskEntry {
            id,
            label,
            connection,
            started: now,
            ended: None,
            rows: None,
            outcome: None,
            cancel,
        });
        id
    }

    fn running_mut(&mut self, id: TaskId) -> Option<&mut TaskEntry> {
        self.entries
            .iter_mut()
            .find(|e| e.id == id && e.is_running())
    }

    pub fn progress(&mut self, id: TaskId, rows: u64) -> bool {
        match self.running_mut(id) {
            Some(e) if e.rows != Some(rows) => {
                e.rows = Some(rows);
                true
            }
            _ => false,
        }
    }

    /// Ends a running task; a task already ended keeps its first outcome.
    pub fn finish(&mut self, id: TaskId, outcome: TaskOutcome, now: Instant) -> bool {
        let Some(e) = self.running_mut(id) else {
            return false;
        };
        e.outcome = Some(outcome);
        e.ended = Some(now);
        e.cancel = None;
        self.prune(now);
        true
    }

    /// Forgets quick successes and all but the newest finished tasks.
    fn prune(&mut self, now: Instant) {
        self.entries.retain(|e| e.is_running() || e.is_visible(now));
        let finished = self.entries.iter().filter(|e| !e.is_running()).count();
        let mut extra = finished.saturating_sub(FINISHED_LIMIT);
        self.entries.retain(|e| {
            if extra > 0 && !e.is_running() {
                extra -= 1;
                return false;
            }
            true
        });
    }

    pub fn clear_finished(&mut self) {
        self.entries.retain(TaskEntry::is_running);
    }

    pub fn get(&self, id: TaskId) -> Option<&TaskEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn entries(&self) -> &[TaskEntry] {
        &self.entries
    }

    pub fn any_running(&self) -> bool {
        self.entries.iter().any(TaskEntry::is_running)
    }

    /// Running tasks that are listed.
    pub fn running_count(&self, now: Instant) -> usize {
        self.entries
            .iter()
            .filter(|e| e.is_running() && e.is_visible(now))
            .count()
    }
}

/// The registry as an entity, so views can observe it.
pub struct TaskList {
    pub registry: Registry,
    ticking: bool,
}

struct Tasks(Entity<TaskList>);

impl Global for Tasks {}

pub fn init(cx: &mut App) {
    let list = cx.new(|_| TaskList {
        registry: Registry::default(),
        ticking: false,
    });
    cx.set_global(Tasks(list));
}

/// The task list, to observe or read.
pub fn list(cx: &App) -> Option<Entity<TaskList>> {
    cx.try_global::<Tasks>().map(|t| t.0.clone())
}

impl TaskList {
    /// Tells the watchers once a second while a listed task runs, so clocks tick, and
    /// once more when `id` passes [`VISIBLE_AFTER`], so it shows on time. Work that ends
    /// before then redraws nothing.
    fn tick(&mut self, id: TaskId, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(VISIBLE_AFTER).await;
            this.update(cx, |this, cx| {
                if this.registry.get(id).is_some_and(TaskEntry::is_running) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        if self.ticking {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let go_on = this.update(cx, |this, cx| {
                    if this.registry.running_count(Instant::now()) > 0 {
                        cx.notify();
                    }
                    this.ticking = this.registry.any_running();
                    this.ticking
                });
                if !matches!(go_on, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }
}

/// Registers work that has started. `cancel` stops it the way the panel that started
/// it would; without one the task cannot be cancelled from the list.
pub fn start_task(
    label: impl Into<SharedString>,
    connection: Option<SharedString>,
    cancel: Option<CancelFn>,
    cx: &mut App,
) -> Option<TaskId> {
    let list = list(cx)?;
    let label = label.into();
    Some(list.update(cx, |l, cx| {
        let id = l.registry.start(label, connection, cancel, Instant::now());
        l.tick(id, cx);
        id
    }))
}

/// Rows done so far.
pub fn progress(id: TaskId, rows: u64, cx: &mut App) {
    if let Some(list) = list(cx) {
        list.update(cx, |l, _| {
            l.registry.progress(id, rows);
        });
    }
}

pub fn finish(id: TaskId, outcome: TaskOutcome, cx: &mut App) {
    if let Some(list) = list(cx) {
        list.update(cx, |l, cx| {
            if l.registry.finish(id, outcome, Instant::now()) {
                cx.notify();
            }
        });
    }
}

/// Stops a running task through its cancel path and marks it cancelled.
pub fn cancel(id: TaskId, cx: &mut App) {
    let Some(list) = list(cx) else {
        return;
    };
    let cancel = list
        .read(cx)
        .registry
        .get(id)
        .filter(|e| e.is_running())
        .and_then(|e| e.cancel.clone());
    if let Some(f) = cancel {
        f(cx);
        finish(id, TaskOutcome::Cancelled, cx);
    }
}

pub fn clear_finished(cx: &mut App) {
    if let Some(list) = list(cx) {
        list.update(cx, |l, cx| {
            l.registry.clear_finished();
            cx.notify();
        });
    }
}

/// A short label for a statement: its first line, cut to a readable length.
pub fn statement_label(statement: &str) -> String {
    const MAX: usize = 80;
    let mut lines = statement.lines().map(str::trim).filter(|l| !l.is_empty());
    // A leading comment says less than the statement under it.
    let line = lines
        .clone()
        .find(|l| !l.starts_with("--"))
        .or_else(|| lines.next())
        .unwrap_or("");
    let mut out: String = line.chars().take(MAX).collect();
    if line.chars().count() > MAX || statement.trim().lines().count() > 1 {
        out.push('…');
    }
    out
}

/// A running task's clock, which moves once a second: `3 s`, `3 m 05 s`.
pub fn running_text(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs} s")
    } else {
        elapsed_text(d)
    }
}

/// `0.4 s`, `12 s`, `3 m 05 s`, `1 h 02 m`.
pub fn elapsed_text(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 10 {
        format!("{:.1} s", d.as_secs_f64())
    } else if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} m {:02} s", secs / 60, secs % 60)
    } else {
        format!("{} h {:02} m", secs / 3600, secs % 3600 / 60)
    }
}

/// Follows a grid cache's loading (pages, a count, a stream being read) as one task at a
/// time: a task starts when loading is seen and ends when it stops. The owner keeps the
/// follower; once it is dropped, a task still running ends as cancelled, since the tab
/// that wanted the rows is gone.
#[derive(Default)]
pub struct CacheFollower {
    shared: Rc<Follow>,
}

#[derive(Default)]
struct Follow {
    generation: Cell<u64>,
    task: Cell<Option<TaskId>>,
}

impl CacheFollower {
    /// Watches `cache` from now on, until its loading stops; `label` names the work.
    /// Watching gives up quietly when nothing is asked of the cache (a tab not shown).
    pub fn follow(
        &self,
        cache: Arc<GridCache>,
        label: SharedString,
        connection: Option<SharedString>,
        cancel: Option<CancelFn>,
        cx: &mut App,
    ) {
        let generation = self.shared.generation.get() + 1;
        self.shared.generation.set(generation);
        let shared: Weak<Follow> = Rc::downgrade(&self.shared);
        // A task an older follow started is this one's to end, even if the owner goes
        // before the first look.
        let mut seen: Option<TaskId> = self.shared.task.get();
        cx.spawn(async move |cx| {
            let mut quiet = 0;
            loop {
                cx.background_executor().timer(FOLLOW_EVERY).await;
                let Some(follow) = shared.upgrade() else {
                    // The owner is gone, and with it whoever wanted the rows.
                    if let Some(id) = seen {
                        cx.update(|cx| finish(id, TaskOutcome::Cancelled, cx)).ok();
                    }
                    break;
                };
                if follow.generation.get() != generation {
                    // A newer follow took over, with any task still running.
                    break;
                }
                let status = cache.status();
                // An estimate is not a count of anything done, so it is left out.
                let rows = match cache.row_count() {
                    RowCount::Exact(n) | RowCount::AtLeast(n) => Some(n),
                    RowCount::Estimated(_) | RowCount::Unknown => None,
                };
                let done = cx.update(|cx| match status {
                    Status::Loading => {
                        match follow.task.get() {
                            Some(id) => {
                                if let Some(rows) = rows {
                                    progress(id, rows, cx);
                                }
                            }
                            None => {
                                let id = start_task(
                                    label.clone(),
                                    connection.clone(),
                                    cancel.clone(),
                                    cx,
                                );
                                if let (Some(id), Some(list)) = (id, list(cx)) {
                                    // It was seen a moment after it began.
                                    list.update(cx, |l, _| {
                                        if let Some(e) =
                                            l.registry.entries.iter_mut().find(|e| e.id == id)
                                        {
                                            if let Some(s) = e.started.checked_sub(FOLLOW_EVERY) {
                                                e.started = s;
                                            }
                                            e.rows = rows;
                                        }
                                    });
                                }
                                follow.task.set(id);
                            }
                        }
                        false
                    }
                    Status::Error(e) => {
                        if let Some(id) = follow.task.take() {
                            finish(id, TaskOutcome::Failed(e), cx);
                        }
                        true
                    }
                    Status::Idle | Status::Done => match follow.task.take() {
                        Some(id) => {
                            if let Some(rows) = rows {
                                progress(id, rows, cx);
                            }
                            finish(id, TaskOutcome::Done, cx);
                            true
                        }
                        None => {
                            quiet += 1;
                            quiet >= 3
                        }
                    },
                });
                seen = follow.task.get();
                drop(follow);
                if !matches!(done, Ok(false)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Ends the task being followed as cancelled, after the owner stopped the work.
    pub fn cancelled(&self, cx: &mut App) {
        self.shared.generation.set(self.shared.generation.get() + 1);
        if let Some(id) = self.shared.task.take() {
            finish(id, TaskOutcome::Cancelled, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn later(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn quick_successes_are_not_kept() {
        let mut r = Registry::default();
        let t0 = Instant::now();
        let quick = r.start("quick".into(), None, None, t0);
        let slow = r.start("slow".into(), None, None, t0);
        let failed = r.start("failed".into(), None, None, t0);
        assert_eq!(r.running_count(later(t0, 100)), 0, "not shown yet");
        assert_eq!(r.running_count(later(t0, 500)), 3);
        assert!(r.finish(quick, TaskOutcome::Done, later(t0, 50)));
        assert!(r.finish(failed, TaskOutcome::Failed("no".into()), later(t0, 60)));
        assert!(r.get(quick).is_none());
        assert!(r.get(failed).is_some(), "a failure is kept, however quick");
        assert!(r.progress(slow, 10));
        assert!(!r.progress(slow, 10), "no change");
        assert!(r.finish(slow, TaskOutcome::Done, later(t0, 900)));
        assert!(!r.finish(slow, TaskOutcome::Cancelled, later(t0, 950)));
        let e = r.get(slow).unwrap();
        assert_eq!(e.outcome, Some(TaskOutcome::Done));
        assert_eq!(e.rows, Some(10));
        assert_eq!(e.elapsed(later(t0, 5000)), Duration::from_millis(900));
        assert!(!r.any_running());
        r.clear_finished();
        assert!(r.entries().is_empty());
    }

    #[test]
    fn the_finished_list_is_bounded() {
        let mut r = Registry::default();
        let t0 = Instant::now();
        let running = r.start("still going".into(), None, None, t0);
        for n in 0..250 {
            let id = r.start(format!("task {n}").into(), None, None, t0);
            r.finish(id, TaskOutcome::Cancelled, later(t0, 1));
        }
        assert_eq!(r.entries().len(), FINISHED_LIMIT + 1);
        assert!(r.get(running).is_some_and(TaskEntry::is_running));
        assert_eq!(
            r.entries().last().map(|e| e.label.as_ref()),
            Some("task 249")
        );
    }

    #[test]
    fn labels_and_times_read_well() {
        assert_eq!(statement_label("  \n SELECT 1\nFROM t"), "SELECT 1…");
        assert_eq!(statement_label("SELECT 1;"), "SELECT 1;");
        assert_eq!(statement_label("-- totals\nSELECT 2"), "SELECT 2…");
        assert_eq!(statement_label("-- only a note"), "-- only a note");
        assert_eq!(statement_label(&"x".repeat(100)).chars().count(), 81);
        assert_eq!(elapsed_text(Duration::from_millis(400)), "0.4 s");
        assert_eq!(elapsed_text(Duration::from_secs(42)), "42 s");
        assert_eq!(elapsed_text(Duration::from_secs(185)), "3 m 05 s");
        assert_eq!(elapsed_text(Duration::from_secs(3720)), "1 h 02 m");
        assert_eq!(running_text(Duration::from_millis(2900)), "2 s");
        assert_eq!(running_text(Duration::from_secs(185)), "3 m 05 s");
    }
}
