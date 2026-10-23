//! Bounded change hints. Reconciliation remains the authority for source bytes.
use crate::contracts::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::{Duration, Instant},
};

pub(crate) const SAFETY_INTERVAL_MS: u64 = 60_000;
pub(crate) const POLL_MS: u64 = 250;
const MAX_TARGETS: usize = 512;
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub id: String,
    pub root: PathBuf,
    pub excluded_paths: Vec<String>,
}

#[derive(Default)]
pub(crate) struct Watches {
    configured: BTreeMap<String, Config>,
    roots: BTreeMap<String, native::Root>,
    retry: BTreeMap<String, Instant>,
    errors: BTreeMap<String, String>,
    native: native::Queue,
    events: u64,
}
impl Watches {
    pub(crate) fn sync(&mut self, configs: &[Config]) -> BTreeSet<String> {
        let mut dirty = match self.native.poll(&self.roots) {
            Ok((dirty, count)) => {
                self.events = self.events.saturating_add(count);
                dirty
            }
            Err(error) => {
                let dirty = self.configured.keys().cloned().collect();
                for id in self.configured.keys() {
                    self.errors.insert(id.clone(), error.code.clone());
                }
                self.roots.clear();
                self.retry.clear();
                self.native = native::Queue::default();
                dirty
            }
        };
        let fresh: BTreeMap<_, _> = configs.iter().map(|c| (c.id.clone(), c.clone())).collect();
        self.roots.retain(|id, _| fresh.contains_key(id));
        self.errors.retain(|id, _| fresh.contains_key(id));
        self.retry.retain(|id, _| fresh.contains_key(id));
        for config in configs {
            let changed = self.configured.get(&config.id) != Some(config);
            if changed {
                dirty.insert(config.id.clone());
                self.retry.remove(&config.id);
            }
            if changed || dirty.contains(&config.id) {
                self.roots.remove(&config.id);
            }
            if self.roots.contains_key(&config.id)
                || self
                    .retry
                    .get(&config.id)
                    .is_some_and(|at| *at > Instant::now())
            {
                continue;
            }
            let available = MAX_TARGETS - self.roots.values().map(native::Root::len).sum::<usize>();
            match self.native.subscribe(config, available) {
                Ok(root) => {
                    self.roots.insert(config.id.clone(), root);
                    self.retry.remove(&config.id);
                    self.errors.remove(&config.id);
                    dirty.insert(config.id.clone());
                }
                Err(error) => {
                    self.errors.insert(config.id.clone(), error.code);
                    self.retry
                        .insert(config.id.clone(), Instant::now() + Duration::from_secs(1));
                }
            }
        }
        self.configured = fresh;
        dirty.retain(|id| self.configured.contains_key(id));
        if configs.is_empty() {
            self.native = native::Queue::default();
        }
        dirty
    }
    pub(crate) fn status(&self) -> Value {
        let collections:BTreeMap<_,_>=self.configured.keys().map(|id|(id,json!({"active":self.roots.contains_key(id),"watchedTargets":self.roots.get(id).map_or(0,native::Root::len),"lastErrorCode":self.errors.get(id)}))).collect();
        json!({"backend":native::BACKEND,"maxTargets":MAX_TARGETS,"pollIntervalMs":POLL_MS,"safetyScanMaxIntervalMs":SAFETY_INTERVAL_MS,"observedEvents":self.events,"collections":collections})
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::{
        fs::{File, OpenOptions},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::OpenOptionsExt},
        },
        path::Path,
    };
    pub(super) const BACKEND: &str = "kqueue-vnode";
    struct Target {
        file: File,
        token: usize,
    }
    pub(super) struct Root {
        targets: Vec<Target>,
    }
    impl Root {
        pub(super) fn len(&self) -> usize {
            self.targets.len()
        }
    }
    #[derive(Default)]
    pub(super) struct Queue {
        queue: Option<File>,
        next: usize,
    }
    impl Queue {
        pub(super) fn subscribe(&mut self, config: &Config, budget: usize) -> Result<Root> {
            if budget == 0 {
                return Err(Error::new(
                    "resource_exhausted",
                    "Filesystem watch target budget exhausted",
                ));
            }
            if config.root.canonicalize()? != config.root {
                return Err(Error::new("source_unavailable", "Watch root changed"));
            }
            if self.queue.is_none() {
                // SAFETY: the new queue is owned here and never changes another process's state.
                let fd = unsafe { libc::kqueue() };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                let file = unsafe { File::from_raw_fd(fd) };
                if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                self.queue = Some(file);
            }
            let root = OpenOptions::new()
                .read(true)
                .custom_flags(
                    libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
                .open(&config.root)?;
            let mut result = Root {
                targets: Vec::new(),
            };
            self.add(&mut result, root)?;
            let policy = CollectionPolicy {
                excluded_paths: config.excluded_paths.clone(),
                ..Default::default()
            };
            let walker = walkdir::WalkDir::new(&config.root)
                .follow_links(false)
                .follow_root_links(false)
                .max_open(1)
                .into_iter()
                .filter_entry(|entry| {
                    let relative = entry
                        .path()
                        .strip_prefix(&config.root)
                        .ok()
                        .and_then(Path::to_str)
                        .unwrap_or("");
                    !policy.excludes(relative)
                        && (!entry.file_type().is_dir()
                            || relative.is_empty()
                            || !matches!(
                                entry.file_name().to_str(),
                                Some(
                                    ".git"
                                        | "node_modules"
                                        | "target"
                                        | "dist"
                                        | "build"
                                        | ".private"
                                )
                            ))
                });
            for entry in walker {
                let entry = entry.map_err(|_| {
                    Error::new("source_unavailable", "Watch traversal is incomplete")
                })?;
                if entry.depth() == 0 {
                    continue;
                }
                let directory = entry.file_type().is_dir();
                let supported = entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| {
                            matches!(s.to_ascii_lowercase().as_str(), "md" | "markdown" | "txt")
                        });
                if !directory && !supported {
                    continue;
                }
                if result.len() >= budget {
                    return Err(Error::new(
                        "resource_exhausted",
                        "Filesystem watch target budget exhausted",
                    ));
                }
                let relative = entry
                    .path()
                    .strip_prefix(&config.root)
                    .map_err(|_| Error::new("source_unavailable", "Watch path changed"))?;
                let file = open_relative(&result.targets[0].file, relative, directory)?;
                self.add(&mut result, file)?;
            }
            Ok(result)
        }
        fn add(&mut self, root: &mut Root, file: File) -> Result<()> {
            self.next = self.next.checked_add(1).ok_or_else(|| {
                Error::new("resource_exhausted", "Watch identity capacity exhausted")
            })?;
            let token = self.next;
            let change = libc::kevent {
                ident: file.as_raw_fd() as _,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: libc::NOTE_WRITE
                    | libc::NOTE_EXTEND
                    | libc::NOTE_RENAME
                    | libc::NOTE_DELETE
                    | libc::NOTE_REVOKE,
                data: 0,
                udata: token as *mut libc::c_void,
            };
            if unsafe {
                libc::kevent(
                    self.queue.as_ref().unwrap().as_raw_fd(),
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            root.targets.push(Target { file, token });
            Ok(())
        }
        pub(super) fn poll(
            &mut self,
            roots: &BTreeMap<String, Root>,
        ) -> Result<(BTreeSet<String>, u64)> {
            let targets: BTreeMap<_, _> = roots
                .iter()
                .flat_map(|(id, root)| root.targets.iter().map(move |target| (target.token, id)))
                .collect();
            let mut dirty = BTreeSet::new();
            let mut count = 0;
            let Some(queue) = &self.queue else {
                return Ok((dirty, count));
            };
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            for _ in 0..8 {
                let mut events: [libc::kevent; 64] = unsafe { std::mem::zeroed() };
                let n = unsafe {
                    libc::kevent(
                        queue.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        events.as_mut_ptr(),
                        64,
                        &timeout,
                    )
                };
                if n < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        break;
                    }
                    return Err(error.into());
                }
                for event in &events[..n as usize] {
                    if event.flags & libc::EV_ERROR != 0 {
                        return Err(Error::new("worker_failed", "Filesystem watch queue failed"));
                    }
                    if let Some(id) = targets.get(&(event.udata as usize)) {
                        dirty.insert((**id).clone());
                        count += 1;
                    }
                }
                if n < 64 {
                    break;
                }
            }
            Ok((dirty, count))
        }
    }
    fn open_relative(root: &File, relative: &Path, directory: bool) -> Result<File> {
        let components: Vec<_> = relative.components().collect();
        let mut current = root.try_clone()?;
        for (index, component) in components.iter().enumerate() {
            let std::path::Component::Normal(name) = component else {
                return Err(Error::new("source_unavailable", "Invalid watch path"));
            };
            let name = std::ffi::CString::new(name.as_bytes())
                .map_err(|_| Error::new("source_unavailable", "Invalid watch path"))?;
            let final_component = index + 1 == components.len();
            let flags = libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC
                | if final_component && !directory {
                    libc::O_EVTONLY
                } else {
                    libc::O_RDONLY | libc::O_DIRECTORY
                };
            let fd = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            current = unsafe { File::from_raw_fd(fd) };
        }
        let metadata = current.metadata()?;
        if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
            return Err(Error::new("source_unavailable", "Watch target changed"));
        }
        Ok(current)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn native_capacity_failure_closes_partial_targets_and_exclusions_allow_recovery() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        fs::write(root.join("guide.md"), "policy\n").unwrap();
        fs::create_dir(root.join("bulk")).unwrap();
        for index in 0..MAX_TARGETS {
            fs::write(root.join(format!("bulk/{index}.md")), "fixture\n").unwrap();
        }
        let mut watches = Watches::default();
        let mut config = Config {
            id: "fixture".into(),
            root,
            excluded_paths: vec![],
        };
        watches.sync(&[config.clone()]);
        assert_eq!(watches.status()["collections"]["fixture"]["active"], false);
        assert_eq!(
            watches.status()["collections"]["fixture"]["watchedTargets"],
            0
        );
        assert_eq!(
            watches.status()["collections"]["fixture"]["lastErrorCode"],
            "resource_exhausted"
        );
        config.excluded_paths = vec!["bulk".into()];
        watches.sync(&[config.clone()]);
        assert_eq!(watches.status()["collections"]["fixture"]["active"], true);
        assert_eq!(
            watches.status()["collections"]["fixture"]["watchedTargets"],
            2
        );
        assert!(watches.sync(&[config.clone()]).is_empty());
        fs::write(config.root.join("bulk/0.md"), "excluded change\n").unwrap();
        assert!(watches.sync(&[config.clone()]).is_empty());
        fs::write(config.root.join("guide.md"), "native change\n").unwrap();
        assert!(watches.sync(&[config]).contains("fixture"));
        assert!(watches.status()["observedEvents"].as_u64().unwrap() > 0);
        watches.sync(&[]);
        assert!(watches.roots.is_empty());
        assert!(watches.configured.is_empty());
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::*;
    pub(super) const BACKEND: &str = "unavailable";
    pub(super) struct Root;
    impl Root {
        pub(super) fn len(&self) -> usize {
            0
        }
    }
    #[derive(Default)]
    pub(super) struct Queue;
    impl Queue {
        pub(super) fn subscribe(&mut self, _: &Config, _: usize) -> Result<Root> {
            Err(Error::new(
                "unsupported_capability",
                "Native watching is available only on macOS",
            ))
        }
        pub(super) fn poll(
            &mut self,
            _: &BTreeMap<String, Root>,
        ) -> Result<(BTreeSet<String>, u64)> {
            Ok((BTreeSet::new(), 0))
        }
    }
}
