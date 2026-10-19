//! `geleider daemon`: the background process that owns the queue, starts Runs
//! when their Resource has a free slot, reports their results, and keeps Claims.
#![allow(
    clippy::disallowed_macros,
    reason = "the background process logs to stderr, which launchd keeps in a file"
)]

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

mod claims;
mod dispatches;

use crate::attest::{Attestations, Destination};
use crate::claim::{self, ClaimStatus};
use crate::config::{Config, ConfigCache};
use crate::delivery::{self, Retries};
use crate::git;
use crate::github::{self, CachedPullRequests, Gh, GitHub, Posted, PullRequests};
use crate::hook::{Datagram, MAX_DATAGRAM_BYTES};
use crate::lease::{self, Lease, QueuedRun, RunningRun};
use crate::paths::{self, Paths};
use crate::procs::{self, Snapshot};
use crate::protocol::{self, Reply, Request, ResourceStatus, RunInfo, Status};
use crate::report::{self, MacNotifier, Notifier, TAIL_LINES};
use crate::session::{Activity, Harness, Session, SessionState};
use crate::store::{NewRun, Run, RunState, Store, now_ms};
use crate::text;
use crate::usage::{self, Reading, ResetCheck, Subscription, SubscriptionUsage, Window};
use crate::{ci, cleanup};

/// How often the background process reads Codex's session logs and looks for
/// Limit windows that have reset.
const USAGE_INTERVAL: Duration = Duration::from_secs(30);
/// The longest request accepted over the socket. Real ones are well under 1 KiB.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
/// How often finished Runs are offered to the Codex threads that submitted them.
const DELIVERY_INTERVAL: Duration = Duration::from_secs(2);
/// How long an ended Session stays in `status`, and how often older ones are forgotten.
const SESSION_HISTORY: Duration = Duration::from_secs(7 * 24 * 3600);
const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);
/// How long a hook's attestation waits for its submission. The Harness may ask
/// the user to allow the call first. `GELEIDER_ATTESTATION_TTL_MS` overrides it
/// for tests.
const ATTESTATION_TTL: Duration = Duration::from_secs(10 * 60);
/// How long a submission waits for the attestation sent just before it.
const ATTESTATION_WAIT: Duration = Duration::from_millis(500);
/// Activity datagrams waiting to be recorded; more are dropped, like a full socket.
const ACTIVITY_BACKLOG: usize = 256;
/// How long `status` waits for the Finished-worktree scan it starts. A slower
/// scan finishes in the background and the reply shows the one before it.
const STATUS_SCAN_WAIT: Duration = Duration::from_secs(2);

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
            claiming: HashSet::new(),
        }),
        wake: (Mutex::new(false), Condvar::new()),
        attestations: Attestations::new(attestation_ttl()),
        notifier: Box::new(MacNotifier),
        github: Box::new(Gh),
        prs: CachedPullRequests::default(),
        scan: Scan::default(),
        claims_file: paths.claims(),
        claim_quiet: env_ms("GELEIDER_CLAIM_QUIET_MS").unwrap_or(claim::QUIET_AFTER),
    });
    daemon.publish_claims(&self::lock(&daemon.core).store);
    eprintln!("geleider: listening on {}", paths::display(&socket));

    let ingress = Arc::clone(&daemon);
    std::thread::spawn(move || ingress.ingress_loop(&hooks));
    let delivery = Arc::clone(&daemon);
    std::thread::spawn(move || delivery.delivery_loop());
    let scheduler = Arc::clone(&daemon);
    std::thread::spawn(move || scheduler.schedule_loop());
    let ci = Arc::clone(&daemon);
    std::thread::spawn(move || ci.ci_loop());
    let claims = Arc::clone(&daemon);
    std::thread::spawn(move || claims.claims_loop());

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
    /// Session destinations hooks vouched for, waiting for their submissions.
    attestations: Attestations,
    notifier: Box<dyn Notifier>,
    github: Box<dyn GitHub + Send + Sync>,
    prs: CachedPullRequests,
    scan: Scan,
    /// Where start hooks read every Claim.
    claims_file: PathBuf,
    /// How long a holder may be quiet before its Claim is flagged.
    claim_quiet: Duration,
}

struct Core {
    store: Store,
    config: ConfigCache,
    /// Runs this process started and is waiting on. Other running Runs were
    /// started before a restart and are watched through the process table.
    children: HashSet<i64>,
    /// Issues being claimed on GitHub right now, so two Sessions can't both.
    claiming: HashSet<(String, u64)>,
}

impl Core {
    /// The current config and every Lease held right now.
    fn leases(&mut self, snapshot: &Snapshot) -> Result<(Config, Vec<Lease>)> {
        let config = self.config.get()?.clone();
        let running = running_runs(&self.store)?;
        let held = lease::leases(&config.resources, &running, snapshot);
        Ok((config, held))
    }

    /// The current config with every Session and Run: what decides whether a
    /// worktree is still in use.
    fn worktree_users(&mut self) -> Result<(Config, Vec<Session>, Vec<Run>)> {
        Ok((
            self.config.get()?.clone(),
            self.store.sessions()?,
            self.store.list(0)?,
        ))
    }
}

/// The latest Finished-worktree scan for `status`, which runs `git` in every
/// worktree and may wait on GitHub, so it never holds up the reply.
#[derive(Default)]
struct Scan {
    state: Mutex<ScanState>,
    done: Condvar,
}

#[derive(Default)]
struct ScanState {
    latest: Option<cleanup::Inventory>,
    running: bool,
}

/// Something that happened to a Run that people should hear about.
enum Event {
    /// A queued Run started because a slot freed up.
    Started(Run),
    Finished(Run),
}

impl Daemon {
    fn ci_loop(&self) {
        let mut poller = ci::Poller::default();
        loop {
            if let Err(err) = self.poll_ci(&mut poller) {
                eprintln!("geleider: checking CI: {err:#}");
            }
            std::thread::sleep(ci::TICK);
        }
    }

    fn poll_ci(&self, poller: &mut ci::Poller) -> Result<()> {
        let (config, sessions) = {
            let mut core = lock(&self.core);
            (core.config.get()?.clone(), core.store.sessions()?)
        };
        if !config.notifications {
            return Ok(());
        }
        let Some((repo, branch)) = poller.next(&config, &sessions, Instant::now(), ci::INTERVAL)
        else {
            return Ok(());
        };
        // Network calls never hold the queue/Session lock.
        for pr in self.prs.pull_requests(&repo, &branch)? {
            for failure in ci::failures(&pr) {
                let new = lock(&self.core)
                    .store
                    .new_ci_failure(&repo, pr.number, &failure.id)?;
                if new {
                    // Branch and check names are chosen by Sessions and workflows.
                    self.notifier.notify(
                        &format!("geleider: CI failed on {repo} #{}", pr.number),
                        &format!(
                            "{}: {}\n{}",
                            text::one_line(&branch),
                            text::one_line(&failure.name),
                            pr.url
                        ),
                    );
                }
            }
        }
        Ok(())
    }

    /// Finished worktrees, rechecked against Sessions and Runs recorded while
    /// GitHub was being asked. `fresh` skips the PR cache, as removal requires.
    fn finished(&self, fresh: bool) -> Result<cleanup::Inventory> {
        let (config, sessions, runs) = lock(&self.core).worktree_users()?;
        let github: &dyn PullRequests = if fresh { &Gh } else { &self.prs };
        let mut inventory = cleanup::discover(&config, &sessions, &runs, github);
        let (config, sessions, runs) = lock(&self.core).worktree_users()?;
        inventory
            .finished_worktrees
            .retain(|wt| cleanup::validate(&config, &sessions, &runs, wt).is_ok());
        Ok(inventory)
    }

    /// Starts a cached scan unless one is running, then returns the newest
    /// complete scan within [`STATUS_SCAN_WAIT`].
    fn recent_finished(self: &Arc<Self>) -> cleanup::Inventory {
        let mut state = lock(&self.scan.state);
        if !state.running {
            state.running = true;
            let daemon = Arc::clone(self);
            std::thread::spawn(move || {
                let inventory = daemon
                    .finished(false)
                    .unwrap_or_else(|err| cleanup::Inventory {
                        cleanup_warnings: vec![format!("worktrees: {err:#}")],
                        ..Default::default()
                    });
                let mut state = lock(&daemon.scan.state);
                state.latest = Some(inventory);
                state.running = false;
                daemon.scan.done.notify_all();
            });
        }
        let (state, _) = self
            .scan
            .done
            .wait_timeout_while(state, STATUS_SCAN_WAIT, |s| s.running)
            .unwrap_or_else(PoisonError::into_inner);
        state.latest.clone().unwrap_or_else(|| cleanup::Inventory {
            cleanup_pending: true,
            ..Default::default()
        })
    }

    /// Removes the worktrees the user confirmed that are still Finished now.
    fn clean(&self, worktrees: Vec<cleanup::FinishedWorktree>, cwd: &Path) -> Result<Reply> {
        // Refresh GitHub after the user confirms; no cached PR state authorizes removal.
        let current = self.finished(true)?;
        let mut cleaned = cleanup::Cleaned::default();
        for approved in worktrees {
            let path = approved.worktree.display().to_string();
            if !current.finished_worktrees.contains(&approved) {
                cleaned.skipped.push(format!(
                    "{path}: no longer a Finished worktree (or PR state unavailable)"
                ));
                continue;
            }
            // Holding the lock serializes the final check and removal with
            // incoming Session and Run records; no GitHub call happens under it.
            let mut core = lock(&self.core);
            let (config, sessions, runs) = core.worktree_users()?;
            match cleanup::remove(&config, &sessions, &runs, &approved, cwd) {
                Ok(()) => cleaned.removed.push(approved.worktree),
                Err(err) => cleaned.skipped.push(format!("{path}: {err:#}")),
            }
        }
        Ok(Reply::Cleaned(cleaned))
    }

    /// Records hook datagrams. Any process of this user can write to the socket,
    /// so each datagram is validated again here, not only by the hook.
    /// Attestations are taken at once; activity, which runs git, is recorded in
    /// order by another thread, so a submission never waits behind it.
    fn ingress_loop(self: &Arc<Self>, hooks: &UnixDatagram) {
        let (activities, backlog) = mpsc::sync_channel(ACTIVITY_BACKLOG);
        let recorder = Arc::clone(self);
        std::thread::spawn(move || {
            for activity in backlog {
                if let Err(err) = recorder.record_activity(activity) {
                    eprintln!("geleider: recording Session activity: {err:#}");
                }
            }
        });
        let mut bytes = [0; MAX_DATAGRAM_BYTES];
        while let Ok(size) = hooks.recv(&mut bytes) {
            match serde_json::from_slice::<Datagram>(&bytes[..size]) {
                Ok(Datagram::Attestation(attestation)) => self.attestations.record(attestation),
                Ok(Datagram::Activity(activity)) => {
                    let _ = activities.try_send(activity);
                }
                Err(_) => continue,
            }
        }
    }

    fn record_activity(self: &Arc<Self>, activity: Activity) -> Result<()> {
        // Dropped like a malformed datagram: logging each would let a flood fill the log.
        if !activity.cwd.is_absolute() {
            return Ok(());
        }
        // Locating the worktree runs git, so it happens without holding the lock.
        let config = self.config()?;
        let next = locate_session(&config, &activity);
        let core = lock(&self.core);
        let previous = core.store.session(&activity.key)?;
        let was = previous.as_ref().map(|s| s.state);
        let Some(session) = Session::after(previous, next, &activity) else {
            return Ok(());
        };
        core.store.update_session(&session)?;
        // Only Dispatched Sessions notify; every other Session stays quiet.
        let notice = self.dispatch_notice(&core.store, &session, was)?;
        drop(core);
        if let Some(notice) = notice
            && config.notifications
        {
            self.announce_dispatch(notice);
        }
        Ok(())
    }

    /// Offers finished Runs to the Codex threads that submitted them, until one
    /// acknowledges by succeeding. Undelivered Runs stay in SQLite, so they are
    /// retried after a restart too. OpenCode polls instead (`session-results`).
    fn delivery_loop(&self) {
        let mut retries = Retries::default();
        loop {
            let pending = lock(&self.core).store.pending_results(Harness::Codex);
            match pending {
                Ok(runs) => {
                    for run in runs {
                        self.deliver(&run, &mut retries);
                    }
                }
                Err(err) => eprintln!("geleider: reading undelivered results: {err:#}"),
            }
            std::thread::sleep(DELIVERY_INTERVAL);
        }
    }

    fn deliver(&self, run: &Run, retries: &mut Retries) {
        let Some(session) = &run.session else {
            return;
        };
        let now = Instant::now();
        if !retries.is_due(run.id, now) {
            return;
        }
        // `codex queue` runs without holding the lock.
        match delivery::codex(run, session) {
            Ok(()) => {
                retries.succeeded(run.id);
                if let Err(err) = lock(&self.core).store.acknowledge(run.id, session) {
                    eprintln!("geleider: acknowledging Run #{}: {err:#}", run.id);
                }
            }
            Err(err) => {
                if retries.failed(run.id, now) {
                    eprintln!("geleider: delivering Run #{} (will retry): {err:#}", run.id);
                }
            }
        }
    }

    fn schedule_loop(self: &Arc<Self>) {
        let mut usage_checked: Option<Instant> = None;
        let mut pruned: Option<Instant> = None;
        loop {
            if let Err(err) = self.tick(None) {
                eprintln!("geleider: {err:#}");
            }
            if pruned.is_none_or(|t| t.elapsed() >= PRUNE_INTERVAL) {
                let before = now_ms() - SESSION_HISTORY.as_millis() as i64;
                if let Err(err) = lock(&self.core).store.prune_sessions(before) {
                    eprintln!("geleider: forgetting ended Sessions: {err:#}");
                }
                pruned = Some(Instant::now());
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
            for mut session in core.store.live_sessions()? {
                if session
                    .owner_pid
                    .is_some_and(|pid| !snapshot.is_alive(pid, session.owner_started.as_deref()))
                {
                    session.state = SessionState::Ended;
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
            Ok(_) => protocol::decode_request(&line).and_then(|request| self.handle(request)),
            Err(err) => Err(anyhow::anyhow!("reading request: {err}")),
        };
        let mut out = protocol::encode_reply(reply);
        out.push('\n');
        let _ = (&stream).write_all(out.as_bytes());
    }

    fn handle(self: &Arc<Self>, request: Request) -> Result<Reply> {
        match request {
            Request::ReviewActivity { mut activity } => {
                // This request only ever records a Cross-review, which must never
                // be counted as a PR's writing Session.
                activity.review = true;
                self.record_activity(activity)?;
                Ok(Reply::Recorded)
            }
            Request::FinishedWorktrees => Ok(Reply::Worktrees(self.finished(true)?)),
            Request::Clean { worktrees, cwd } => self.clean(worktrees, &cwd),
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
                let core = lock(&self.core);
                if let Some(id) = ack {
                    core.store.acknowledge(id, &session)?;
                }
                let queued = queued_runs(&core.store)?;
                // One bounded log tail per mailbox reply, even after a long outage.
                let next = core.store.next_result(&session)?;
                Ok(Reply::Runs {
                    runs: next.into_iter().map(|r| info(r, &queued, false)).collect(),
                })
            }
            Request::Status => self.status(),
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
            Request::Claim {
                cwd,
                issue,
                session,
            } => self.claim(&cwd, &issue, session),
            Request::Release {
                cwd,
                issue,
                session,
                force,
            } => self.release(&cwd, &issue, session, force),
            Request::DispatchClaim {
                cwd,
                number,
                session,
                worktree,
                branch,
                pull_request,
                profile,
            } => self.dispatch_claim(dispatches::Planned {
                cwd,
                number,
                session,
                worktree,
                branch,
                pull_request,
                profile,
            }),
            Request::DispatchStarted { session, actual } => {
                self.dispatch_started(&session, &actual)?;
                Ok(Reply::Recorded)
            }
            Request::DispatchFailed { session } => self.dispatch_failed(&session),
        }
    }

    /// Sessions, Claims, Resources with their holders, Runs and usage.
    fn status(self: &Arc<Self>) -> Result<Reply> {
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
        let now = now_ms();
        let claims = core
            .store
            .claims()?
            .into_iter()
            .map(|c| {
                let holder = sessions.iter().find(|s| s.key == c.session);
                ClaimStatus::new(c, holder, self.claim_quiet, now)
            })
            .collect();
        drop(core);
        let mut finished = self.recent_finished();
        // The scan may predate a Session or Run that started in a worktree since.
        finished
            .finished_worktrees
            .retain(|wt| !cleanup::busy(&wt.worktree, &sessions, &runs));
        Ok(Reply::Status(Status {
            sessions,
            claims,
            resources,
            runs: runs.into_iter().map(|r| info(r, &queued, false)).collect(),
            subscriptions: self.usage()?,
            finished,
        }))
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
        session: Option<Destination>,
    ) -> Result<Reply> {
        // The result is queued as a prompt in this Session, so only a hook may
        // choose it. Anything else is kept as a Run with no destination.
        let session = session
            .filter(|d| self.attestations.consume(d, ATTESTATION_WAIT))
            .map(|d| d.key);
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
        let mut info = info(run, &queued, false);
        info.delivery = Some(delivery::notice(info.run.session.as_ref()));
        Ok(Reply::Run(Box::new(info)))
    }
}

fn attestation_ttl() -> Duration {
    env_ms("GELEIDER_ATTESTATION_TTL_MS").unwrap_or(ATTESTATION_TTL)
}

/// A duration in milliseconds from the environment, which tests use to shorten waits.
fn env_ms(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map(Duration::from_millis)
}

/// What `activity` alone says about its Session: where it is and what it is doing.
/// A directory outside every configured Project is still shown, by its repository.
fn locate_session(config: &Config, activity: &Activity) -> Session {
    let cwd = &activity.cwd;
    let (project, worktree, branch) = match git::locate(config, cwd) {
        Ok(location) => (
            Some(location.project_name.to_string()),
            location.worktree,
            location.branch,
        ),
        Err(_) => (
            None,
            git::worktree(cwd).unwrap_or_else(|| cwd.clone()),
            git::branch(cwd),
        ),
    };
    Session {
        key: activity.key.clone(),
        project,
        worktree,
        branch,
        state: activity.state,
        last_activity: activity.at,
        pending_prompts: Vec::new(),
        owner_pid: activity.owner_pid,
        owner_started: activity.owner_pid.and_then(procs::start_time),
        provider_id: activity.provider_id.clone(),
        review: activity.review,
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
        delivery: None,
    }
}
