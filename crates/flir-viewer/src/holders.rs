//! Finds programs that may be holding the camera. UsbDk gives a device to
//! one process at a time and cannot say which one has it, so this looks for
//! the usual suspects by name: another copy of this viewer, the `dump`
//! tools, FLIR's own software.

use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const RESCAN: Duration = Duration::from_secs(2);

pub struct Holder {
    pub pid: Pid,
    pub name: String,
}

pub struct Scanner {
    sys: System,
    last: Option<Instant>,
    pub found: Vec<Holder>,
}

impl Scanner {
    pub fn new() -> Self {
        Self { sys: System::new(), last: None, found: Vec::new() }
    }

    /// Rescans the process list at most every couple of seconds.
    pub fn refresh(&mut self) {
        if self.last.is_some_and(|t| t.elapsed() < RESCAN) {
            return;
        }
        self.last = Some(Instant::now());
        self.sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
        let me = sysinfo::get_current_pid().ok();
        let mut found: Vec<Holder> = self
            .sys
            .processes()
            .iter()
            // Linux lists threads too; keep whole processes, other than us.
            .filter(|(pid, p)| Some(**pid) != me && p.thread_kind().is_none())
            .filter_map(|(pid, p)| {
                let name = p.name().to_string_lossy().into_owned();
                may_hold_camera(&name).then_some(Holder { pid: *pid, name })
            })
            .collect();
        found.sort_by_key(|h| h.pid);
        log::debug!("{} process(es) may hold the camera", found.len());
        self.found = found;
    }

    /// Ends a process; the next scan shows whether it is gone.
    pub fn kill(&mut self, pid: Pid) -> bool {
        let ok = self.sys.process(pid).is_some_and(|p| p.kill());
        self.last = None;
        ok
    }
}

pub fn may_hold_camera(process_name: &str) -> bool {
    let n = process_name.to_lowercase();
    let stem = n.strip_suffix(".exe").unwrap_or(&n);
    matches!(stem, "flir-viewer" | "dump" | "dump_libusb") || n.contains("flir") || n.contains("thermal studio")
}

#[cfg(test)]
mod tests {
    use super::may_hold_camera;

    #[test]
    fn recognises_camera_users() {
        assert!(may_hold_camera("flir-viewer.exe"));
        assert!(may_hold_camera("dump.exe"));
        assert!(may_hold_camera("FLIR Tools.exe"));
        assert!(!may_hold_camera("explorer.exe"));
        assert!(!may_hold_camera("dumpster.exe"));
    }
}
