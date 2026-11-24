use crate::{
    errors::{AppError, describe_exit},
    packages::Package,
    portable_lock,
    scoop::Scoop,
    winget::{
        EFFECTCRAFT_ID, EFFECTCRAFT_SCOPE_FIX_VERSION, Output, REQUEST_SUPERSEDED, Request, Scope,
        Table, Winget, can_pin_version, parse_table,
    },
};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    thread,
};

const HISTORY_LIMIT: usize = 80;
pub const QUEUE_LIMIT: usize = 200;
const DIAGNOSTIC_LIMIT: usize = 48 * 1024;

fn installer_code(output: &Output) -> Option<u32> {
    if !matches!(
        output.code,
        0x8a150006 | 0x8a150030 | 0x8a150049 | 0x8a150052 | 0x8a150057
    ) {
        return None;
    }
    output.text.lines().rev().find_map(|line| {
        let value = line
            .trim()
            .strip_prefix("Installer failed with exit code:")
            .or_else(|| line.trim().strip_prefix("Uninstall failed with exit code:"))?
            .split_whitespace()
            .next()?;
        if let Some(hex) = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        {
            u32::from_str_radix(hex, 16).ok()
        } else {
            value
                .parse()
                .ok()
                .or_else(|| value.parse::<i32>().ok().map(|code| code as u32))
        }
    })
}

fn windows_sharing_error(text: &str) -> bool {
    [32, 33].into_iter().any(|code| {
        let message = std::io::Error::from_raw_os_error(code).to_string();
        message
            .split_once(" (os error ")
            .is_some_and(|(message, _)| text.contains(message))
    }) || text
        .contains("The process cannot access the file because it is being used by another process")
}

fn outcome(output: &Output) -> (Status, String) {
    let nested = installer_code(output);
    let code = nested.unwrap_or(output.code);
    let status = match code {
        0x8a150101 | 0x8a150103 | 0x8a150111 | 0x80070020 | 0x80070021 | 0x80073d02 => {
            Status::AppInUse
        }
        32 | 33 if nested.is_none() => Status::AppInUse,
        1618 | 0x80070652 | 0x8a150102 => Status::InstallerBusy,
        1641 | 3010 | 0x80070669 | 0x80070bc2 | 0x8a150109..=0x8a15010b => Status::RestartRequired,
        1602 | 0x80070642 | 0x800704c7 | 0x8a150005 | 0x8a15010c => Status::Cancelled,
        _ if matches!(output.code, 0x8a150052 | 0x8a150057)
            && windows_sharing_error(&output.text) =>
        {
            Status::AppInUse
        }
        _ if output.success() => Status::Completed,
        _ => Status::Failed,
    };
    let message = match status {
        Status::AppInUse => {
            "Files are in use. Save your work and close the app or other programs using its files, then review the change again."
        }
        Status::InstallerBusy => {
            "Another installer is running. Wait for it to finish, then resume queued work or review this operation again."
        }
        Status::RestartRequired if code == 0x8a15010a => {
            "Windows must restart before this change can run. Review the remaining changes after restarting."
        }
        Status::RestartRequired if matches!(code, 1641 | 0x80070669 | 0x8a15010b) => {
            "The installer reported that a Windows restart was initiated. Check the installed state after restarting."
        }
        Status::RestartRequired => {
            "A restart is required to finish. Check the installed state and review remaining changes after restarting."
        }
        Status::Cancelled => {
            "WinGet or the installer reported cancellation. Check the installed state before trying again."
        }
        _ if nested.is_some() => {
            return (
                status,
                format!("The installer failed with exit code {code}. Open View log for details."),
            );
        }
        _ => describe_exit(output.code),
    };
    (status, message.into())
}

fn bounded(mut text: String) -> String {
    if text.len() > DIAGNOSTIC_LIMIT {
        let mut start = text.len() - DIAGNOSTIC_LIMIT;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text.drain(..start);
        text.insert_str(0, "[Earlier output omitted]\n");
    }
    text
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Queued,
    Running,
    Completed,
    CheckRequired,
    AppInUse,
    InstallerBusy,
    RestartRequired,
    Failed,
    Cancelled,
}
impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Running => "Running",
            Self::Completed => "Completed",
            Self::CheckRequired => "Check installed state",
            Self::AppInUse => "Files in use",
            Self::InstallerBusy => "Installer busy",
            Self::RestartRequired => "Restart required",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }

    pub fn reviewable(&self) -> bool {
        matches!(self, Self::AppInUse | Self::InstallerBusy)
    }
}
#[derive(Clone, Debug)]
pub struct Operation {
    pub serial: u64,
    pub package: Package,
    pub verb: &'static str,
    pub status: Status,
    pub diagnostic: String,
    pub source_consent: bool,
    pub package_consent: bool,
}

impl Operation {
    fn cancel(&mut self, reason: &str) {
        self.status = Status::Cancelled;
        self.diagnostic = reason.into();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueBlock {
    InstallerBusy,
    RestartRequired,
}
#[derive(Clone, Default)]
pub struct Queue {
    pub entries: VecDeque<Operation>,
    serial: u64,
    block: Option<QueueBlock>,
}
impl Queue {
    pub fn enqueue_group(
        &mut self,
        changes: Vec<(Package, &'static str)>,
        source_consent: bool,
        package_consent: bool,
    ) -> Result<(), String> {
        if self.block == Some(QueueBlock::RestartRequired) {
            return Err("Restart Windows before starting more package changes.".into());
        }
        // Stage the whole group so an invalid, duplicate, or over-capacity
        // entry cannot leave part of a confirmed selection in the queue.
        let mut staged = self.clone();
        for (package, verb) in changes {
            staged.enqueue(package, verb, source_consent, package_consent)?;
        }
        *self = staged;
        Ok(())
    }

    fn enqueue(
        &mut self,
        package: Package,
        verb: &'static str,
        source_consent: bool,
        package_consent: bool,
    ) -> Result<(), String> {
        if !package.manageable() || !["install", "upgrade", "uninstall"].contains(&verb) {
            return Err("This package cannot be managed reliably by WinGet.".into());
        }
        if package
            .scoop
            .as_ref()
            .is_some_and(|scoop| verb != "install" || package.version != scoop.version)
        {
            return Err("Scoop is available for reviewed new installs only.".into());
        }
        if verb != "uninstall" && !package_consent {
            return Err("Publisher agreement consent is required.".into());
        }
        if self.entries.iter().any(|e| {
            e.package.key() == package.key() && matches!(e.status, Status::Queued | Status::Running)
        }) {
            return Err(format!("{} already has an active operation.", package.name));
        }
        if self.pending() >= QUEUE_LIMIT {
            return Err(
                "The operation queue is full. Wait for current operations to complete.".into(),
            );
        }
        self.serial += 1;
        self.entries.push_back(Operation {
            serial: self.serial,
            package,
            verb,
            status: Status::Queued,
            diagnostic: "Waiting for the current operation to finish.".into(),
            source_consent,
            package_consent,
        });
        self.prune();
        Ok(())
    }
    fn prune(&mut self) {
        while self
            .entries
            .iter()
            .filter(|e| !matches!(e.status, Status::Queued | Status::Running))
            .count()
            > HISTORY_LIMIT
        {
            if let Some(index) = self
                .entries
                .iter()
                .position(|e| !matches!(e.status, Status::Queued | Status::Running))
            {
                self.entries.remove(index);
            }
        }
    }
    pub fn start_next(&mut self) -> Option<Operation> {
        if self.running() || self.block.is_some() {
            return None;
        }
        let next = self
            .entries
            .iter_mut()
            .find(|e| e.status == Status::Queued)?;
        next.status = Status::Running;
        next.diagnostic = "WinGet is running. Check for publisher installer or UAC windows. Progress is indeterminate. For safety, active installers are allowed to finish; only queued operations can be cancelled.".into();
        Some(next.clone())
    }
    pub fn finish(&mut self, serial: u64, status: Status, diagnostic: String) {
        let Some(e) = self
            .entries
            .iter_mut()
            .find(|e| e.serial == serial && e.status == Status::Running)
        else {
            return;
        };
        e.diagnostic = bounded(diagnostic);
        let block = match status {
            Status::InstallerBusy => Some(QueueBlock::InstallerBusy),
            Status::RestartRequired => Some(QueueBlock::RestartRequired),
            _ => None,
        };
        e.status = status;
        if let Some(block) = block {
            self.block = Some(block);
            if block == QueueBlock::RestartRequired {
                self.cancel_waiting("Skipped before launch because Windows needs to restart. Review this change after restarting.");
            }
        }
        self.prune();
    }
    pub fn cancel(&mut self, serial: u64) -> bool {
        if let Some(e) = self
            .entries
            .iter_mut()
            .find(|e| e.serial == serial && e.status == Status::Queued)
        {
            e.cancel("Cancelled before the package change started.");
            self.prune();
            true
        } else {
            false
        }
    }

    fn cancel_waiting(&mut self, reason: &str) {
        for entry in &mut self.entries {
            if entry.status == Status::Queued {
                entry.cancel(reason);
            }
        }
        self.prune();
    }
    pub fn pending(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.status, Status::Queued | Status::Running))
            .count()
    }
    pub fn running(&self) -> bool {
        self.entries.iter().any(|e| e.status == Status::Running)
    }
}

struct Shared {
    queue: Queue,
    shutdown: bool,
}

fn verify_effectcraft_scope(
    package: &Package,
    machine: Table,
    user: Table,
) -> Result<Scope, AppError> {
    let scope_error = || {
        AppError("Could not verify one existing EffectCraft installation with the reviewed version. Refresh Updates before trying again.".into())
    };
    let mut verified = None;
    for (scope, table) in [(Scope::Machine, machine), (Scope::User, user)] {
        if table.skipped != 0 || table.ambiguous {
            return Err(scope_error());
        }
        match table.packages.as_slice() {
            [] => {}
            [entry]
                if verified.is_none()
                    && entry.key() == package.key()
                    && entry.installed.is_some()
                    && entry.installed == package.installed =>
            {
                verified = Some(scope)
            }
            _ => return Err(scope_error()),
        }
    }
    verified.ok_or_else(scope_error)
}

#[derive(Clone)]
pub struct Manager {
    shared: Arc<(Mutex<Shared>, Condvar)>,
}

struct OperationResult {
    status: Status,
    diagnostic: String,
}

impl From<Output> for OperationResult {
    fn from(output: Output) -> Self {
        let (status, message) = outcome(&output);
        Self {
            status,
            diagnostic: format!("{message} (0x{:08X})\n\n{}", output.code, output.text),
        }
    }
}
impl Manager {
    pub fn start(winget: Winget, notify: impl Fn(bool) + Send + Sync + 'static) -> Self {
        Self::start_with(notify, move |operation, progress, closing| {
            let package = &operation.package;
            let scoop = (operation.verb == "install").then(Scoop::detect).flatten();
            if let Some(reviewed) = &package.scoop {
                let scoop = scoop.as_ref().ok_or_else(|| {
                    AppError("The existing Scoop installation is no longer available.".into())
                })?;
                progress(format!("Rechecking {} with Scoop…", reviewed.qualified()));
                let (success, verified, log) = scoop.install(
                    &winget,
                    package,
                    reviewed,
                    operation.source_consent,
                    closing,
                    progress,
                )?;
                return Ok(OperationResult {
                    status: match (success, verified) {
                        (true, true) => Status::Completed,
                        (false, false) => Status::Failed,
                        _ => Status::CheckRequired,
                    },
                    diagnostic: if success && verified {
                        format!(
                            "Scoop installed and verified {}.\n\n{log}",
                            reviewed.version
                        )
                    } else {
                        format!("Check the Scoop installed state before trying again.\n\n{log}")
                    },
                });
            }
            if operation.verb == "install"
                && let Some(scoop) = &scoop
            {
                scoop.check_absent(&package.id)?;
            }
            progress("Rechecking the reviewed package in WinGet…".into());
            let current = winget.review_package(
                operation.verb,
                package,
                operation.source_consent,
                closing,
            )?;
            if operation.verb != "install"
                && let Some(in_use) = portable_lock::find(&package.id, current.installed.as_deref())
            {
                let reason = match in_use {
                    portable_lock::InUse::Running(path) => format!(
                        "Windows reports that the installed executable is running: {}",
                        path.display()
                    ),
                    portable_lock::InUse::Locked(path) => {
                        format!("Windows reported a sharing lock on {}", path.display())
                    }
                };
                return Ok(OperationResult {
                    status: Status::AppInUse,
                    diagnostic: format!(
                        "Files are in use. Save your work and close the app or other programs using its files, then review the change again.\n\n{reason}\nThe installer was never started.",
                    ),
                });
            }
            let version = match operation.verb {
                "install" if current.version == package.version => Some(package.version.clone()),
                "install" => {
                    return Err(AppError(
                        "The catalog version changed after confirmation. Review the package again."
                            .into(),
                    ));
                }
                "upgrade" => package.available.clone(),
                _ => package
                    .installed
                    .as_ref()
                    .filter(|v| can_pin_version(v))
                    .cloned(),
            };
            let inventory = |scope, obsolete: &dyn Fn() -> bool| {
                parse_table(
                    &winget.read(
                        &Request::InstalledExact(package.id.clone(), Some(scope)),
                        operation.source_consent,
                        obsolete,
                    )?,
                    true,
                )
            };
            let mut effectcraft_machine_update = false;
            if operation.verb == "upgrade"
                && package.id.eq_ignore_ascii_case(EFFECTCRAFT_ID)
                && package.available.as_deref() == Some(EFFECTCRAFT_SCOPE_FIX_VERSION)
            {
                progress(
                    "Checking the existing EffectCraft installation scope with WinGet…".into(),
                );
                effectcraft_machine_update = matches!(
                    verify_effectcraft_scope(
                        package,
                        inventory(Scope::Machine, closing)?,
                        inventory(Scope::User, closing)?
                    )?,
                    Scope::Machine
                );
            }
            let mut output = winget.run(
                &Request::Change {
                    verb: operation.verb,
                    id: operation.package.id.clone(),
                    version: version.clone(),
                    accept_package: operation.package_consent,
                    effectcraft_machine_update,
                },
                operation.source_consent,
                closing,
                &mut *progress,
            )?;
            if effectcraft_machine_update && output.success() {
                let mut expected = package.clone();
                expected.installed = package.available.clone();
                let verified = match (
                    inventory(Scope::Machine, &|| false),
                    inventory(Scope::User, &|| false),
                ) {
                    (Ok(machine), Ok(user)) => matches!(
                        verify_effectcraft_scope(&expected, machine, user),
                        Ok(Scope::Machine)
                    ),
                    _ => false,
                };
                let status = if verified {
                    "Verified the updated EffectCraft installation is machine-wide."
                } else {
                    "WinGet reported success. The updated EffectCraft installation scope could not yet be verified. Refresh the inventory after any requested restart."
                };
                output.text = format!("{status}\n\n{}", output.text);
            }
            let mut result = OperationResult::from(output);
            if result.status == Status::Completed {
                progress("Verifying installed state with WinGet…".into());
                let inventory = winget.exact_package(
                    &Request::InstalledExact(package.id.clone(), None),
                    package,
                    true,
                    operation.source_consent,
                    || false,
                );
                let checked = installation_matches(operation.verb, version.as_deref(), inventory);
                if let Err(reason) = checked {
                    result.status = Status::CheckRequired;
                    result.diagnostic = format!(
                        "WinGet reported success. Verify the installed state before trying again.\n\n{reason}\n\n{}",
                        result.diagnostic
                    );
                }
            }
            Ok(result)
        })
    }
    fn start_with(
        notify: impl Fn(bool) + Send + Sync + 'static,
        run: impl Fn(
            &Operation,
            &mut dyn FnMut(String),
            &dyn Fn() -> bool,
        ) -> Result<OperationResult, AppError>
        + Send
        + 'static,
    ) -> Self {
        let manager = Self {
            shared: Arc::new((
                Mutex::new(Shared {
                    queue: Queue::default(),
                    shutdown: false,
                }),
                Condvar::new(),
            )),
        };
        let shared = manager.shared.clone();
        let notify = Arc::new(notify);
        thread::spawn(move || {
            loop {
                let operation = {
                    let (lock, signal) = &*shared;
                    let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if s.shutdown {
                            return;
                        }
                        if let Some(op) = s.queue.start_next() {
                            break op;
                        }
                        s = signal.wait(s).unwrap_or_else(|e| e.into_inner());
                    }
                };
                notify(false);
                let closing = || shared.0.lock().unwrap_or_else(|e| e.into_inner()).shutdown;
                let result = run(
                    &operation,
                    &mut |text| {
                        let mut s = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(e) = s
                            .queue
                            .entries
                            .iter_mut()
                            .find(|e| e.serial == operation.serial)
                        {
                            e.diagnostic = bounded(text);
                        }
                        drop(s);
                        notify(false);
                    },
                    &closing,
                );
                let result = match result {
                    Ok(result) => result,
                    Err(e) if e.0 == REQUEST_SUPERSEDED && closing() => OperationResult {
                        status: Status::Cancelled,
                        diagnostic:
                            "Cancelled during package verification, before the installer started."
                                .into(),
                    },
                    Err(e) => OperationResult {
                        status: Status::Failed,
                        diagnostic: e.to_string(),
                    },
                };
                shared
                    .0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .queue
                    .finish(operation.serial, result.status, result.diagnostic);
                notify(true);
            }
        });
        manager
    }
    pub fn enqueue_group(
        &self,
        changes: Vec<(Package, &'static str)>,
        source_consent: bool,
        package_consent: bool,
    ) -> Result<(), String> {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown {
            return Err("SlimStore is closing. Reopen it to start new operations.".into());
        }
        state
            .queue
            .enqueue_group(changes, source_consent, package_consent)?;
        self.shared.1.notify_one();
        Ok(())
    }
    pub fn snapshot(&self) -> Vec<Operation> {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .entries
            .iter()
            .cloned()
            .collect()
    }
    pub fn cancel(&self, serial: u64) {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .cancel(serial);
    }
    pub fn block(&self) -> Option<QueueBlock> {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .block
    }

    pub fn resume(&self) -> bool {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.queue.block != Some(QueueBlock::InstallerBusy) {
            return false;
        }
        state.queue.block = None;
        self.shared.1.notify_one();
        true
    }
    pub fn pending(&self) -> usize {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .pending()
    }
    pub fn stop_after_active(&self) -> bool {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown = true;
        state
            .queue
            .cancel_waiting("Cancelled before the package change started.");
        self.shared.1.notify_all();
        state.queue.running()
    }

    pub fn is_stopping(&self) -> bool {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown
    }
}

fn installation_matches(
    verb: &str,
    target: Option<&str>,
    installed: Result<Option<Package>, AppError>,
) -> Result<(), String> {
    let installed = installed.map_err(|error| error.to_string())?;
    let current = installed
        .as_ref()
        .and_then(|package| package.installed.as_deref());
    if verb == "uninstall" {
        return if installed.is_none() {
            Ok(())
        } else {
            Err(format!(
                "WinGet still lists this package (version {}).",
                current.unwrap_or("unknown")
            ))
        };
    }
    match (target, current) {
        (Some(expected), Some(actual)) if actual == expected => Ok(()),
        (Some(expected), Some(actual)) => Err(format!(
            "Reviewed version {expected}, inventory reports {actual}."
        )),
        _ => Err("WinGet could not confirm the installed version.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_with_conflict_or_missing_consent_never_queues_partially() {
        let mut queue = Queue::default();
        queue.enqueue(p("A.One"), "install", false, true).unwrap();
        let group = vec![(p("B.Two"), "install"), (p("a.one"), "install")];
        assert!(queue.enqueue_group(group, false, true).is_err());
        assert_eq!(queue.pending(), 1);
        let group = vec![(p("B.Two"), "install"), (p("C.Three"), "install")];
        assert!(queue.enqueue_group(group.clone(), false, false).is_err());
        assert_eq!(queue.pending(), 1);
        queue.enqueue_group(group, false, true).unwrap();
        assert_eq!(queue.pending(), 3);
    }
    #[test]
    fn uninstall_group_queues_serially_without_install_agreements() {
        let mut queue = Queue::default();
        queue
            .enqueue_group(
                vec![(p("A.One"), "uninstall"), (p("B.Two"), "uninstall")],
                false,
                false,
            )
            .unwrap();
        let first = queue.start_next().unwrap();
        assert_eq!(
            (first.package.id.as_str(), first.verb),
            ("A.One", "uninstall")
        );
        assert!(queue.start_next().is_none());
        queue.finish(first.serial, Status::Completed, String::new());
        let second = queue.start_next().unwrap();
        assert_eq!(
            (second.package.id.as_str(), second.verb),
            ("B.Two", "uninstall")
        );
    }
    #[test]
    fn mocked_backend_failure_progress_and_recovery() {
        let (tx, rx) = std::sync::mpsc::channel();
        let manager = Manager::start_with(
            move |finished| {
                if finished {
                    let _ = tx.send(());
                }
            },
            |op, progress, _| {
                progress("Real mock diagnostic".into());
                Ok(Output {
                    code: if op.package.id == "A.One" {
                        0x80070005
                    } else {
                        0
                    },
                    text: "Mock completed".into(),
                }
                .into())
            },
        );
        manager
            .enqueue_group(
                vec![(p("A.One"), "install"), (p("B.Two"), "upgrade")],
                false,
                true,
            )
            .unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let ops = manager.snapshot();
        assert_eq!(ops[0].status, Status::Failed);
        assert_eq!(ops[1].status, Status::Completed);
        assert!(ops[0].diagnostic.contains("Permission"));
        assert!(!manager.stop_after_active());
    }
    #[test]
    fn unicode_diagnostics_are_bounded() {
        let text = bounded("日".repeat(60_000));
        assert!(text.len() < DIAGNOSTIC_LIMIT + 100);
        assert!(text.contains("Earlier output omitted"));
    }

    #[test]
    fn scope_fix_requires_one_installation_of_the_reviewed_version() {
        let mut package = p(EFFECTCRAFT_ID);
        package.installed = Some("0.24.1".into());
        let inventory = |entries| Table {
            packages: entries,
            skipped: 0,
            ambiguous: false,
        };
        let matching = || inventory(vec![package.clone()]);
        let empty = || inventory(vec![]);

        assert!(matches!(
            verify_effectcraft_scope(&package, matching(), empty()),
            Ok(Scope::Machine)
        ));
        assert!(matches!(
            verify_effectcraft_scope(&package, empty(), matching()),
            Ok(Scope::User)
        ));
        assert!(verify_effectcraft_scope(&package, matching(), matching()).is_err());
        assert!(verify_effectcraft_scope(&package, empty(), empty()).is_err());
        let mut changed = package.clone();
        changed.installed = Some("0.25.1".into());
        assert!(verify_effectcraft_scope(&package, inventory(vec![changed]), empty()).is_err());
    }

    #[test]
    fn reports_nested_installer_exit_code() {
        let output = Output { code: 0x8a150006, text: "Hash verified\nInstaller failed with exit code: 1603\nInstaller log is available at: path".into() };
        assert!(outcome(&output).1.contains("1603"));
    }

    #[test]
    fn conditions_distinguish_file_locks_busy_restarts_and_cancellation() {
        for (code, text, expected) in [
            (0x8a150101, "", Status::AppInUse),
            (0x8a150111, "", Status::AppInUse),
            (0x80073d02, "", Status::AppInUse),
            (
                0x8a150057,
                "remove_all: The process cannot access the file because it is being used by another process.\nUninstall failed with exit code: 0x8a150003 : Executing command failed",
                Status::AppInUse,
            ),
            (
                0x8a150057,
                "Uninstall failed with exit code: 0x80070005",
                Status::Failed,
            ),
            (
                0x8a150049,
                "Installer failed with exit code: 1618",
                Status::InstallerBusy,
            ),
            (
                0x8a150030,
                "Uninstall failed with exit code: 0x80070652",
                Status::InstallerBusy,
            ),
            (
                0x8a150006,
                "Installer failed with exit code: 1602",
                Status::Cancelled,
            ),
            (0x800704c7, "", Status::Cancelled),
            (3010, "", Status::RestartRequired),
            (1641, "", Status::RestartRequired),
            (0x8a15010a, "", Status::RestartRequired),
            (0x8a15010b, "", Status::RestartRequired),
            (0x8a150011, "Installer hash does not match", Status::Failed),
            (0x80190194, "Not found (404)", Status::Failed),
            (
                0x8a15002e,
                "Installer failed with exit code: 1618",
                Status::Failed,
            ),
            (
                0,
                "Installer failed with exit code: 1602",
                Status::Completed,
            ),
        ] {
            assert_eq!(
                outcome(&Output {
                    code,
                    text: text.into()
                })
                .0,
                expected,
                "{code:#x}"
            );
        }
    }

    #[test]
    fn installer_busy_waits_for_explicit_resume_without_replaying_attempt() {
        let (tx, rx) = std::sync::mpsc::channel();
        let timeout = std::time::Duration::from_secs(5);
        let manager = Manager::start_with(
            move |done| {
                if done {
                    tx.send(()).unwrap();
                }
            },
            |op, _, _| {
                Ok(Output {
                    code: if op.package.id == "A.One" {
                        0x8a150102
                    } else {
                        0
                    },
                    text: String::new(),
                }
                .into())
            },
        );
        manager
            .enqueue_group(
                vec![(p("A.One"), "install"), (p("B.Two"), "install")],
                false,
                true,
            )
            .unwrap();
        rx.recv_timeout(timeout).unwrap();
        assert_eq!(manager.block(), Some(QueueBlock::InstallerBusy));
        assert_eq!(manager.snapshot()[1].status, Status::Queued);
        assert!(manager.resume());
        rx.recv_timeout(timeout).unwrap();
        let jobs = manager.snapshot();
        assert_eq!(jobs[0].status, Status::InstallerBusy);
        assert_eq!(jobs[1].status, Status::Completed);
        assert!(!manager.resume());
        manager.stop_after_active();
    }

    #[test]
    fn reboot_outcomes_stop_waiting_and_new_work_for_the_session() {
        let mut queue = Queue::default();
        queue
            .enqueue_group(
                vec![(p("A.One"), "install"), (p("B.Two"), "install")],
                false,
                true,
            )
            .unwrap();
        let first = queue.start_next().unwrap();
        queue.finish(
            first.serial,
            Status::RestartRequired,
            "Windows needs to restart".into(),
        );
        assert!(queue.start_next().is_none());
        assert_eq!(queue.pending(), 0);
        assert_eq!(queue.entries[1].status, Status::Cancelled);
        assert!(queue.entries[1].diagnostic.contains("restart"));
        assert!(
            queue
                .enqueue_group(vec![(p("C.Three"), "install")], false, true)
                .is_err()
        );
    }

    #[test]
    fn success_verification_requires_the_reviewed_version_or_absence() {
        let mut installed = p("A.One");
        installed.installed = Some("2.0".into());
        for action in ["install", "upgrade"] {
            assert!(installation_matches(action, Some("2.0"), Ok(Some(installed.clone()))).is_ok());
            assert!(
                installation_matches(action, Some("2.1"), Ok(Some(installed.clone()))).is_err()
            );
            assert!(installation_matches(action, Some("2.0"), Ok(None)).is_err());
        }
        assert!(installation_matches("uninstall", Some("2.0"), Ok(Some(installed))).is_err());
        assert!(installation_matches("uninstall", Some("2.0"), Ok(None)).is_ok());
        assert!(
            installation_matches(
                "uninstall",
                Some("2.0"),
                Err(AppError("inventory unavailable".into()))
            )
            .is_err()
        );
    }
    fn p(id: &str) -> Package {
        Package {
            id: id.into(),
            name: id.into(),
            source: "winget".into(),
            ..Default::default()
        }
    }
    #[test]
    fn serial_queue_prevents_conflicts_and_duplicates() {
        let mut q = Queue::default();
        q.enqueue(p("A.One"), "install", false, true).unwrap();
        assert!(q.enqueue(p("a.one"), "upgrade", false, true).is_err());
        q.enqueue(p("B.Two"), "install", false, true).unwrap();
        let first = q.start_next().unwrap();
        assert!(q.start_next().is_none());
        q.finish(first.serial, Status::Completed, "ok".into());
        assert_eq!(q.start_next().unwrap().package.id, "B.Two");
    }
    #[test]
    fn cancel_and_recover_after_failure() {
        let mut q = Queue::default();
        q.enqueue(p("A.One"), "install", false, true).unwrap();
        assert!(q.cancel(1));
        assert!(q.start_next().is_none());
        q.enqueue(p("A.One"), "install", false, true).unwrap();
        let op = q.start_next().unwrap();
        assert!(!q.cancel(op.serial));
        q.finish(op.serial, Status::Failed, "offline".into());
        q.enqueue(p("A.One"), "install", false, true).unwrap();
        assert!(q.start_next().is_some());
    }
    #[test]
    fn history_is_bounded() {
        let mut q = Queue::default();
        for _ in 0..120 {
            q.enqueue(p("A.One"), "install", false, true).unwrap();
            let op = q.start_next().unwrap();
            q.finish(op.serial, Status::Completed, "ok".into());
        }
        assert_eq!(q.entries.len(), HISTORY_LIMIT);
    }
    #[test]
    fn missing_backend_fails_and_queue_recovers() {
        let (tx, rx) = std::sync::mpsc::channel();
        let m = Manager::start(Winget { executable: None }, move |done| {
            if done {
                let _ = tx.send(());
            }
        });
        m.enqueue_group(
            vec![(p("A.One"), "install"), (p("B.Two"), "install")],
            false,
            true,
        )
        .unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(m.snapshot().iter().all(|e| e.status == Status::Failed));
        assert_eq!(m.pending(), 0);
        assert!(!m.stop_after_active());
    }

    #[test]
    fn closing_cancels_waiting_work_before_the_active_operation_finishes() {
        let timeout = std::time::Duration::from_secs(5);
        let (started, running) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (finished, completed) = std::sync::mpsc::channel();
        let manager = Manager::start_with(
            move |done| {
                if done {
                    let _ = finished.send(());
                }
            },
            move |op, _, _| {
                started.send(op.package.id.clone()).unwrap();
                if op.package.id == "A.One" {
                    released.recv_timeout(timeout).unwrap();
                }
                Ok(Output {
                    code: 0,
                    text: String::new(),
                }
                .into())
            },
        );
        manager
            .enqueue_group(
                vec![(p("A.One"), "install"), (p("B.Two"), "install")],
                false,
                true,
            )
            .unwrap();
        assert_eq!(running.recv_timeout(timeout).unwrap(), "A.One");
        assert!(manager.stop_after_active());
        assert_eq!(manager.snapshot()[1].status, Status::Cancelled);
        assert!(
            manager
                .enqueue_group(vec![(p("C.Three"), "install")], false, true)
                .is_err()
        );
        release.send(()).unwrap();
        completed.recv_timeout(timeout).unwrap();
        assert!(matches!(
            running.recv_timeout(timeout),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
        assert_eq!(manager.pending(), 0);
    }

    #[test]
    fn closing_during_preflight_reports_cancellation_without_launch() {
        let timeout = std::time::Duration::from_secs(5);
        let (verifying, started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (finished, completed) = std::sync::mpsc::channel();
        let manager = Manager::start_with(
            move |done| {
                if done {
                    finished.send(()).unwrap();
                }
            },
            move |_, _, closing| {
                verifying.send(()).unwrap();
                released.recv_timeout(timeout).unwrap();
                assert!(closing());
                Err(AppError(REQUEST_SUPERSEDED.into()))
            },
        );
        manager
            .enqueue_group(vec![(p("A.One"), "upgrade")], false, true)
            .unwrap();
        started.recv_timeout(timeout).unwrap();
        assert!(manager.stop_after_active());
        release.send(()).unwrap();
        completed.recv_timeout(timeout).unwrap();
        let rows = manager.snapshot();
        assert_eq!(rows[0].status, Status::Cancelled);
        assert!(rows[0].diagnostic.contains("before the installer started"));
    }
}
