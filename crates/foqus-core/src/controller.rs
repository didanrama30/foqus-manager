use crate::{
    AppliedChange, ChangeOutcome, ControlError, FocusSession, FocusState, GameProfile, PathPolicy,
    ProcessControlBackend, ProcessIdentity, ProcessOutcome, ProcessSample, SessionStatus,
    classify_path, journal::FocusJournal, unix_seconds,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ControllerError {
    #[error("focus journal error: {0}")]
    Journal(#[from] crate::ProfileError),
    #[error("previous session still needs recovery")]
    RestorePending,
}

impl<B: ProcessControlBackend> FocusController<B> {
    pub fn new(backend: B, journal_path: PathBuf) -> Self {
        Self {
            backend,
            journal_path,
            session: None,
            last_scan: None,
            scan_backoff: HashMap::new(),
        }
    }

    pub fn status(&self) -> SessionStatus {
        status_of(self.session.as_ref())
    }

    pub fn session(&self) -> Option<&FocusSession> {
        self.session.as_ref()
    }

    /// Load a journal left by an earlier controller and restore what is still safe.
    pub fn recover_if_needed(&mut self) -> Result<Option<SessionStatus>, ControllerError> {
        let Some(journal) = FocusJournal::load(&self.journal_path)? else {
            return Ok(None);
        };
        if journal.session.changes.is_empty() {
            FocusJournal::remove(&self.journal_path)?;
            return Ok(None);
        }
        let mut session = journal.session;
        session.state = FocusState::Recovery;
        session
            .errors
            .push("an unfinished session needs recovery; review and retry restore".into());
        self.session = Some(session);
        Ok(Some(self.status()))
    }

    pub fn start(
        &mut self,
        target: ProcessIdentity,
        profile: GameProfile,
    ) -> Result<SessionStatus, ControllerError> {
        if self.session.is_some() {
            let restored = self.stop();
            if !matches!(restored.state, FocusState::Idle) {
                return Ok(restored);
            }
        }
        self.session = Some(FocusSession {
            target,
            profile_snapshot: profile.clone(),
            state: FocusState::Starting,
            changes: vec![],
            outcomes: vec![],
            errors: vec![],
        });
        self.scan_backoff.clear();
        self.persist()?;

        let target_name = profile
            .executable_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("pid {}", target.pid));
        let target_result = self.apply_target(target, &target_name)?;
        let target_ok = matches!(target_result.outcome, ChangeOutcome::Applied);
        self.push_outcome(target_result.clone());
        if !target_ok {
            self.session_mut().state = FocusState::Error {
                message: target_result
                    .message
                    .clone()
                    .unwrap_or_else(|| "target priority could not be applied".into()),
            };
            self.session_mut().errors.push(format!(
                "{}: {}",
                target_name,
                target_result
                    .message
                    .unwrap_or_else(|| "target priority could not be applied".into())
            ));
            self.persist()?;
            return Ok(self.status());
        }

        let samples = self.backend.list_processes();
        for sample in samples {
            if sample.identity == target {
                continue;
            }
            if sample.capability == crate::ProcessCapability::SystemProtected
                || sample.state == crate::ProcessState::AccessDenied
            {
                continue;
            }
            let Some(path) = sample.executable_path.as_deref() else {
                continue;
            };
            let policy = classify_path(&profile, path);
            if policy == PathPolicy::Unmanaged {
                continue;
            }
            let name = sample.name.clone();
            let outcome = if policy == PathPolicy::Excluded {
                ProcessOutcome {
                    identity: sample.identity,
                    name,
                    outcome: ChangeOutcome::Excluded,
                    message: Some("excluded; no changes applied".into()),
                    partial: false,
                }
            } else {
                match self.apply_background(&sample, &name, &profile) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let _ = self.restore_all();
                        return Err(error);
                    }
                }
            };
            self.push_outcome(outcome);
        }

        self.session_mut().state = FocusState::Active {
            started_at_unix: unix_seconds(SystemTime::now()),
        };
        if let Err(error) = self.persist() {
            let _ = self.restore_all();
            return Err(error);
        }
        Ok(self.status())
    }

    pub fn stop(&mut self) -> SessionStatus {
        self.restore_all()
    }

    /// Called by the UI poll. A game that exited or a reused PID triggers restore.
    pub fn tick(&mut self) -> SessionStatus {
        let Some(session) = self.session.as_ref() else {
            return SessionStatus::default();
        };
        if !matches!(session.state, FocusState::Active { .. }) {
            return self.status();
        }
        if let Err(ControlError::Exited | ControlError::IdentityChanged) =
            self.backend.read_priorities(session.target)
        {
            return self.restore_all();
        }
        if self
            .last_scan
            .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(2))
        {
            self.scan_managed();
        }
        self.status()
    }

    fn scan_managed(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let profile = session.profile_snapshot.clone();
        let target = session.target;
        let known: Vec<ProcessIdentity> = session
            .changes
            .iter()
            .map(|change| change.identity)
            .collect();
        self.last_scan = Some(Instant::now());
        let samples = self.backend.list_processes();
        let mut changed = false;
        for sample in samples {
            if sample.identity == target || known.contains(&sample.identity) {
                continue;
            }
            if sample.capability == crate::ProcessCapability::SystemProtected
                || sample.state == crate::ProcessState::AccessDenied
            {
                continue;
            }
            if self
                .scan_backoff
                .get(&sample.identity)
                .is_some_and(|last| last.elapsed() < std::time::Duration::from_secs(30))
            {
                continue;
            }
            let Some(path) = sample.executable_path.as_deref() else {
                continue;
            };
            if classify_path(&profile, path) != PathPolicy::Managed {
                continue;
            }
            let name = sample.name.clone();
            match self.apply_background(&sample, &name, &profile) {
                Ok(outcome) => {
                    if !matches!(outcome.outcome, ChangeOutcome::Applied) {
                        self.scan_backoff.insert(sample.identity, Instant::now());
                    }
                    if !matches!(outcome.outcome, ChangeOutcome::Skipped) {
                        changed = true;
                    }
                    self.push_outcome(outcome);
                }
                Err(_) => {
                    self.scan_backoff.insert(sample.identity, Instant::now());
                }
            }
        }
        if changed {
            let _ = self.persist();
        }
    }

    pub fn retry_restore(&mut self) -> SessionStatus {
        self.restore_all()
    }

    /// Apply a profile change. Excluding a process immediately restores Foqus changes.
    pub fn update_profile(
        &mut self,
        profile: GameProfile,
    ) -> Result<SessionStatus, ControllerError> {
        let Some(session) = self.session.as_ref() else {
            return Ok(self.status());
        };
        let target = session.target;
        let excluded: Vec<ProcessIdentity> = session
            .changes
            .iter()
            .filter(|change| {
                change.executable_path.as_deref().is_some_and(|path| {
                    path != profile.executable_path
                        && classify_path(&profile, path) == PathPolicy::Excluded
                })
            })
            .map(|change| change.identity)
            .collect();
        self.session_mut().profile_snapshot = profile;
        for identity in excluded {
            if identity != target {
                self.restore_process(identity);
            }
        }
        self.persist()?;
        Ok(self.status())
    }

    /// Restore one process immediately (used when the user excludes it).
    pub fn restore_process(&mut self, identity: ProcessIdentity) -> Option<ProcessOutcome> {
        let change = self
            .session
            .as_ref()?
            .changes
            .iter()
            .find(|change| change.identity == identity)
            .cloned()?;
        let (outcome, complete) = self.restore_change(&change);
        if complete && let Some(session) = self.session.as_mut() {
            session.changes.retain(|item| item.identity != identity);
        }
        self.push_outcome(outcome.clone());
        let _ = self.persist();
        Some(outcome)
    }

    fn restore_all(&mut self) -> SessionStatus {
        let Some(session) = self.session.as_mut() else {
            return SessionStatus::default();
        };
        session.state = FocusState::Stopping;
        let changes = session.changes.clone();
        let mut remaining = Vec::new();
        let mut errors = Vec::new();
        for change in changes {
            let (outcome, complete) = self.restore_change(&change);
            if !matches!(outcome.outcome, ChangeOutcome::Applied) {
                errors.push(format!(
                    "{}: {}",
                    outcome.name,
                    outcome
                        .message
                        .clone()
                        .unwrap_or_else(|| format!("{:?}", outcome.outcome))
                ));
            }
            if !complete {
                remaining.push(change);
            }
            self.push_outcome(outcome);
        }
        let session = self.session_mut();
        session.changes = remaining;
        session.errors.extend(errors.clone());
        if session.changes.is_empty() {
            session.state = FocusState::Idle;
            if errors.is_empty() {
                session.errors.clear();
            }
            let _ = FocusJournal::remove(&self.journal_path);
        } else {
            session.state = FocusState::Error {
                message: errors
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "restore needs attention".into()),
            };
            let _ = self.persist();
        }
        self.status()
    }

    fn apply_target(
        &mut self,
        identity: ProcessIdentity,
        name: &str,
    ) -> Result<ProcessOutcome, ControllerError> {
        let current = match self.backend.read_priorities(identity) {
            Ok(value) => value,
            Err(error) => return Ok(outcome_for(identity, name, error)),
        };
        if current.cpu == crate::GAME_CPU_PRIORITY {
            return Ok(ProcessOutcome {
                identity,
                name: name.into(),
                outcome: ChangeOutcome::Applied,
                message: Some("already above normal".into()),
                partial: false,
            });
        }
        let change = AppliedChange {
            identity,
            executable_path: None,
            original_cpu_priority: Some(current.cpu),
            applied_cpu_priority: Some(crate::GAME_CPU_PRIORITY),
            original_memory_priority: None,
            applied_memory_priority: None,
        };
        // Write the intent before the Win32 call so a crash cannot lose recovery data.
        self.upsert_change(change)?;
        if let Err(error) = self
            .backend
            .set_cpu_priority(identity, crate::GAME_CPU_PRIORITY)
        {
            self.remove_change(identity)?;
            return Ok(outcome_for(identity, name, error));
        }
        Ok(ProcessOutcome {
            identity,
            name: name.into(),
            outcome: ChangeOutcome::Applied,
            message: None,
            partial: false,
        })
    }

    fn apply_background(
        &mut self,
        sample: &ProcessSample,
        name: &str,
        profile: &GameProfile,
    ) -> Result<ProcessOutcome, ControllerError> {
        let identity = sample.identity;
        let mut current = match self.backend.read_priorities(identity) {
            Ok(value) => value,
            Err(error) => return Ok(outcome_for(identity, name, error)),
        };
        let mut change = AppliedChange {
            identity,
            executable_path: sample.executable_path.clone(),
            original_cpu_priority: None,
            applied_cpu_priority: None,
            original_memory_priority: None,
            applied_memory_priority: None,
        };
        let mut messages = Vec::new();
        let mut applied = false;
        if current.cpu != crate::BACKGROUND_CPU_PRIORITY {
            change.original_cpu_priority = Some(current.cpu);
            change.applied_cpu_priority = Some(crate::BACKGROUND_CPU_PRIORITY);
            self.upsert_change(change.clone())?;
            match self
                .backend
                .set_cpu_priority(identity, crate::BACKGROUND_CPU_PRIORITY)
            {
                Ok(()) => {
                    current.cpu = crate::BACKGROUND_CPU_PRIORITY;
                    applied = true;
                }
                Err(error) => {
                    change.original_cpu_priority = None;
                    change.applied_cpu_priority = None;
                    self.remove_change(identity)?;
                    messages.push(format!("cpu: {error}"));
                }
            }
        }
        if profile.reduce_background_memory {
            match current.memory {
                Some(original) if original != crate::BACKGROUND_MEMORY_PRIORITY => {
                    let mut pending = change.clone();
                    pending.original_memory_priority = Some(original);
                    pending.applied_memory_priority = Some(crate::BACKGROUND_MEMORY_PRIORITY);
                    self.upsert_change(pending.clone())?;
                    match self
                        .backend
                        .set_memory_priority(identity, crate::BACKGROUND_MEMORY_PRIORITY)
                    {
                        Ok(()) => {
                            applied = true;
                        }
                        Err(error) => {
                            pending.original_memory_priority = None;
                            pending.applied_memory_priority = None;
                            if pending.applied_cpu_priority.is_some() {
                                self.upsert_change(pending)?;
                            } else {
                                self.remove_change(identity)?;
                            }
                            messages.push(format!("memory: {error}"));
                        }
                    }
                }
                None => messages.push("memory priority could not be read; skipped".into()),
                Some(_) => {}
            }
        }
        if applied {
            Ok(ProcessOutcome {
                identity,
                name: name.into(),
                outcome: ChangeOutcome::Applied,
                message: (!messages.is_empty()).then(|| messages.join("; ")),
                partial: !messages.is_empty(),
            })
        } else if messages.is_empty() {
            Ok(ProcessOutcome {
                identity,
                name: name.into(),
                outcome: ChangeOutcome::Skipped,
                message: Some("already at the configured priorities".into()),
                partial: false,
            })
        } else {
            Ok(ProcessOutcome {
                identity,
                name: name.into(),
                outcome: ChangeOutcome::AccessDenied,
                message: Some(messages.join("; ")),
                partial: false,
            })
        }
    }

    fn restore_change(&self, change: &AppliedChange) -> (ProcessOutcome, bool) {
        let name = change
            .executable_path
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("pid {}", change.identity.pid));
        let current = match self.backend.read_priorities(change.identity) {
            Ok(value) => value,
            Err(ControlError::Exited | ControlError::IdentityChanged) => {
                return (
                    ProcessOutcome {
                        identity: change.identity,
                        name,
                        outcome: ChangeOutcome::Exited,
                        message: Some(
                            "process exited or PID was reused; nothing to restore".into(),
                        ),
                        partial: false,
                    },
                    true,
                );
            }
            Err(error) => return (outcome_for(change.identity, &name, error), false),
        };
        let mut conflict = None;
        if let (Some(original), Some(applied)) =
            (change.original_cpu_priority, change.applied_cpu_priority)
        {
            if current.cpu == applied {
                if let Err(error) = self.backend.set_cpu_priority(change.identity, original) {
                    return (outcome_for(change.identity, &name, error), false);
                }
            } else if current.cpu != original {
                conflict = Some(format!(
                    "cpu priority changed outside Foqus (expected {applied:#x}, found {:#x})",
                    current.cpu
                ));
            }
        }
        if let (Some(original), Some(applied)) = (
            change.original_memory_priority,
            change.applied_memory_priority,
        ) {
            match current.memory {
                Some(value) if value == applied => {
                    if let Err(error) = self.backend.set_memory_priority(change.identity, original)
                    {
                        return (outcome_for(change.identity, &name, error), false);
                    }
                }
                Some(value) if value == original => {}
                Some(value) => {
                    conflict = Some(format!(
                        "memory priority changed outside Foqus (expected {applied}, found {value})"
                    ));
                }
                None => {
                    conflict = Some("memory priority could not be read during restore".into());
                }
            }
        }
        if let Some(message) = conflict {
            (
                ProcessOutcome {
                    identity: change.identity,
                    name,
                    outcome: ChangeOutcome::Conflict,
                    message: Some(message),
                    partial: false,
                },
                true,
            )
        } else {
            (
                ProcessOutcome {
                    identity: change.identity,
                    name,
                    outcome: ChangeOutcome::Applied,
                    message: Some("restored".into()),
                    partial: false,
                },
                true,
            )
        }
    }

    fn upsert_change(&mut self, change: AppliedChange) -> Result<(), ControllerError> {
        let session = self.session_mut();
        if let Some(existing) = session
            .changes
            .iter_mut()
            .find(|existing| existing.identity == change.identity)
        {
            *existing = change;
        } else {
            session.changes.push(change);
        }
        self.persist()
    }

    fn remove_change(&mut self, identity: ProcessIdentity) -> Result<(), ControllerError> {
        self.session_mut()
            .changes
            .retain(|change| change.identity != identity);
        self.persist()
    }

    fn push_outcome(&mut self, outcome: ProcessOutcome) {
        if let Some(session) = self.session.as_mut() {
            session.outcomes.push(outcome);
            if session.outcomes.len() > 200 {
                let excess = session.outcomes.len() - 200;
                session.outcomes.drain(0..excess);
            }
        }
    }

    fn session_mut(&mut self) -> &mut FocusSession {
        self.session.as_mut().expect("session exists during apply")
    }

    fn persist(&self) -> Result<(), ControllerError> {
        if let Some(session) = self.session.as_ref()
            && (!session.changes.is_empty()
                || !matches!(session.state, FocusState::Idle | FocusState::Error { .. }))
        {
            FocusJournal {
                schema_version: crate::JOURNAL_SCHEMA_VERSION,
                session: session.clone(),
            }
            .save_atomic(&self.journal_path)?;
        }
        Ok(())
    }
}

fn outcome_for(identity: ProcessIdentity, name: &str, error: ControlError) -> ProcessOutcome {
    let outcome = match error {
        ControlError::Exited => ChangeOutcome::Exited,
        ControlError::IdentityChanged => ChangeOutcome::Exited,
        ControlError::AccessDenied => ChangeOutcome::AccessDenied,
        ControlError::Unsupported(_) => ChangeOutcome::Unsupported,
        ControlError::Backend(_) => ChangeOutcome::RestoreFailed,
    };
    ProcessOutcome {
        identity,
        name: name.into(),
        outcome,
        message: Some(error.to_string()),
        partial: false,
    }
}

fn status_of(session: Option<&FocusSession>) -> SessionStatus {
    match session {
        Some(session) => SessionStatus {
            state: session.state.clone(),
            target: Some(session.target),
            profile_name: Some(session.profile_snapshot.name.clone()),
            outcomes: session.outcomes.clone(),
            errors: session.errors.clone(),
            applied_changes: session.changes.len(),
        },
        None => SessionStatus::default(),
    }
}

pub struct FocusController<B: ProcessControlBackend> {
    backend: B,
    journal_path: PathBuf,
    session: Option<FocusSession>,
    last_scan: Option<Instant>,
    scan_backoff: HashMap<ProcessIdentity, Instant>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProcessCapability, ProcessPriorities, ProcessState};
    use std::{collections::HashMap, sync::Mutex};

    struct MockBackend {
        priorities: Mutex<HashMap<ProcessIdentity, ProcessPriorities>>,
        processes: Vec<ProcessSample>,
        fail_cpu: Mutex<Vec<ProcessIdentity>>,
        reused: Mutex<Vec<ProcessIdentity>>,
    }

    impl MockBackend {
        fn new(priorities: &[(ProcessIdentity, u32)], processes: Vec<ProcessSample>) -> Self {
            Self {
                priorities: Mutex::new(
                    priorities
                        .iter()
                        .map(|(identity, cpu)| {
                            (
                                *identity,
                                ProcessPriorities {
                                    cpu: *cpu,
                                    memory: Some(5),
                                },
                            )
                        })
                        .collect(),
                ),
                processes,
                fail_cpu: Mutex::new(vec![]),
                reused: Mutex::new(vec![]),
            }
        }
    }

    impl ProcessControlBackend for MockBackend {
        fn list_processes(&self) -> Vec<ProcessSample> {
            self.processes.clone()
        }

        fn read_priorities(
            &self,
            identity: ProcessIdentity,
        ) -> Result<ProcessPriorities, ControlError> {
            if self.reused.lock().unwrap().contains(&identity) {
                return Err(ControlError::IdentityChanged);
            }
            self.priorities
                .lock()
                .unwrap()
                .get(&identity)
                .copied()
                .ok_or(ControlError::Exited)
        }

        fn set_cpu_priority(
            &self,
            identity: ProcessIdentity,
            priority: u32,
        ) -> Result<(), ControlError> {
            if self.fail_cpu.lock().unwrap().contains(&identity) {
                return Err(ControlError::AccessDenied);
            }
            let mut priorities = self.priorities.lock().unwrap();
            let value = priorities.get_mut(&identity).ok_or(ControlError::Exited)?;
            value.cpu = priority;
            Ok(())
        }

        fn set_memory_priority(
            &self,
            identity: ProcessIdentity,
            priority: u8,
        ) -> Result<(), ControlError> {
            let mut priorities = self.priorities.lock().unwrap();
            let value = priorities.get_mut(&identity).ok_or(ControlError::Exited)?;
            value.memory = Some(priority);
            Ok(())
        }
    }

    fn identity(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            creation_time_100ns: 100 + u64::from(pid),
        }
    }

    fn sample(pid: u32, path: &str) -> ProcessSample {
        ProcessSample {
            identity: identity(pid),
            name: path.rsplit('\\').next().unwrap_or(path).into(),
            executable_path: Some(PathBuf::from(path)),
            parent_pid: None,
            session_id: Some(1),
            cpu_percent: None,
            working_set_bytes: None,
            private_bytes: None,
            capability: ProcessCapability::Candidate,
            state: ProcessState::Running,
        }
    }

    fn profile() -> GameProfile {
        GameProfile {
            id: "p".into(),
            name: "Game".into(),
            executable_path: PathBuf::from(r"C:\Games\game.exe"),
            game_priority: Default::default(),
            reduce_background_memory: true,
            managed_executables: vec![
                PathBuf::from(r"C:\Apps\managed.exe"),
                PathBuf::from(r"C:\Apps\excluded.exe"),
            ],
            excluded_executables: vec![PathBuf::from(r"C:\Apps\excluded.exe")],
            restore_when_game_exits: true,
        }
    }

    fn controller() -> (FocusController<MockBackend>, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "foqus-test-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let backend = MockBackend::new(
            &[
                (identity(1), 0x20),
                (identity(2), 0x20),
                (identity(3), 0x20),
            ],
            vec![
                sample(1, r"C:\Games\game.exe"),
                sample(2, r"C:\Apps\managed.exe"),
                sample(3, r"C:\Apps\excluded.exe"),
            ],
        );
        (FocusController::new(backend, path.clone()), path)
    }

    #[test]
    fn start_applies_game_and_managed_but_not_excluded() {
        let (mut controller, path) = controller();
        let status = controller.start(identity(1), profile()).unwrap();
        assert!(matches!(status.state, FocusState::Active { .. }));
        let values = controller.backend.priorities.lock().unwrap();
        assert_eq!(values[&identity(1)].cpu, crate::GAME_CPU_PRIORITY);
        assert_eq!(values[&identity(2)].cpu, crate::BACKGROUND_CPU_PRIORITY);
        assert_eq!(values[&identity(3)].cpu, 0x20);
        assert_eq!(
            values[&identity(2)].memory,
            Some(crate::BACKGROUND_MEMORY_PRIORITY)
        );
        assert_eq!(
            controller
                .session()
                .unwrap()
                .outcomes
                .iter()
                .find(|outcome| outcome.identity == identity(3))
                .unwrap()
                .outcome,
            ChangeOutcome::Excluded
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn excluded_wins_over_managed_and_identity_is_lossless() {
        let profile = profile();
        assert_eq!(
            classify_path(&profile, Path::new(r"C:\Apps\excluded.exe")),
            PathPolicy::Excluded
        );
        let parsed = ProcessIdentity::from_ui(42, "18446744073709551615").unwrap();
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.creation_time_100ns, u64::MAX);
        assert!(ProcessIdentity::from_ui(42, "not-a-number").is_err());
        let json = serde_json::to_string(&parsed).unwrap();
        assert!(json.contains("\"18446744073709551615\""));
        assert_eq!(
            serde_json::from_str::<ProcessIdentity>(&json).unwrap(),
            parsed
        );
    }

    #[test]
    fn background_failure_is_partial_and_keeps_session_active() {
        let (mut controller, path) = controller();
        controller
            .backend
            .fail_cpu
            .lock()
            .unwrap()
            .push(identity(2));
        let status = controller.start(identity(1), profile()).unwrap();
        assert!(matches!(status.state, FocusState::Active { .. }));
        assert!(
            controller
                .session()
                .unwrap()
                .outcomes
                .iter()
                .any(|outcome| outcome.identity == identity(2)
                    && outcome.outcome == ChangeOutcome::Applied
                    && outcome
                        .message
                        .as_deref()
                        .is_some_and(|message| message.contains("cpu")))
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn game_exit_tick_restores_managed_processes() {
        let (mut controller, path) = controller();
        controller.start(identity(1), profile()).unwrap();
        controller
            .backend
            .priorities
            .lock()
            .unwrap()
            .remove(&identity(1));
        let status = controller.tick();
        assert!(matches!(status.state, FocusState::Idle));
        assert_eq!(
            controller.backend.priorities.lock().unwrap()[&identity(2)].cpu,
            0x20
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stop_restores_only_values_foqus_applied() {
        let (mut controller, path) = controller();
        controller.start(identity(1), profile()).unwrap();
        let status = controller.stop();
        assert!(matches!(status.state, FocusState::Idle));
        let values = controller.backend.priorities.lock().unwrap();
        assert_eq!(values[&identity(1)].cpu, 0x20);
        assert_eq!(values[&identity(2)].cpu, 0x20);
        assert_eq!(values[&identity(2)].memory, Some(5));
        assert!(!path.exists());
    }

    #[test]
    fn stop_reports_external_change_as_conflict() {
        let (mut controller, path) = controller();
        controller.start(identity(1), profile()).unwrap();
        controller
            .backend
            .priorities
            .lock()
            .unwrap()
            .get_mut(&identity(2))
            .unwrap()
            .cpu = 0x0000_0040;
        let status = controller.stop();
        assert!(matches!(status.state, FocusState::Idle));
        assert_eq!(
            controller
                .session()
                .unwrap()
                .outcomes
                .iter()
                .filter(|outcome| outcome.outcome == ChangeOutcome::Conflict)
                .count(),
            1
        );
        assert_eq!(controller.session().unwrap().changes.len(), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn target_failure_rolls_back_and_does_not_go_active() {
        let (mut controller, path) = controller();
        controller
            .backend
            .fail_cpu
            .lock()
            .unwrap()
            .push(identity(1));
        let status = controller.start(identity(1), profile()).unwrap();
        assert!(matches!(status.state, FocusState::Error { .. }));
        assert_eq!(
            controller.backend.priorities.lock().unwrap()[&identity(2)].cpu,
            0x20
        );
        assert!(controller.session().unwrap().changes.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recovery_journal_restores_after_restart() {
        let (mut controller, path) = controller();
        controller.start(identity(1), profile()).unwrap();
        drop(controller);
        let backend = MockBackend::new(
            &[
                (identity(1), crate::GAME_CPU_PRIORITY),
                (identity(2), crate::BACKGROUND_CPU_PRIORITY),
            ],
            vec![],
        );
        let mut restarted = FocusController::new(backend, path.clone());
        let status = restarted.recover_if_needed().unwrap().unwrap();
        assert!(matches!(status.state, FocusState::Recovery));
        assert!(matches!(restarted.retry_restore().state, FocusState::Idle));
        let values = restarted.backend.priorities.lock().unwrap();
        assert_eq!(values[&identity(1)].cpu, 0x20);
        assert_eq!(values[&identity(2)].cpu, 0x20);
    }

    #[test]
    fn reused_pid_is_skipped_without_touching_new_process() {
        let (mut controller, path) = controller();
        controller.start(identity(1), profile()).unwrap();
        controller.backend.reused.lock().unwrap().push(identity(2));
        let status = controller.stop();
        assert!(matches!(status.state, FocusState::Idle));
        assert!(
            controller
                .session()
                .unwrap()
                .outcomes
                .iter()
                .any(|outcome| outcome.identity == identity(2)
                    && outcome.outcome == ChangeOutcome::Exited)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn excluding_active_process_restores_it_immediately() {
        let (mut controller, path) = controller();
        controller.start(identity(1), profile()).unwrap();
        let mut changed = profile();
        changed
            .excluded_executables
            .push(PathBuf::from(r"C:\Apps\managed.exe"));
        let status = controller.update_profile(changed).unwrap();
        assert!(matches!(status.state, FocusState::Active { .. }));
        assert_eq!(
            controller.backend.priorities.lock().unwrap()[&identity(2)].cpu,
            0x20
        );
        let _ = std::fs::remove_file(path);
    }
}
