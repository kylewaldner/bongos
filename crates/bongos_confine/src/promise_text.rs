//! What bongos promises about job confinement on the running platform, shown at the top of the
//! Confinement settings section.

/// The platform bongos runs on, as far as confinement is concerned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    Linux,
    MacOs,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Platform::Linux
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Other
        }
    }
}

/// Whether the platform's sandbox (Bubblewrap on Linux, Seatbelt on macOS) can run here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxAvailability {
    Available,
    Unavailable,
}

/// The promise shown to the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Promise {
    pub title: &'static str,
    pub body: &'static str,
}

const LINUX: Promise = Promise {
    title: "Jobs are confined",
    body: "Jobs (workflow programs, agent steps and task checks) can write only to their job \
           tree, their scratch directory and the directories they declare. They never get your \
           repository's shared git data: bongos brings back their commits and branches after \
           each run and checks them. Git run by bongos and by the editor never uses \
           configuration or pointers a job wrote.",
};

const MACOS: Promise = Promise {
    title: "Jobs are confined by path",
    body: "Jobs can write only to their job tree, their scratch directory and the directories \
           they declare; macOS enforces this by path. Reads are not restricted, and a process a \
           job leaves running may outlive it, so what keeps job-written files from running code \
           on your machine is bongos's import and git handling rather than an isolated view of \
           the disk.",
};

const UNCONFINED: Promise = Promise {
    title: "Jobs are not confined",
    body: "No sandbox is available here, so bongos promises no confinement for jobs. Install \
           Bubblewrap (Linux) to get it. Whether a job may run without a sandbox is decided by \
           the workflow sandbox setting.",
};

/// The promise for a platform and whether its sandbox is available.
pub fn promise(platform: Platform, availability: SandboxAvailability) -> Promise {
    match (platform, availability) {
        (Platform::Linux, SandboxAvailability::Available) => LINUX,
        (Platform::MacOs, SandboxAvailability::Available) => MACOS,
        _ => UNCONFINED,
    }
}

/// The notes added below the promise for each confinement switch that is off.
pub fn switch_off_notes(agent_steps: bool, task_checks: bool) -> Vec<&'static str> {
    let mut notes = Vec::new();
    if !agent_steps {
        notes.push("Agent steps are not confined (confinement.agent_steps is off)");
    }
    if !task_checks {
        notes.push("Task checks are not confined (confinement.task_checks is off)");
    }
    notes
}
