//! DigUp's helper processes, which do all the work: `digup search-service` answers the panel (a JSON request per line
//! in, the reply on a line out with the request's id; lines without an id are events), and `digup index-service` keeps
//! the index current (commands in, events out) and exits when it's done. Sources/digup has both protocols. Each one's
//! output is read on a thread of its own, which waits for the process when its output ends, so none is left a zombie.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

type EventSink = Arc<dyn Fn(Value) + Send + Sync>;
/// Sender and whether its reply resets the interactive idle clock (index maintenance must not).
type PendingReplies = Arc<Mutex<HashMap<u64, (oneshot::Sender<Value>, bool)>>>;

/// How a helper is started.
#[derive(Clone)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Where its standard error goes (llama.cpp and the helper write their warnings there).
    pub log: Option<PathBuf>,
    /// Work in the background (indexing, the download): below the person's own programs for the processor.
    pub background: bool,
}

impl Launch {
    fn spawn(&self) -> Result<(Child, ChildStdin, ChildStdout), String> {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(match self.log.as_ref().and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok()) {
                Some(file) => Stdio::from(file),
                None => Stdio::null(),
            });
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW (no console flashes up), and BELOW_NORMAL_PRIORITY_CLASS for background work.
            command.creation_flags(0x0800_0000 | if self.background { 0x0000_4000 } else { 0 });
        }
        #[cfg(unix)]
        if self.background {
            use std::os::unix::process::CommandExt;
            // SAFETY: nice() only changes the new process's own priority, between fork and exec.
            unsafe {
                command.pre_exec(|| {
                    libc::nice(10);
                    Ok(())
                });
            }
        }
        let mut child = command.spawn().map_err(|error| format!("can't start {}: {error}", self.program.display()))?;
        #[cfg(windows)]
        job::add(&child);
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        Ok((child, stdin, stdout))
    }
}

/// A process started apart from the helpers here (the speed check, folder estimates): on Windows it joins their job, so it
/// ends with DigUp, and with them before an update's installer runs.
pub fn adopt(child: &Child) {
    #[cfg(windows)]
    job::add(child);
    #[cfg(not(windows))]
    let _ = child;
}

/// Before an update's installer replaces the helper's files (Windows, where a running program's files can't be
/// replaced): every process in the job ends, and is gone, within five seconds.
pub fn end_all() {
    #[cfg(windows)]
    job::end_all();
}

/// Asks the process to stop (`farewell` on stdin, then stdin closed), and ends it if it hasn't within two seconds.
fn stop(mut child: Child, mut stdin: ChildStdin, farewell: &str) {
    let _ = writeln!(stdin, "{farewell}").and_then(|_| stdin.flush());
    drop(stdin);
    for _ in 0..40 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Each helper started gets a number, so the thread reading it knows whether its slot still holds it or a newer one.
static STARTED: AtomicU64 = AtomicU64::new(1);

/// A helper whose output ended is on its way out: waits for it (up to five seconds) and gives its exit code.
fn reap(child: &mut Child) -> Option<i32> {
    for _ in 0..100 {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    None
}

/// `digup search-service`: started on the first request, and again after it died.
pub struct SearchService {
    launch: Launch,
    running: Arc<Mutex<Option<Searching>>>,
    next_id: AtomicU64,
    on_event: EventSink,
    folders: Mutex<Option<Value>>,
    closed: AtomicBool,
}

struct Searching {
    child: Child,
    stdin: ChildStdin,
    number: u64,
    /// Replies still to come, by request id. Each process has its own, so one that dies fails only its own requests.
    pending: PendingReplies,
    last_reply: Arc<Mutex<Instant>>,
}

impl SearchService {
    pub fn new(launch: Launch, on_event: impl Fn(Value) + Send + Sync + 'static) -> SearchService {
        SearchService {
            launch,
            running: Arc::default(),
            next_id: AtomicU64::new(1),
            on_event: Arc::new(on_event),
            folders: Mutex::new(None),
            closed: AtomicBool::new(false),
        }
    }

    /// Sends a request and waits for its reply.
    pub async fn ask(&self, mut request: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        request["id"] = json!(id);
        let reply = self.send(id, &request)?;
        let reply = reply.await.map_err(|_| "DigUp's helper stopped".to_string())?;
        match reply.get("ok") {
            Some(Value::Bool(false)) => Err(reply["error"].as_str().unwrap_or("the helper failed").to_string()),
            _ => Ok(reply),
        }
    }

    fn send(&self, id: u64, request: &Value) -> Result<oneshot::Receiver<Value>, String> {
        let mut running = self.running.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            return Err("DigUp is quitting".into());
        }
        let op = request["op"].as_str().unwrap_or_default();
        if op == "folders" {
            *self.folders.lock().unwrap() = Some(request.clone());
        }
        if let Some(searching) = running.as_mut() {
            if !matches!(searching.child.try_wait(), Ok(None)) {
                *running = None;
            }
        }
        if running.is_none() {
            // Background index events must not resurrect a sleeping search worker. A new worker reads the
            // current index itself; folder changes are replayed before its first request.
            if op == "reload" || op == "folders" {
                let (sender, receiver) = oneshot::channel();
                let _ = sender.send(json!({"ok": true}));
                return Ok(receiver);
            }
            *running = Some(self.start()?);
        }
        let searching = running.as_mut().unwrap();
        let (sender, receiver) = oneshot::channel();
        searching.pending.lock().unwrap().insert(id, (sender, op != "reload" && op != "folders"));
        if let Err(error) = writeln!(searching.stdin, "{request}").and_then(|_| searching.stdin.flush()) {
            searching.pending.lock().unwrap().remove(&id);
            *running = None;
            return Err(format!("DigUp's helper isn't answering: {error}"));
        }
        Ok(receiver)
    }

    fn start(&self) -> Result<Searching, String> {
        let (child, mut stdin, stdout) = self.launch.spawn()?;
        if let Some(mut folders) = self.folders.lock().unwrap().clone() {
            folders["id"] = json!(0);
            let _ = writeln!(stdin, "{folders}").and_then(|_| stdin.flush());
        }
        let number = STARTED.fetch_add(1, Ordering::Relaxed);
        let pending: PendingReplies = Arc::default();
        let (replies, on_event, slot) = (pending.clone(), self.on_event.clone(), self.running.clone());
        let last_reply = Arc::new(Mutex::new(Instant::now()));
        let replied = last_reply.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                match message.get("id").and_then(Value::as_u64) {
                    Some(id) => {
                        let mut replies = replies.lock().unwrap();
                        if let Some((sender, interactive)) = replies.remove(&id) {
                            if interactive { *replied.lock().unwrap() = Instant::now(); }
                            let _ = sender.send(message);
                        }
                    }
                    None => on_event(message),
                }
            }
            // Gone: its waiting requests fail (their senders drop here), and the next request starts another.
            replies.lock().unwrap().clear();
            let mut slot = slot.lock().unwrap();
            if slot.as_ref().is_some_and(|searching| searching.number == number) {
                if let Some(mut searching) = slot.take() {
                    reap(&mut searching.child);
                }
            }
            drop(slot);
            on_event(json!({"event": "exit"}));
        });
        Ok(Searching { child, stdin, number, pending, last_reply })
    }

    /// Called off the UI thread. Pending requests (including abandoned callers) pin the worker until its reply.
    /// Sending and retirement share the slot lock: a request either pins this worker or starts the next one.
    pub fn retire_idle(&self, timeout: Duration) -> bool {
        let retiring = {
            let mut running = self.running.lock().unwrap();
            let idle = running.as_ref().is_some_and(|searching| {
                let pending = searching.pending.lock().unwrap();
                pending.is_empty() && searching.last_reply.lock().unwrap().elapsed() >= timeout
            });
            if idle { running.take() } else { None }
        };
        if let Some(searching) = retiring {
            stop(searching.child, searching.stdin, &json!({"op": "quit"}).to_string());
            true
        } else {
            false
        }
    }

    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.stop();
    }

    pub fn stop(&self) {
        if let Some(searching) = self.running.lock().unwrap().take() {
            stop(searching.child, searching.stdin, &json!({"op": "quit"}).to_string());
        }
    }
}

#[cfg(all(test, unix))]
mod search_lifecycle_tests {
    use super::*;

    fn service() -> Arc<SearchService> {
        Arc::new(SearchService::new(Launch {
            program: "python3".into(),
            args: vec!["-u".into(), "-c".into(), r#"
import json, os, sys, time
folders = []
for line in sys.stdin:
    q = json.loads(line)
    if q['op'] == 'quit': break
    if q['op'] == 'folders': folders = q['roots']
    if q['op'] == 'slow': time.sleep(0.15)
    print(json.dumps({'id': q['id'], 'ok': True, 'pid': os.getpid(), 'roots': folders}), flush=True)
"#.into()],
            env: vec![], log: None, background: false,
        }, |_| {}))
    }

    #[test]
    fn retirement_reopens_with_latest_folders_and_shutdown_is_final() {
        tauri::async_runtime::block_on(async {
            let search = service();
            search.ask(json!({"op": "folders", "roots": ["new"]})).await.unwrap();
            search.ask(json!({"op": "reload"})).await.unwrap();
            assert!(search.running.lock().unwrap().is_none(), "background events must not start a worker");
            let first = search.ask(json!({"op": "status"})).await.unwrap();
            assert_eq!(first["roots"], json!(["new"]));
            assert!(!search.retire_idle(Duration::from_secs(60)), "a fresh reply cancels expiry");
            *search.running.lock().unwrap().as_ref().unwrap().last_reply.lock().unwrap() =
                Instant::now() - Duration::from_secs(60);
            search.ask(json!({"op": "reload"})).await.unwrap();
            assert!(search.retire_idle(Duration::from_secs(30)), "index reloads must not keep search warm");
            search.ask(json!({"op": "status"})).await.unwrap();
            assert!(search.retire_idle(Duration::ZERO));
            search.ask(json!({"op": "folders", "roots": ["latest"]})).await.unwrap();
            let second = search.ask(json!({"op": "status"})).await.unwrap();
            assert_ne!(first["pid"], second["pid"]);
            assert_eq!(second["roots"], json!(["latest"]));
            search.shutdown();
            assert!(search.ask(json!({"op": "status"})).await.is_err());
            assert!(search.running.lock().unwrap().is_none());
        });
    }

    #[test]
    fn requests_racing_retirement_always_get_a_reply() {
        let search = service();
        tauri::async_runtime::block_on(search.ask(json!({"op": "status"}))).unwrap();
        for _ in 0..12 {
            let retiring = search.clone();
            let thread = std::thread::spawn(move || retiring.retire_idle(Duration::ZERO));
            let reply = tauri::async_runtime::block_on(search.ask(json!({"op": "status"}))).unwrap();
            assert_eq!(reply["ok"], true);
            thread.join().unwrap();
        }
        let pid = tauri::async_runtime::block_on(search.ask(json!({"op": "status"}))).unwrap()["pid"].as_u64().unwrap();
        search.shutdown();
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn abandoned_in_flight_request_pins_worker_and_does_not_stop_indexer() {
        let search = service();
        // send without awaiting: closing the caller's receiver must not make a busy helper disposable.
        let receiver = search.send(999, &json!({"id": 999, "op": "slow"})).unwrap();
        drop(receiver);
        assert!(!search.retire_idle(Duration::ZERO));
        tauri::async_runtime::block_on(search.ask(json!({"op": "status"}))).unwrap();
        let index = IndexService::new(|_| {});
        let launch = Launch { program: "python3".into(), args: vec!["-u".into(), "-c".into(),
            "import sys\nfor line in sys.stdin:\n if line.strip() == 'stop': break".into()],
            env: vec![], log: None, background: true };
        index.run(&launch).unwrap();
        assert!(search.retire_idle(Duration::ZERO));
        assert!(matches!(index.running.lock().unwrap().as_mut().unwrap().child.try_wait(), Ok(None)));
        index.shutdown();
        assert!(index.run(&launch).is_err());
        assert!(index.sync_code(&launch).is_err());
        let model = ModelDownload::new(|_| {});
        model.shutdown();
        assert!(model.run(&launch).is_err());
        search.shutdown();
    }
}

/// `digup index-service`: syncs the index with the folders, indexes what's new, and exits once it's done (after a
/// minute with nothing to do). `run` starts it, asks the running one to sync again, or, when the folders changed (another
/// launch), starts it over with them. When it ends by itself, `on_event` gets {"event": "ended", "status": its exit
/// code, "owed": whether a sync asked of it never started}; one stopped here doesn't report.
pub struct IndexService {
    running: Arc<Mutex<Option<Indexing>>>,
    on_event: EventSink,
    closed: AtomicBool,
}

struct Indexing {
    child: Child,
    stdin: ChildStdin,
    args: Vec<String>,
    number: u64,
    /// Asked to sync, and it hasn't started that sync yet: owed if it exits first (it was leaving as it was asked).
    owed: bool,
}

impl IndexService {
    pub fn new(on_event: impl Fn(Value) + Send + Sync + 'static) -> IndexService {
        IndexService { running: Arc::default(), on_event: Arc::new(on_event), closed: AtomicBool::new(false) }
    }

    pub fn run(&self, launch: &Launch) -> Result<(), String> {
        let mut running = self.running.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) { return Err("DigUp is quitting".into()); }
        if let Some(indexing) = running.as_mut() {
            if matches!(indexing.child.try_wait(), Ok(None)) && indexing.args == launch.args {
                indexing.owed = true;
                return writeln!(indexing.stdin, "sync").and_then(|_| indexing.stdin.flush()).map_err(|e| e.to_string());
            }
        }
        // Gone, or other folders: (stop it and) start anew.
        if let Some(Indexing { child, stdin, .. }) = running.take() {
            stop(child, stdin, "stop");
        }
        let (child, stdin, stdout) = launch.spawn()?;
        let number = STARTED.fetch_add(1, Ordering::Relaxed);
        let (on_event, slot) = (self.on_event.clone(), self.running.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
                if event["event"] == "sync" {
                    if let Some(indexing) = slot.lock().unwrap().as_mut().filter(|indexing| indexing.number == number) {
                        indexing.owed = false;
                    }
                }
                on_event(event);
            }
            let mut slot = slot.lock().unwrap();
            if slot.as_ref().is_some_and(|indexing| indexing.number == number) {
                if let Some(mut indexing) = slot.take() {
                    drop(slot);
                    let status = reap(&mut indexing.child);
                    on_event(json!({"event": "ended", "status": status, "owed": indexing.owed}));
                }
            }
        });
        *running = Some(Indexing { child, stdin, args: launch.args.clone(), number, owed: false });
        Ok(())
    }

    /// Code search's folders changed: the running helper syncs the code index (`run` would sync the main one), or one
    /// starts (it syncs both).
    pub fn sync_code(&self, launch: &Launch) -> Result<(), String> {
        {
            let mut running = self.running.lock().unwrap();
            if self.closed.load(Ordering::SeqCst) { return Err("DigUp is quitting".into()); }
            if let Some(indexing) = running.as_mut() {
                if matches!(indexing.child.try_wait(), Ok(None)) && indexing.args == launch.args {
                    return writeln!(indexing.stdin, "sync code").and_then(|_| indexing.stdin.flush()).map_err(|e| e.to_string());
                }
            }
        }
        self.run(launch)
    }

    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.stop();
    }

    pub fn stop(&self) {
        if let Some(indexing) = self.running.lock().unwrap().take() {
            stop(indexing.child, indexing.stdin, "stop");
        }
    }
}

/// `digup download-model`: the model files, from the Hugging Face cache or downloaded with resume, each checked. One at
/// a time; `run` again while it runs does nothing. Its events ("progress", "done", "error"…) go to `on_event`.
pub struct ModelDownload {
    running: Arc<Mutex<Option<(Child, u64)>>>,
    on_event: EventSink,
    closed: AtomicBool,
}

impl ModelDownload {
    pub fn new(on_event: impl Fn(Value) + Send + Sync + 'static) -> ModelDownload {
        ModelDownload { running: Arc::default(), on_event: Arc::new(on_event), closed: AtomicBool::new(false) }
    }

    pub fn run(&self, launch: &Launch) -> Result<(), String> {
        let mut running = self.running.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) { return Err("DigUp is quitting".into()); }
        if let Some((child, _)) = running.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                return Ok(());
            }
        }
        let (child, stdin, stdout) = launch.spawn()?;
        drop(stdin);
        let number = STARTED.fetch_add(1, Ordering::Relaxed);
        let (on_event, slot) = (self.on_event.clone(), self.running.clone());
        std::thread::spawn(move || {
            let mut ended = false;
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(event) = serde_json::from_str::<Value>(&line) {
                    ended |= event["event"] == "done" || event["event"] == "error";
                    on_event(event);
                }
            }
            let mut slot = slot.lock().unwrap();
            if slot.as_ref().is_some_and(|(_, started)| *started == number) {
                if let Some((mut child, _)) = slot.take() {
                    reap(&mut child);
                }
            }
            drop(slot);
            if !ended {
                // No final word: the helper died (after a finished file, on some Linux systems). What to make of it is
                // the app's call: the files may all be there.
                on_event(json!({"event": "stopped"}));
            }
        });
        *running = Some((child, number));
        Ok(())
    }

    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.stop();
    }

    pub fn stop(&self) {
        if let Some((mut child, _)) = self.running.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// On Windows the helpers go into a job that closes with this process, so they end even if the shell crashes (on Linux
/// and macOS the helpers watch their parent themselves: `ParentWatch` in Sources/digup). Their own children follow
/// them into it.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    struct Job(HANDLE);
    // SAFETY: a job's handle may be used from any thread; it's never closed (the system closes it with this process).
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    fn job() -> Option<&'static Job> {
        static JOB: OnceLock<Option<Job>> = OnceLock::new();
        JOB.get_or_init(|| unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return None;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(handle, JobObjectExtendedLimitInformation,
                                              &limits as *const _ as *const _, std::mem::size_of_val(&limits) as u32);
            (set != 0).then_some(Job(handle))
        })
        .as_ref()
    }

    pub fn add(child: &std::process::Child) {
        if let Some(job) = job() {
            // SAFETY: both handles are valid: the job's for this process's life, the child's while `child` lives.
            unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) };
        }
    }

    pub fn end_all() {
        let Some(job) = job() else { return };
        // SAFETY: the job's handle is valid for this process's life; `info` is the size the query is told.
        unsafe {
            TerminateJobObject(job.0, 1);
            for _ in 0..100 {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                let asked = QueryInformationJobObject(job.0, JobObjectBasicAccountingInformation,
                                                      &mut info as *mut _ as *mut _, std::mem::size_of_val(&info) as u32,
                                                      std::ptr::null_mut());
                if asked == 0 || info.ActiveProcesses == 0 {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}
