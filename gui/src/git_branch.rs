//! Bounded, cached Git status lookups for workspace sidebar rows.
//!
//! Git is intentionally the only sidebar detail that is not supplied by the
//! cmux protocol. All process work stays on this module's worker thread and
//! results rejoin the normal stamped UI update channel.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::session::{StampedUpdate, Update};

pub const CACHE_TTL: Duration = Duration::from_secs(5);
const GIT_TIMEOUT: Duration = Duration::from_secs(1);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const TEMP_FILE_ATTEMPTS: u64 = 64;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct Request {
    directory: PathBuf,
    generation: u64,
}

struct CacheEntry {
    branch: Option<String>,
    checked_at: Instant,
    delivered_generation: Option<u64>,
}

pub struct Resolver {
    requests: mpsc::Sender<Request>,
}

impl Resolver {
    pub fn request(&self, directory: &Path, generation: u64) {
        let _ = self.requests.send(Request {
            directory: directory.to_path_buf(),
            generation,
        });
    }
}

pub fn spawn(updates: async_channel::Sender<StampedUpdate>) -> Resolver {
    spawn_with_options(OsString::from("git"), GIT_TIMEOUT, CACHE_TTL, updates)
}

fn spawn_with_options(
    program: OsString,
    timeout: Duration,
    cache_ttl: Duration,
    updates: async_channel::Sender<StampedUpdate>,
) -> Resolver {
    let (requests, receiver) = mpsc::channel();
    thread::spawn(move || resolver_loop(receiver, updates, program, timeout, cache_ttl));
    Resolver { requests }
}

fn resolver_loop(
    receiver: mpsc::Receiver<Request>,
    updates: async_channel::Sender<StampedUpdate>,
    program: OsString,
    timeout: Duration,
    cache_ttl: Duration,
) {
    let mut cache = HashMap::<PathBuf, CacheEntry>::new();
    while let Ok(first) = receiver.recv() {
        let mut pending = HashMap::from([(first.directory.clone(), first)]);
        while let Ok(request) = receiver.try_recv() {
            pending.insert(request.directory.clone(), request);
        }

        for request in pending.into_values() {
            resolve_request(request, &updates, &program, timeout, cache_ttl, &mut cache);
        }
    }
}

fn resolve_request(
    request: Request,
    updates: &async_channel::Sender<StampedUpdate>,
    program: &OsStr,
    timeout: Duration,
    cache_ttl: Duration,
    cache: &mut HashMap<PathBuf, CacheEntry>,
) {
    let now = Instant::now();
    if let Some(entry) = cache.get_mut(&request.directory) {
        if now.duration_since(entry.checked_at) < cache_ttl {
            if entry.delivered_generation != Some(request.generation) {
                send_result(
                    updates,
                    request.generation,
                    request.directory,
                    entry.branch.clone(),
                );
                entry.delivered_generation = Some(request.generation);
            }
            return;
        }
    }

    let branch = query_git_branch_with(program, &request.directory, timeout);
    send_result(
        updates,
        request.generation,
        request.directory.clone(),
        branch.clone(),
    );
    cache.insert(
        request.directory,
        CacheEntry {
            branch,
            checked_at: Instant::now(),
            delivered_generation: Some(request.generation),
        },
    );
}

fn send_result(
    updates: &async_channel::Sender<StampedUpdate>,
    generation: u64,
    directory: PathBuf,
    branch: Option<String>,
) {
    let _ = updates.send_blocking(StampedUpdate {
        generation,
        update: Update::GitBranch { directory, branch },
    });
}

fn query_git_branch_with(program: &OsStr, directory: &Path, timeout: Duration) -> Option<String> {
    let mut output = OutputFile::new().ok()?;
    let child_output = output.file.try_clone().ok()?;
    let mut child = Command::new(program)
        .arg("-C")
        .arg(directory)
        .args(["status", "--porcelain=v2", "--branch"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_output))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let status = wait_with_timeout(&mut child, timeout)?;
    if !status.success() {
        return None;
    }

    output.file.seek(SeekFrom::Start(0)).ok()?;
    parse_git_status(BufReader::new(&output.file)).ok()?
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_and_reap(child);
                return None;
            }
            Ok(None) => {
                thread::sleep(
                    WAIT_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(_) => {
                kill_and_reap(child);
                return None;
            }
        }
    }
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn parse_git_status(reader: impl BufRead) -> io::Result<Option<String>> {
    let mut head = None;
    let mut oid = None;
    let mut dirty = false;

    for line in reader.lines() {
        let line = line?;
        if let Some(value) = line.strip_prefix("# branch.head ") {
            head = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("# branch.oid ") {
            oid = Some(value.to_string());
        } else if !line.is_empty() && !line.starts_with("# ") {
            dirty = true;
        }
    }

    let mut branch = match head.as_deref() {
        Some("(detached)") => {
            let short = oid.unwrap_or_default().chars().take(7).collect::<String>();
            if short.is_empty() {
                return Ok(None);
            }
            short
        }
        Some("(unknown)") | None => return Ok(None),
        Some(branch) => branch.to_string(),
    };
    if dirty {
        branch.push('*');
    }
    Ok(Some(branch))
}

struct OutputFile {
    path: PathBuf,
    file: File,
}

impl OutputFile {
    fn new() -> io::Result<Self> {
        for _ in 0..TEMP_FILE_ATTEMPTS {
            let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "cmux-gtk-git-{}-{sequence}.status",
                std::process::id()
            ));
            match OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path)
            {
                Ok(file) => return Ok(Self { path, file }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate Git status output file",
        ))
    }
}

impl Drop for OutputFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "cmux-gtk-git-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        #[cfg(unix)]
        fn script(&self, name: &str, body: &str, mode: u32) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_clean_and_dirty_branch_names() {
        let clean = b"# branch.oid 0123456789abcdef\n# branch.head main\n";
        let dirty = b"# branch.oid 0123456789abcdef\n# branch.head topic\n? new.txt\n";

        assert_eq!(
            parse_git_status(Cursor::new(clean)).unwrap(),
            Some("main".into())
        );
        assert_eq!(
            parse_git_status(Cursor::new(dirty)).unwrap(),
            Some("topic*".into())
        );
    }

    #[test]
    fn detached_head_uses_short_commit_and_dirty_suffix() {
        let status = b"# branch.oid 0123456789abcdef\n# branch.head (detached)\n1 .M N... file\n";

        assert_eq!(
            parse_git_status(Cursor::new(status)).unwrap(),
            Some("0123456*".into())
        );
    }

    #[test]
    fn non_repository_returns_no_branch() {
        let directory = TestDirectory::new();

        assert_eq!(
            query_git_branch_with(OsStr::new("git"), directory.path(), GIT_TIMEOUT),
            None
        );
    }

    #[test]
    fn missing_git_returns_no_branch() {
        let directory = TestDirectory::new();
        let missing = directory.path().join("missing-git");

        assert_eq!(
            query_git_branch_with(&missing.into_os_string(), directory.path(), GIT_TIMEOUT),
            None
        );
    }

    #[test]
    fn missing_cwd_returns_no_branch() {
        let directory = TestDirectory::new();
        let missing = directory.path().join("gone");

        assert_eq!(
            query_git_branch_with(OsStr::new("git"), &missing, GIT_TIMEOUT),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied_returns_no_branch() {
        let directory = TestDirectory::new();
        let program = directory.script("git-denied", "exit 0", 0o600);

        assert_eq!(
            query_git_branch_with(&program.into_os_string(), directory.path(), GIT_TIMEOUT),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_returns_no_branch() {
        let directory = TestDirectory::new();
        let program = directory.script("git-fails", "exit 7", 0o700);

        assert_eq!(
            query_git_branch_with(&program.into_os_string(), directory.path(), GIT_TIMEOUT),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_and_reaps_child() {
        let directory = TestDirectory::new();
        let pid_file = directory.path().join("pid");
        let program = directory.script(
            "git-hangs",
            &format!(
                "echo $$ > {}\nwhile :; do :; done",
                pid_file.to_string_lossy()
            ),
            0o700,
        );
        let started = Instant::now();

        // Long enough that the script reliably reaches its `echo` even on a
        // loaded machine, while still far below the assertion below.
        assert_eq!(
            query_git_branch_with(
                &program.into_os_string(),
                directory.path(),
                Duration::from_millis(300),
            ),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(2));

        // The shell may still have been between spawning and writing its pid, so
        // an absent file means there is simply nothing left to assert about.
        let Ok(pid) = fs::read_to_string(&pid_file) else {
            return;
        };
        let pid = pid.trim();
        if pid.is_empty() {
            return;
        }
        // Killing is not reaping: the kernel keeps /proc/<pid> until the parent
        // collects the status, so poll rather than reading it once.
        let entry = Path::new("/proc").join(pid);
        let deadline = Instant::now() + Duration::from_secs(2);
        while entry.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!entry.exists(), "child {pid} outlived its timeout");
    }

    #[cfg(unix)]
    #[test]
    fn resolver_caches_each_directory_and_redelivers_for_a_new_generation() {
        let directory = TestDirectory::new();
        let count_file = directory.path().join("count");
        let program = directory.script(
            "git-counts",
            &format!(
                "echo run >> {}\nprintf '# branch.oid 0123456789abcdef\\n# branch.head main\\n'",
                count_file.to_string_lossy()
            ),
            0o700,
        );
        let (updates, receiver) = async_channel::unbounded();
        let resolver = spawn_with_options(
            program.into_os_string(),
            GIT_TIMEOUT,
            Duration::from_secs(60),
            updates,
        );

        resolver.request(directory.path(), 4);
        let first = receiver.recv_blocking().unwrap();
        assert!(matches!(
            first.update,
            Update::GitBranch {
                branch: Some(ref branch),
                ..
            } if branch == "main"
        ));

        resolver.request(directory.path(), 4);
        thread::sleep(Duration::from_millis(30));
        assert!(receiver.try_recv().is_err());
        assert_eq!(fs::read_to_string(&count_file).unwrap().lines().count(), 1);

        resolver.request(directory.path(), 5);
        assert_eq!(receiver.recv_blocking().unwrap().generation, 5);
        assert_eq!(fs::read_to_string(count_file).unwrap().lines().count(), 1);
    }
}
