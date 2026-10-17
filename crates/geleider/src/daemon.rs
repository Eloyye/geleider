//! `geleider daemon`: the background process that owns the queue, starts Runs
//! when their Resource has a free slot, and reports their results.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{Config, ConfigCache};
use crate::git;
use crate::github::{self, Gh, GitHub, Posted};
use crate::lease::{self, Lease, QueuedRun, RunningRun};
use crate::paths::{self, Paths};
use crate::procs::{self, Snapshot};
use crate::protocol::{self, Reply, Request, ResourceStatus, RunInfo, Status};
use crate::report::{self, MacNotifier, Notifier, TAIL_LINES};
use crate::session::{Activity, Harness, Session};
use crate::store::{NewRun, Run, RunState, Store, now_ms};
use crate::usage::{self, Reading, ResetCheck, Subscription, SubscriptionUsage, Window};

/// How often the background process reads Codex's session logs and looks for
/// Limit windows that have reset.
const USAGE_INTERVAL: Duration = Duration::from_secs(30);
/// The longest request accepted over the socket. Real ones are well under 1 KiB.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

/// Locks `mutex` even if a thread panicked while holding it. The state behind
/// geleider's locks lives in SQLite or is rebuilt from the process table, so one
/// failed thread must not take down every later request.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn run(paths: Paths) -> Result<()> {
    paths.ensure_state_dir()?;
    let lock = File::create(paths.lock_file())?;
    if lock.try_lock().is_err() {
        bail!("another geleider background process is already running");
    }

    // Holding the lock means any socket left behind is stale.
    let socket = paths.socket();
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("listening on {}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;

    let hook_path = paths.hook_socket();
    let _ = std::fs::remove_file(&hook_path);
    let hooks = UnixDatagram::bind(&hook_path)?;
    std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o600))?;

    let daemon = Arc::new(Daemon {
        core: Mutex::new(Core {
            store: Store::open(&paths.database(), paths.run_logs())?,
            config: ConfigCache::new(paths.config.clone()),
            children: HashSet::new(),
        }),
        wake: (Mutex::new(false), Condvar::new()),
        notifier: Box::new(MacNotifier),
        github: Box::new(Gh),
    });
    eprintln!("geleider: listening on {}", paths::display(&socket));

    let ingress = Arc::clone(&daemon);
    std::thread::spawn(move || {
        let mut bytes = [0; 8192];
        while let Ok(size) = hooks.recv(&mut bytes) {
            if let Ok(activity) = serde_json::from_slice::<Activity>(&bytes[..size])
                && let Err(err) = ingress.record_activity(activity)
            {
                eprintln!("geleider: recording Session activity: {err:#}");
            }
        }
    });
    // Independent from the scheduler and PR reporting. Failed deliveries remain
    // in SQLite for retry, including across daemon restarts.
    let delivery = Arc::clone(&daemon);
    std::thread::spawn(move || {
        let mut warned = HashSet::new();
        loop {
            let pending = delivery
                .core
                .lock()
                .unwrap()
                .store
                .pending_results()
                .unwrap_or_default();
            for run in pending.into_iter().filter(|r| {
                r.session
                    .as_ref()
                    .is_some_and(|s| s.harness == Harness::Codex)
            }) {
                match crate::delivery::codex(&run) {
                    Ok(()) => {
                        let result = delivery
                            .core
                            .lock()
                            .unwrap()
                            .store
                            .acknowledge(run.id, run.session.as_ref().expect("filtered"));
                        if let Err(err) = result {
                            eprintln!("geleider: acknowledging result: {err:#}");
                        }
                        warned.remove(&run.id);
                    }
                    Err(err) if warned.insert(run.id) => {
                        eprintln!("geleider: delivering Run #{} (will retry): {err:#}", run.id)
                    }
                    Err(_) => {}
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });

    let scheduler = Arc::clone(&daemon);
    std::thread::spawn(move || scheduler.schedule_loop());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let daemon = Arc::clone(&daemon);
                std::thread::spawn(move || daemon.serve(stream));
            }
            Err(err) => eprintln!("geleider: accept failed: {err}"),
        }
    }
    drop(lock);
    Ok(())
}

struct Daemon {
    core: Mutex<Core>,
    /// Set and signalled to make the scheduler look at the queue right away.
    wake: (Mutex<bool>, Condvar),
    notifier: Box<dyn Notifier>,
    github: Box<dyn GitHub + Send + Sync>,
}

struct Core {
    store: Store,
    config: ConfigCache,
    /// Runs this process started and is waiting on. Other running Runs were
    /// started before a restart and are watched through the process table.
    children: HashSet<i64>,
}

impl Core {
    /// The current config and every Lease held right now.
    fn leases(&mut self, snapshot: &Snapshot) -> Result<(Config, Vec<Lease>)> {
        let config = self.config.get()?.clone();
        let running = running_runs(&self.store)?;
        let held = lease::leases(&config.resources, &running, snapshot);
        Ok((config, held))
    }
}

/// Something that happened to a Run that people should hear about.
enum Event {
    /// A queued Run started because a slot freed up.
    Started(Run),
    Finished(Run),
}

impl Daemon {
    fn record_activity(&self, activity: Activity) -> Result<()> {
        let config = self.core.lock().unwrap().config.get()?.clone();
        let location = git::locate(&config, &activity.cwd).ok();
        let mut session = Session {
            key: activity.key,
            project: location.as_ref().map(|l| l.project_name.to_string()),
            worktree: location
                .as_ref()
                .map(|l| l.worktree.clone())
                .unwrap_or_else(|| {
                    git::worktree(&activity.cwd).unwrap_or_else(|| activity.cwd.clone())
                }),
            branch: location
                .as_ref()
                .and_then(|l| l.branch.clone())
                .or_else(|| git::branch(&activity.cwd)),
            state: activity.state,
            last_activity: activity.at,
            pending_prompts: Vec::new(),
            owner_pid: activity.owner_pid,
            owner_started: activity.owner_pid.and_then(procs::start_time),
        };
        let core = self.core.lock().unwrap();
        if let Some(previous) = core.store.session(&session.key)? {
            if previous.last_activity > activity.at {
                return Ok(());
            }
            if session.owner_pid.is_none() {
                session.owner_pid = previous.owner_pid;
                session.owner_started = previous.owner_started;
            }
            if !activity.reset {
                session.pending_prompts = previous.pending_prompts;
            }
        }
        if let Some(resolved) = activity.resolved {
            session
                .pending_prompts
                .retain(|p| *p != resolved && p != "user");
        }
        if let Some(wait_for) = activity.wait_for
            && !session.pending_prompts.contains(&wait_for)
            && (wait_for != "user" || session.pending_prompts.is_empty())
        {
            if wait_for != "user" {
                session.pending_prompts.retain(|p| p != "user");
            }
            session.pending_prompts.push(wait_for);
        }
        if !session.pending_prompts.is_empty() {
            session.state = crate::session::SessionState::Waiting;
        }
        core.store.update_session(&session)
    }

    fn schedule_loop(self: &Arc<Self>) {
        let mut usage_checked: Option<Instant> = None;
        loop {
            if let Err(err) = self.tick(None) {
                eprintln!("geleider: {err:#}");
            }
            if usage_checked.is_none_or(|t| t.elapsed() >= USAGE_INTERVAL) {
                if let Err(err) = self.refresh_usage() {
                    eprintln!("geleider: reading usage: {err:#}");
                }
                usage_checked = Some(Instant::now());
            }
            let interval = self
                .config()
                .map(|c| c.poll_interval())
                .unwrap_or(Duration::from_secs(2));
            let (flag, signal) = &self.wake;
            let (mut woken, _) = signal
                .wait_timeout_while(lock(flag), interval, |w| !*w)
                .unwrap_or_else(PoisonError::into_inner);
            *woken = false;
        }
    }

    fn wake(&self) {
        let (flag, signal) = &self.wake;
        *lock(flag) = true;
        signal.notify_one();
    }

    /// The current config, re-read if the file changed.
    fn config(&self) -> Result<Config> {
        Ok(lock(&self.core).config.get()?.clone())
    }

    /// Looks at the process table, ends the Leases of Runs whose process is gone,
    /// and starts the queued Runs that now have a slot. Returns whether `submitted`
    /// started.
    fn tick(self: &Arc<Self>, submitted: Option<i64>) -> Result<bool> {
        let snapshot = Snapshot::take()?;
        let mut events = Vec::new();
        let mut submitted_started = false;
        {
            let mut core = lock(&self.core);
            let core = &mut *core;

            // A Harness can crash without sending SessionEnd. Its process identity
            // also prevents persisted Sessions being shown as live after a reboot.
            for mut session in core.store.sessions()? {
                if session.state != crate::session::SessionState::Ended
                    && session.owner_pid.is_some_and(|pid| {
                        !snapshot.is_alive(pid, session.owner_started.as_deref())
                    })
                {
                    session.state = crate::session::SessionState::Ended;
                    session.pending_prompts.clear();
                    core.store.update_session(&session)?;
                }
            }

            // Runs left running by an earlier background process: their Lease ends
            // when their process is gone, but their exit code is unknown.
            for run in core.store.with_state(RunState::Running)? {
                if core.children.contains(&run.id) {
                    continue;
                }
                let alive = run
                    .pid
                    .is_some_and(|pid| snapshot.is_alive(pid, run.pid_started.as_deref()));
                if !alive {
                    core.store.mark_ended(run.id, RunState::Lost, None, None)?;
                    events.push(Event::Finished(core.store.require(run.id)?));
                }
            }

            let (config, held) = core.leases(&snapshot)?;
            let queued = queued_runs(&core.store)?;
            for id in lease::runs_to_start(&config.resources, &held, &queued) {
                let run = core.store.require(id)?;
                match self.start(core, &run) {
                    Ok(()) if Some(id) == submitted => submitted_started = true,
                    Ok(()) => events.push(Event::Started(core.store.require(id)?)),
                    Err(err) => {
                        core.store.mark_ended(
                            id,
                            RunState::Failed,
                            None,
                            Some(&format!("{err:#}")),
                        )?;
                        events.push(Event::Finished(core.store.require(id)?));
                    }
                }
            }
        }
        for event in events {
            self.announce(event);
        }
        Ok(submitted_started)
    }

    /// Starts a Run's process, outside any sandbox, in its worktree, with its
    /// output going straight to its log so the log survives a restart.
    fn start(self: &Arc<Self>, core: &mut Core, run: &Run) -> Result<()> {
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&run.log_path)
            .with_context(|| format!("creating {}", run.log_path.display()))?;
        let (program, args) = run.argv.split_first().context("empty command")?;
        let child = Command::new(program)
            .args(args)
            .current_dir(&run.worktree)
            .env("GELEIDER_RUN_ID", run.id.to_string())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Its own process group, so it outlives a restart of this process.
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting {program}"))?;
        let pid = child.id();
        core.store
            .mark_running(run.id, pid, procs::start_time(pid).as_deref())?;
        core.children.insert(run.id);

        let daemon = Arc::clone(self);
        let id = run.id;
        std::thread::spawn(move || daemon.wait_for(id, child));
        Ok(())
    }

    fn wait_for(self: &Arc<Self>, id: i64, mut child: Child) {
        let (state, code, error) = match child.wait() {
            Ok(status) => {
                // A Run killed by a signal reports 128 + the signal, like a shell.
                let code = status.code().or_else(|| status.signal().map(|s| 128 + s));
                let state = if status.success() {
                    RunState::Succeeded
                } else {
                    RunState::Failed
                };
                (state, code, None)
            }
            Err(err) => (
                RunState::Failed,
                None,
                Some(format!("waiting for the run: {err}")),
            ),
        };
        let finished = {
            let mut core = lock(&self.core);
            core.children.remove(&id);
            core.store
                .mark_ended(id, state, code, error.as_deref())
                .and_then(|()| core.store.require(id))
        };
        // Let the next queued Run take the slot now rather than on the next poll.
        self.wake();
        match finished {
            Ok(run) => self.announce(Event::Finished(run)),
            Err(err) => eprintln!("geleider: recording the end of run #{id}: {err:#}"),
        }
    }

    /// Notifies the user and, for a finished Run, comments on its PR. Runs in its
    /// own thread since `gh` can be slow.
    fn announce(self: &Arc<Self>, event: Event) {
        let config = self.config().ok();
        let Some(config) = config else { return };
        let daemon = Arc::clone(self);
        std::thread::spawn(move || match event {
            Event::Started(run) => {
                if config.notifications {
                    let waited = run.started_at.unwrap_or(run.submitted_at) - run.submitted_at;
                    daemon.notifier.notify(
                        &format!("geleider: {} started", report::title(&run)),
                        &format!(
                            "A {} slot freed up after {} in the queue. Run #{} in {}.",
                            run.resource,
                            report::duration(waited),
                            run.id,
                            run.project
                        ),
                    );
                }
            }
            Event::Finished(run) => {
                if config.notifications {
                    daemon.notifier.notify(
                        &format!(
                            "geleider: {} {}",
                            report::title(&run),
                            report::outcome(&run)
                        ),
                        &format!(
                            "Run #{} in {} took {}.",
                            run.id,
                            run.project,
                            report::duration(run.duration_ms().unwrap_or(0))
                        ),
                    );
                }
                if config.pr_comments {
                    let body =
                        report::comment_body(&run, &report::log_tail(&run.log_path, TAIL_LINES));
                    match github::post_result(
                        daemon.github.as_ref(),
                        &run.repo,
                        run.branch.as_deref(),
                        &run.job,
                        &body,
                    ) {
                        Ok(Posted::Created(pr) | Posted::Edited(pr)) => {
                            eprintln!(
                                "geleider: run #{} result posted on {}#{pr}",
                                run.id, run.repo
                            )
                        }
                        Ok(Posted::NoBranch | Posted::NoPr) => {}
                        Err(err) => eprintln!("geleider: posting run #{} result: {err:#}", run.id),
                    }
                }
            }
        });
    }

    fn serve(self: &Arc<Self>, stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut line = String::new();
        let reply = match BufReader::new((&stream).take(MAX_REQUEST_BYTES)).read_line(&mut line) {
            Ok(_) => match serde_json::from_str::<Request>(&line) {
                Ok(request) => self.handle(request),
                Err(err) => Err(anyhow::anyhow!("bad request: {err}")),
            },
            Err(err) => Err(anyhow::anyhow!("reading request: {err}")),
        };
        let mut out = protocol::encode_reply(reply);
        out.push('\n');
        let _ = (&stream).write_all(out.as_bytes());
    }

    fn handle(self: &Arc<Self>, request: Request) -> Result<Reply> {
        match request {
            Request::Submit {
                cwd,
                job,
                args,
                submitter,
                session,
            } => self.submit(&cwd, &job, args, submitter, session),
            Request::GetRun { id } => {
                let core = lock(&self.core);
                let run = core.store.require(id)?;
                let queued = queued_runs(&core.store)?;
                Ok(Reply::Run(Box::new(info(run, &queued, true))))
            }
            Request::ListRuns { finished } => {
                let core = lock(&self.core);
                let queued = queued_runs(&core.store)?;
                let runs = core.store.list(finished)?;
                Ok(Reply::Runs {
                    runs: runs.into_iter().map(|r| info(r, &queued, false)).collect(),
                })
            }
            Request::SessionResults { session, ack } => {
                let core = self.core.lock().unwrap();
                if let Some(id) = ack {
                    core.store.acknowledge(id, &session)?;
                }
                let queued = queued_runs(&core.store)?;
                Ok(Reply::Runs {
                    runs: core
                        .store
                        .pending_results()?
                        .into_iter()
                        .filter(|r| r.session.as_ref() == Some(&session))
                        // One bounded log tail per mailbox reply, even after a long outage.
                        .take(1)
                        .map(|r| info(r, &queued, false))
                        .collect(),
                })
            }
            Request::Status => {
                let snapshot = Snapshot::take()?;
                let mut core = lock(&self.core);
                let (config, held) = core.leases(&snapshot)?;
                let resources = config
                    .resources
                    .iter()
                    .map(|(name, r)| ResourceStatus {
                        name: name.clone(),
                        slots: r.slots,
                        process: r.process.clone(),
                        holders: held
                            .iter()
                            .filter(|l| l.resource == *name)
                            .map(|l| l.holder.clone())
                            .collect(),
                    })
                    .collect();
                let queued = queued_runs(&core.store)?;
                let runs = core.store.list(0)?;
                let sessions = core.store.sessions()?;
                drop(core);
                Ok(Reply::Status(Status {
                    sessions,
                    resources,
                    runs: runs.into_iter().map(|r| info(r, &queued, false)).collect(),
                    subscriptions: self.usage()?,
                }))
            }
            Request::ReportUsage {
                subscription,
                readings,
            } => {
                self.record_usage(subscription, &readings, now_ms())?;
                Ok(Reply::Usage {
                    subscriptions: Vec::new(),
                })
            }
            Request::Usage => Ok(Reply::Usage {
                subscriptions: self.usage()?,
            }),
        }
    }

    /// Reads the newest Codex numbers, so what is shown is never older than its
    /// session logs, then reports every enabled Subscription.
    fn usage(self: &Arc<Self>) -> Result<Vec<SubscriptionUsage>> {
        self.refresh_usage()?;
        let mut core = lock(&self.core);
        let config = core.config.get()?.clone();
        let windows = core.store.windows()?;
        let now = now_ms();
        Ok(Subscription::ALL
            .into_iter()
            .filter(|s| config.subscriptions.get(*s).enabled)
            .map(|s| SubscriptionUsage {
                subscription: s,
                plan: config.subscriptions.plan(s),
                source: s.source().to_string(),
                windows: windows
                    .iter()
                    .filter(|w| w.subscription == s)
                    .map(|w| usage::view(w, now))
                    .collect(),
            })
            .collect())
    }

    /// Records the newest Codex numbers and notifies about windows that have reset.
    fn refresh_usage(self: &Arc<Self>) -> Result<()> {
        let config = self.config()?;
        // Reading the logs happens without holding the lock.
        let latest = if config.subscriptions.chatgpt.enabled {
            usage::latest_codex(&usage::codex_sessions_dir()?)
        } else {
            None
        };
        // With nothing new this still notifies about windows that have reset.
        let (readings, observed_at) = latest.unwrap_or_else(|| (Vec::new(), now_ms()));
        self.record_usage(Subscription::Chatgpt, &readings, observed_at)
    }

    /// Folds readings into the stored windows. Resets that are due are handled
    /// first, so a reading for the next window cannot hide the end of this one.
    fn record_usage(
        self: &Arc<Self>,
        subscription: Subscription,
        readings: &[Reading],
        observed_at: i64,
    ) -> Result<()> {
        let now = now_ms();
        // A clock set wrong, or a crafted log line, must not date a reading in the
        // future: later readings would then look older and be dropped.
        let observed_at = observed_at.min(now);
        let (config, resets) = {
            let mut core = lock(&self.core);
            let config = core.config.get()?.clone();
            let mut resets = Vec::new();
            for window in core.store.windows()? {
                match usage::check_reset(&window, now, config.subscriptions.notify_resets) {
                    ResetCheck::Pending => continue,
                    ResetCheck::Notify => resets.push(window.clone()),
                    ResetCheck::Quiet => {}
                }
                core.store.put_window(&Window {
                    reset_handled: true,
                    ..window
                })?;
            }
            if config.subscriptions.get(subscription).enabled {
                let stored = core.store.windows()?;
                for reading in readings {
                    let old = stored
                        .iter()
                        .find(|w| w.subscription == subscription && w.minutes == reading.minutes);
                    if let Some(window) = usage::merge(old, subscription, *reading, observed_at) {
                        core.store.put_window(&window)?;
                    }
                }
            }
            (config, resets)
        };
        if config.notifications && !resets.is_empty() {
            let daemon = Arc::clone(self);
            std::thread::spawn(move || {
                for window in resets {
                    let plan = config.subscriptions.plan(window.subscription);
                    daemon.notifier.notify(
                        &format!(
                            "geleider: {plan} {} limit reset",
                            usage::long_label(window.minutes)
                        ),
                        &format!(
                            "It had reached {:.0}% used. The new window starts empty.",
                            window.used_percent
                        ),
                    );
                }
            });
        }
        Ok(())
    }

    fn submit(
        self: &Arc<Self>,
        cwd: &std::path::Path,
        job: &str,
        args: Vec<String>,
        submitter: String,
        session: Option<crate::session::SessionKey>,
    ) -> Result<Reply> {
        // Resolving the worktree runs git, so it happens without holding the lock.
        let config = self.config()?;
        let location = git::locate(&config, cwd)?;
        let rendered = location.project.render(job, &args, &location.worktree)?;
        let run = lock(&self.core).store.insert(NewRun {
            project: location.project_name.to_string(),
            repo: location.project.repo.clone(),
            job: job.to_string(),
            args,
            resource: rendered.resource,
            worktree: location.worktree,
            branch: location.branch,
            argv: rendered.argv,
            submitter,
            session,
        })?;
        self.tick(Some(run.id))?;

        let core = lock(&self.core);
        let run = core.store.require(run.id)?;
        let queued = queued_runs(&core.store)?;
        Ok(Reply::Run(Box::new(info(run, &queued, false))))
    }
}

fn running_runs(store: &Store) -> Result<Vec<RunningRun>> {
    Ok(store
        .with_state(RunState::Running)?
        .into_iter()
        .map(|r| RunningRun {
            id: r.id,
            resource: r.resource,
            pid: r.pid,
        })
        .collect())
}

fn queued_runs(store: &Store) -> Result<Vec<QueuedRun>> {
    Ok(store
        .with_state(RunState::Queued)?
        .into_iter()
        .map(|r| QueuedRun {
            id: r.id,
            resource: r.resource,
        })
        .collect())
}

fn info(run: Run, queued: &[QueuedRun], with_tail: bool) -> RunInfo {
    let queue_position = lease::queue_position(queued, run.id);
    let log_tail = (with_tail && run.started_at.is_some())
        .then(|| report::log_tail(&run.log_path, TAIL_LINES));
    RunInfo {
        run,
        queue_position,
        log_tail,
    }
}
