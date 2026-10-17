//! Leases and the queue, as pure functions over a moment's state so they can be
//! tested without processes or a database.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::config::Resource;
use crate::procs::Snapshot;

/// A Run that has started and not ended.
#[derive(Debug, Clone)]
pub struct RunningRun {
    pub id: i64,
    pub resource: String,
    pub pid: Option<u32>,
}

/// A Run waiting for a slot, in submission order.
#[derive(Debug, Clone)]
pub struct QueuedRun {
    pub id: i64,
    pub resource: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Holder {
    Run {
        id: i64,
    },
    /// A process geleider did not start, such as the Unity Editor opened by hand.
    Process {
        pid: u32,
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub resource: String,
    pub holder: Holder,
}

/// Every Lease held right now. A running Run holds one on its Resource. Any other
/// process named like a Resource's watched process holds one too, unless it was
/// started by a Run on that Resource, whose Lease already covers it.
pub fn leases(
    resources: &BTreeMap<String, Resource>,
    running: &[RunningRun],
    snapshot: &Snapshot,
) -> Vec<Lease> {
    let mut leases: Vec<Lease> = running
        .iter()
        .map(|run| Lease {
            resource: run.resource.clone(),
            holder: Holder::Run { id: run.id },
        })
        .collect();

    let mut processes: Vec<_> = snapshot.processes.values().collect();
    processes.sort_by_key(|p| p.pid);
    for (name, resource) in resources {
        for process in &processes {
            if process.name != resource.process {
                continue;
            }
            let started_by_run = running.iter().any(|run| {
                run.resource == *name
                    && run
                        .pid
                        .is_some_and(|pid| snapshot.descends_from(process.pid, pid))
            });
            if !started_by_run {
                leases.push(Lease {
                    resource: name.clone(),
                    holder: Holder::Process {
                        pid: process.pid,
                        name: process.name.clone(),
                    },
                });
            }
        }
    }
    leases
}

/// Which queued Runs to start now: the oldest ones on each Resource, as many as it
/// has free slots. Runs on a Resource no longer in the config stay queued.
pub fn runs_to_start(
    resources: &BTreeMap<String, Resource>,
    leases: &[Lease],
    queued: &[QueuedRun],
) -> Vec<i64> {
    let mut free: HashMap<&str, u32> = resources
        .iter()
        .map(|(name, r)| {
            let held = leases.iter().filter(|l| l.resource == *name).count() as u32;
            (name.as_str(), r.slots.saturating_sub(held))
        })
        .collect();

    let mut queued: Vec<&QueuedRun> = queued.iter().collect();
    queued.sort_by_key(|r| r.id);
    let mut start = Vec::new();
    for run in queued {
        if let Some(slots) = free.get_mut(run.resource.as_str())
            && *slots > 0
        {
            *slots -= 1;
            start.push(run.id);
        }
    }
    start
}

/// A queued Run's place in line for its Resource, starting at 1.
pub fn queue_position(queued: &[QueuedRun], id: i64) -> Option<usize> {
    let run = queued.iter().find(|r| r.id == id)?;
    Some(
        queued
            .iter()
            .filter(|r| r.resource == run.resource && r.id <= id)
            .count(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::Process;

    fn resources(list: &[(&str, u32, &str)]) -> BTreeMap<String, Resource> {
        list.iter()
            .map(|(name, slots, process)| {
                (
                    name.to_string(),
                    Resource {
                        slots: *slots,
                        process: process.to_string(),
                    },
                )
            })
            .collect()
    }

    fn process(pid: u32, ppid: u32, name: &str) -> Process {
        Process {
            pid,
            ppid,
            started: "t".into(),
            name: name.into(),
        }
    }

    fn queued(list: &[(i64, &str)]) -> Vec<QueuedRun> {
        list.iter()
            .map(|(id, r)| QueuedRun {
                id: *id,
                resource: r.to_string(),
            })
            .collect()
    }

    #[test]
    fn one_slot_runs_one_at_a_time_in_order() {
        let res = resources(&[("unity", 1, "Unity")]);
        let q = queued(&[(3, "unity"), (2, "unity")]);
        assert_eq!(runs_to_start(&res, &[], &q), vec![2]);

        let running = [RunningRun {
            id: 2,
            resource: "unity".into(),
            pid: Some(100),
        }];
        let held = leases(&res, &running, &Snapshot::default());
        assert_eq!(
            runs_to_start(&res, &held, &queued(&[(3, "unity")])),
            Vec::<i64>::new()
        );
    }

    #[test]
    fn hand_started_process_holds_a_lease_and_blocks_the_queue() {
        let res = resources(&[("unity", 1, "Unity")]);
        let snapshot =
            Snapshot::from_processes([process(1, 0, "launchd"), process(500, 1, "Unity")]);
        let held = leases(&res, &[], &snapshot);
        assert_eq!(
            held,
            vec![Lease {
                resource: "unity".into(),
                holder: Holder::Process {
                    pid: 500,
                    name: "Unity".into()
                }
            }]
        );
        assert!(runs_to_start(&res, &held, &queued(&[(1, "unity")])).is_empty());

        // Once the Editor quits the Lease is gone and the Run starts.
        let held = leases(
            &res,
            &[],
            &Snapshot::from_processes([process(1, 0, "launchd")]),
        );
        assert_eq!(
            runs_to_start(&res, &held, &queued(&[(1, "unity")])),
            vec![1]
        );
    }

    #[test]
    fn process_started_by_a_run_shares_the_runs_lease() {
        let res = resources(&[("unity", 1, "Unity")]);
        // Run 7 is a script (pid 200) that launched the Editor (pid 202) via a shell.
        let snapshot = Snapshot::from_processes([
            process(200, 1, "run-playmode.sh"),
            process(201, 200, "bash"),
            process(202, 201, "Unity"),
        ]);
        let running = [RunningRun {
            id: 7,
            resource: "unity".into(),
            pid: Some(200),
        }];
        let held = leases(&res, &running, &snapshot);
        assert_eq!(
            held,
            vec![Lease {
                resource: "unity".into(),
                holder: Holder::Run { id: 7 }
            }]
        );
    }

    #[test]
    fn editor_orphaned_by_a_finished_run_still_holds_a_lease() {
        let res = resources(&[("unity", 1, "Unity")]);
        let snapshot = Snapshot::from_processes([process(202, 1, "Unity")]);
        let held = leases(&res, &[], &snapshot);
        assert_eq!(held.len(), 1);
        assert!(runs_to_start(&res, &held, &queued(&[(8, "unity")])).is_empty());
    }

    #[test]
    fn resources_are_independent_and_respect_slot_counts() {
        let res = resources(&[("unity", 1, "Unity"), ("sim", 2, "Simulator")]);
        let snapshot = Snapshot::from_processes([process(9, 1, "Unity")]);
        let held = leases(&res, &[], &snapshot);
        let q = queued(&[
            (1, "unity"),
            (2, "sim"),
            (3, "sim"),
            (4, "sim"),
            (5, "gone"),
        ]);
        assert_eq!(runs_to_start(&res, &held, &q), vec![2, 3]);
    }

    #[test]
    fn queue_position_counts_only_the_same_resource() {
        let q = queued(&[(1, "unity"), (2, "sim"), (3, "unity")]);
        assert_eq!(queue_position(&q, 3), Some(2));
        assert_eq!(queue_position(&q, 2), Some(1));
        assert_eq!(queue_position(&q, 4), None);
    }
}
