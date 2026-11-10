//! Seamless updates. With the default settings Deltos looks for a new version at start
//! and every six hours, downloads it in the background, shows a small "Restart to
//! update" pill in the title bar, and otherwise installs it as the app quits. Each step
//! can be turned off in Settings > Updates. All the checking, signature verification and
//! installing is in `deltos-updater`; this is the app's side of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use deltos_updater::{Applied, Channel, Downloaded, Update, Updater, UpdaterConfig};
use gpui::{App, AppContext, Context, Entity, Global, Task};

use crate::net::Net;
use crate::state::{AppState, StateEvent};

/// How often to look for a new version while the app runs.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    /// Not checked yet this run.
    Idle,
    /// This build cannot update itself, and why.
    Disabled(String),
    Checking,
    UpToDate,
    Available(String),
    Downloading {
        version: String,
        done: u64,
        total: u64,
    },
    /// Downloaded and verified, waiting for a restart or for the app to quit.
    Ready(String),
    Installing(String),
    Failed(String),
}

pub struct UpdateModel {
    state: Entity<AppState>,
    updater: Arc<Updater>,
    pub status: UpdateStatus,
    pub last_check: Option<SystemTime>,
    pending: Option<Update>,
    downloaded: Option<Downloaded>,
    _schedule: Option<Task<()>>,
    _job: Option<Task<()>>,
}

/// The update model, for the title bar and Settings to reach.
pub struct Updates(pub Entity<UpdateModel>);

impl Global for Updates {}

impl UpdateModel {
    pub fn init(state: Entity<AppState>, cx: &mut App) -> Entity<Self> {
        let updater = Arc::new(Updater::new(UpdaterConfig {
            repo: "khaodius/deltos".into(),
            current_version: CURRENT.into(),
            public_key: deltos_updater::EMBEDDED_PUBLIC_KEY.into(),
            app_name: "deltos".into(),
            channel: Channel::Stable,
        }));
        let entity = cx.new(|cx| {
            let status = match updater.disabled_reason() {
                Some(why) => UpdateStatus::Disabled(why.to_owned()),
                None => UpdateStatus::Idle,
            };
            let mut this = Self {
                state: state.clone(),
                updater: updater.clone(),
                status,
                last_check: None,
                pending: None,
                downloaded: None,
                _schedule: None,
                _job: None,
            };
            if updater.is_enabled() {
                // Leftovers of the previous update go first, off the UI thread.
                let cleaner = updater.clone();
                cx.global::<Net>().spawn(async move {
                    let _ = tokio::task::spawn_blocking(move || cleaner.clean_up()).await;
                });
                if !state.read(cx).demo {
                    this.schedule(cx);
                }
            }
            this
        });
        cx.set_global(Updates(entity.clone()));
        entity
    }

    /// Checks at start and every six hours while automatic checks are on.
    fn schedule(&mut self, cx: &mut Context<Self>) {
        self._schedule = Some(cx.spawn(async move |this, cx| {
            loop {
                let alive = this
                    .update(cx, |this, cx| {
                        if this.state.read(cx).settings.updates.auto_check {
                            this.check(cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(CHECK_EVERY).await;
            }
        }));
    }

    pub fn check(&mut self, cx: &mut Context<Self>) {
        if !self.updater.is_enabled()
            || matches!(
                self.status,
                UpdateStatus::Checking
                    | UpdateStatus::Downloading { .. }
                    | UpdateStatus::Ready(_)
                    | UpdateStatus::Installing(_)
            )
        {
            return;
        }
        self.set(UpdateStatus::Checking, cx);
        let updater = self.updater.clone();
        let job = cx.global::<Net>().run(async move { updater.check().await });
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.last_check = Some(SystemTime::now());
                match result {
                    Some(Ok(Some(update))) => {
                        let version = update.version.clone();
                        this.pending = Some(update);
                        this.set(UpdateStatus::Available(version), cx);
                        if this.state.read(cx).settings.updates.auto_download {
                            this.download(cx);
                        }
                    }
                    Some(Ok(None)) => this.set(UpdateStatus::UpToDate, cx),
                    Some(Err(deltos_updater::Error::Disabled(why))) => {
                        this.set(UpdateStatus::Disabled(why), cx)
                    }
                    Some(Err(e)) => {
                        tracing::info!("could not check for updates: {e}");
                        this.set(UpdateStatus::Failed(e.to_string()), cx);
                    }
                    None => this.set(UpdateStatus::Failed("the check stopped".into()), cx),
                }
            })
            .ok();
        }));
    }

    pub fn download(&mut self, cx: &mut Context<Self>) {
        let Some(update) = self.pending.clone() else {
            return;
        };
        let version = update.version.clone();
        let total = update.asset.size;
        self.set(
            UpdateStatus::Downloading {
                version: version.clone(),
                done: 0,
                total,
            },
            cx,
        );
        let progress = Arc::new(AtomicU64::new(0));
        let report = progress.clone();
        let updater = self.updater.clone();
        let job = cx.global::<Net>().run(async move {
            updater
                .download(&update, move |done, _| {
                    report.store(done, Ordering::Relaxed)
                })
                .await
        });
        // The progress is read a few times a second rather than redrawing per chunk.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let going = this
                    .update(cx, |this, cx| match &mut this.status {
                        UpdateStatus::Downloading { done, .. } => {
                            *done = progress.load(Ordering::Relaxed);
                            cx.notify();
                            true
                        }
                        _ => false,
                    })
                    .unwrap_or(false);
                if !going {
                    break;
                }
            }
        })
        .detach();
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| match result {
                Some(Ok(downloaded)) => {
                    this.downloaded = Some(downloaded);
                    this.set(UpdateStatus::Ready(version), cx);
                }
                Some(Err(e)) => {
                    tracing::warn!("update download failed: {e}");
                    this.set(UpdateStatus::Failed(e.to_string()), cx);
                }
                None => this.set(UpdateStatus::Failed("the download stopped".into()), cx),
            })
            .ok();
        }));
    }

    /// Installs the downloaded version now and restarts into it.
    pub fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let Some(downloaded) = self.downloaded.take() else {
            return;
        };
        let version = downloaded.version.clone();
        self.set(UpdateStatus::Installing(version), cx);
        let updater = self.updater.clone();
        let job = cx
            .global::<Net>()
            .run(async move { updater.install(downloaded).await });
        let state = self.state.clone();
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| match result {
                Some(Ok(Applied::Restart(exe))) => {
                    state.update(cx, |s, _| s.save_now());
                    match std::process::Command::new(&exe).spawn() {
                        Ok(_) => cx.quit(),
                        Err(e) => this.set(
                            UpdateStatus::Failed(format!(
                                "Updated, but Deltos could not restart ({e}). Start it again yourself."
                            )),
                            cx,
                        ),
                    }
                }
                Some(Ok(Applied::Quit)) => {
                    state.update(cx, |s, _| s.save_now());
                    cx.quit();
                }
                Some(Err(e)) => this.set(UpdateStatus::Failed(e.to_string()), cx),
                None => this.set(UpdateStatus::Failed("the update did not install".into()), cx),
            })
            .ok();
        }));
    }

    /// Installs a downloaded update as the app quits, when the setting allows it.
    pub fn install_on_quit(&mut self, cx: &App) {
        if !self.state.read(cx).settings.updates.install_on_quit {
            return;
        }
        if let Some(downloaded) = self.downloaded.take()
            && let Err(e) = self.updater.install_on_exit(downloaded)
        {
            tracing::warn!("could not install the update on quit: {e}");
        }
    }

    fn set(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
            self.state.update(cx, |_, cx| cx.emit(StateEvent::Update));
        }
    }
}
