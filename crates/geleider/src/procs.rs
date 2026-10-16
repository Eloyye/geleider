//! A snapshot of the Mac's process table, read through `ps`.

use std::collections::HashMap;
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    /// `ps`'s start time. With the pid it identifies a process even if the pid is
    /// reused later.
    pub started: String,
    /// The executable's file name, e.g. `ck3`.
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub processes: HashMap<u32, Process>,
}

impl Snapshot {
    pub fn take() -> Result<Self> {
        let output = Command::new("ps")
            .args(["-axww", "-o", "pid=,ppid=,lstart=,comm="])
            .output()
            .context("running ps")?;
        if !output.status.success() {
            bail!(
                "ps failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(Self::parse(&String::from_utf8_lossy(&output.stdout)))
    }

    pub fn parse(text: &str) -> Self {
        let processes = text
            .lines()
            .filter_map(parse_line)
            .map(|p| (p.pid, p))
            .collect();
        Self { processes }
    }

    #[cfg(test)]
    pub fn from_processes(list: impl IntoIterator<Item = Process>) -> Self {
        Self {
            processes: list.into_iter().map(|p| (p.pid, p)).collect(),
        }
    }

    /// Whether the process `pid` that started at `started` is still running.
    pub fn is_alive(&self, pid: u32, started: Option<&str>) -> bool {
        match self.processes.get(&pid) {
            Some(p) => started.is_none_or(|s| s == p.started),
            None => false,
        }
    }

    /// Whether `pid` is `ancestor` or one of its descendants.
    pub fn descends_from(&self, pid: u32, ancestor: u32) -> bool {
        let mut current = pid;
        // The depth bound guards against a cycle in a torn snapshot.
        for _ in 0..256 {
            if current == ancestor {
                return true;
            }
            match self.processes.get(&current) {
                Some(p) if p.ppid != current && p.ppid != 0 => current = p.ppid,
                _ => return false,
            }
        }
        false
    }
}

/// The start time of one process, used to recognise it after a restart.
pub fn start_time(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let started = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!started.is_empty()).then_some(started)
}

fn parse_line(line: &str) -> Option<Process> {
    // `lstart` is always five words, e.g. `Wed Oct 15 18:03:00 2026`; `comm` is the
    // rest of the line and may contain spaces.
    let (pid, rest) = next_word(line)?;
    let (ppid, rest) = next_word(rest)?;
    let mut started = Vec::new();
    let mut rest = rest;
    for _ in 0..5 {
        let (word, tail) = next_word(rest)?;
        started.push(word);
        rest = tail;
    }
    let comm = rest.trim();
    let name = comm.rsplit('/').next().unwrap_or(comm);
    if name.is_empty() {
        return None;
    }
    Some(Process {
        pid: pid.parse().ok()?,
        ppid: ppid.parse().ok()?,
        started: started.join(" "),
        name: name.to_string(),
    })
}

fn next_word(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if text.is_empty() {
        return None;
    }
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    Some((&text[..end], &text[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps_output_with_spaces_in_paths() {
        let snapshot = Snapshot::parse(
            "    1     0 Wed Oct 15 09:00:01 2026     /sbin/launchd\n\
             4567     1 Wed Oct 15 18:03:00 2026     /Games/Crusader Kings III/binaries/ck3.app/Contents/MacOS/ck3\n\
             garbage\n",
        );
        let ck3 = &snapshot.processes[&4567];
        assert_eq!(ck3.name, "ck3");
        assert_eq!(ck3.ppid, 1);
        assert_eq!(ck3.started, "Wed Oct 15 18:03:00 2026");
        assert_eq!(snapshot.processes.len(), 2);
        assert!(snapshot.is_alive(4567, Some("Wed Oct 15 18:03:00 2026")));
        assert!(!snapshot.is_alive(4567, Some("Thu Oct 16 08:00:00 2026")));
    }

    #[test]
    fn real_snapshot_contains_this_process() {
        let snapshot = Snapshot::take().unwrap();
        assert!(snapshot.processes.contains_key(&std::process::id()));
        assert!(start_time(std::process::id()).is_some());
    }
}
