//! End-to-end tests that drive the real `ovvc` binary as independent OS
//! processes.
//!
//! Every "machine" is a separate process with its own data directory and its
//! own loopback IP address, so peers really talk over TCP rather than through
//! any in-process shortcut. Tests are serialized with [`serial`]: several
//! libp2p swarms, mDNS advertisements and DHT records in one test binary would
//! otherwise interfere with one another.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-cli --test e2e -- --test-threads=1
//! ```

#![allow(clippy::type_complexity)]

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ovvc_client::{Client, direct_guild_id};
use ovvc_ipc::default_socket_path;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ovvc");

const IP_A: &str = "127.0.0.2";
const IP_B: &str = "127.0.0.3";
const IP_C: &str = "127.0.0.4";

/// Startup output has to appear before anything else can happen.
const STARTUP: Duration = Duration::from_secs(15);
/// Network propagation (sync, message delivery, reconnection).
const NET: Duration = Duration::from_secs(20);
/// Posting + echoing a few hundred messages through the local chat process.
const BULK: Duration = Duration::from_secs(30);

/// Cargo runs integration tests in parallel by default. mDNS, DHT records and
/// fixed loopback addresses all cross-test global state, so serialize every
/// test in this file. The lock is poison-tolerant: a failed test must not stop
/// the remaining scenarios from running.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// CLI invocations (short-lived commands: init, guild create/invite/join,
// send, history). The long-running `chat` command uses ChatProcess below.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CliOutput {
    command: String,
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl CliOutput {
    fn describe(&self) -> String {
        format!(
            "$ {}\nstatus: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.command, self.status, self.stdout, self.stderr
        )
    }

    fn require_success(self) -> Self {
        assert!(
            self.status.success(),
            "command failed:\n{}",
            self.describe()
        );
        self
    }
}

static DAEMON_BINARY: OnceLock<PathBuf> = OnceLock::new();

/// The daemon binary next to the CLI binary. `cargo test -p ovvc-cli` does not
/// build sibling packages, so build it once if the workspace build has not
/// already produced it.
fn daemon_binary() -> PathBuf {
    DAEMON_BINARY
        .get_or_init(|| {
            if let Some(path) = std::env::var_os("OVVC_DAEMON") {
                return PathBuf::from(path);
            }
            let cli = PathBuf::from(env!("CARGO_BIN_EXE_ovvc"));
            let sibling = cli.with_file_name("ovvc-daemon");
            if sibling.is_file() {
                return sibling;
            }
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "ovvc-daemon"])
                .status()
                .expect("run cargo to build ovvc-daemon");
            assert!(
                status.success(),
                "could not build ovvc-daemon for the CLI e2e tests"
            );
            assert!(
                sibling.is_file(),
                "ovvc-daemon was not produced next to {}",
                cli.display()
            );
            sibling
        })
        .clone()
}

struct Machine {
    label: &'static str,
    ip: &'static str,
    dir: TempDir,
    /// The daemon owns the store for this machine. Every `ovvc` invocation in
    /// the test attaches to it, exactly as the CLI does in normal use.
    daemon: Option<Child>,
}

impl Machine {
    fn new(label: &'static str, ip: &'static str) -> Self {
        let mut machine = Self {
            label,
            ip,
            dir: tempfile::tempdir().expect("create temporary data directory"),
            daemon: None,
        };
        machine.start_daemon();
        machine.ok(&["init"]);
        machine
    }

    // The child is kept in `self.daemon` and reaped by `stop_daemon`, so the
    // usual "wait on every path" advice does not apply here.
    #[allow(clippy::zombie_processes)]
    fn start_daemon(&mut self) {
        let dir = self.dir.path().to_path_buf();
        let mut child = Command::new(daemon_binary())
            .arg("--data-dir")
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("[{}] starting ovvc-daemon: {error}", self.label));

        let socket = default_socket_path(&dir);
        let deadline = Instant::now() + STARTUP;
        loop {
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                self.daemon = Some(child);
                return;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "[{}] ovvc-daemon did not listen on {} within {STARTUP:?}",
                    self.label,
                    socket.display()
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn stop_daemon(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(self.dir.path().join("daemon.json"));
    }

    fn restart_daemon(&mut self) {
        self.stop_daemon();
        self.start_daemon();
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// A stable, human-readable rendering used in failures.
    fn ip(&self) -> &'static str {
        self.ip
    }

    fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> CliOutput {
        let rendered: Vec<String> = args
            .iter()
            .map(|arg| arg.as_ref().to_string_lossy().into_owned())
            .collect();
        let command = format!(
            "ovvc --data-dir {} {}",
            self.path().display(),
            rendered.join(" ")
        );

        let mut command_builder = Command::new(BIN);
        command_builder.arg("--data-dir").arg(self.path());
        command_builder.args(args);

        let output = command_builder.output().unwrap_or_else(|error| {
            panic!("[{}] failed to spawn `{command}`: {error}", self.label)
        });
        CliOutput {
            command,
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn ok<S: AsRef<OsStr>>(&self, args: &[S]) -> CliOutput {
        self.run(args).require_success()
    }

    fn create_guild(&self, guild: &str) {
        self.ok(&["guild", "create", guild]);
    }

    /// This machine's author id, printed as the first line of `ovvc id`.
    fn author_id(&self) -> String {
        self.ok(&["id"])
            .stdout
            .lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .unwrap_or_else(|| panic!("[{}] `id` printed no author id", self.label))
            .to_string()
    }

    /// The transport peer id for this machine's identity, read offline with
    /// `ovvc id`.
    fn peer_id(&self) -> String {
        let output = self.ok(&["id"]);
        field(&output.stdout, "peer: ")
            .unwrap_or_else(|| {
                panic!(
                    "[{}] `id` did not print `peer:`:\n{}",
                    self.label,
                    output.describe()
                )
            })
            .to_string()
    }

    /// Create an invite pinned to one peer address, returning the encoded code.
    fn create_invite(&self, guild: &str, peer_addr: &str) -> String {
        let args = [
            "guild".to_string(),
            "invite".to_string(),
            guild.to_string(),
            "--peer".to_string(),
            peer_addr.to_string(),
        ];
        let output = self.ok(&args);
        let invite = output.stdout.trim().to_string();
        assert!(
            !invite.is_empty(),
            "[{}] invite command printed nothing:\n{}",
            self.label,
            output.describe()
        );
        assert!(
            !invite.contains('\n'),
            "[{}] invite should be a single line, got {invite:?}",
            self.label
        );
        invite
    }

    fn join_guild(&self, invite: &str) -> CliOutput {
        self.run(&["guild".to_string(), "join".to_string(), invite.to_string()])
    }

    fn send(&self, guild: &str, channel: &str, body: &str) {
        self.ok(&[
            "send".to_string(),
            guild.to_string(),
            channel.to_string(),
            body.to_string(),
        ]);
    }

    fn history(&self, guild: &str, channel: &str, limit: usize) -> String {
        self.ok(&[
            "history".to_string(),
            guild.to_string(),
            channel.to_string(),
            "--limit".to_string(),
            limit.to_string(),
        ])
        .stdout
    }

    /// Spawn a long-running `chat` process. `listen` is a TCP port on this
    /// machine's loopback IP; `dial` is a complete multiaddr to dial.
    fn start_chat(&self, guild: &str, listen: Option<u16>, dial: Option<&str>) -> ChatProcess {
        let mut args = vec!["chat".to_string(), guild.to_string()];
        if let Some(port) = listen {
            args.push("--listen".to_string());
            args.push(format!("/ip4/{}/tcp/{port}", self.ip()));
        }
        if let Some(address) = dial {
            args.push("--dial".to_string());
            args.push(address.to_string());
        }
        ChatProcess::spawn(self, args)
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.stop_daemon();
    }
}

// ---------------------------------------------------------------------------
// Long-running chat process with piped stdin/stdout/stderr.
// ---------------------------------------------------------------------------

struct ChatProcess {
    label: &'static str,
    child: Child,
    stdin: Option<ChildStdin>,
    output: Arc<Mutex<String>>,
    readers: Vec<JoinHandle<()>>,
    exited: bool,
}

impl ChatProcess {
    fn spawn(machine: &Machine, args: Vec<String>) -> Self {
        let mut command = Command::new(BIN);
        command
            .arg("--data-dir")
            .arg(machine.path())
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().unwrap_or_else(|error| {
            panic!(
                "[{}] failed to spawn `ovvc --data-dir {} {}`: {error}",
                machine.label,
                machine.path().display(),
                args.join(" ")
            )
        });

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdin = child.stdin.take();
        let output = Arc::new(Mutex::new(String::new()));
        let readers = vec![
            pump(stdout, Arc::clone(&output)),
            pump(stderr, Arc::clone(&output)),
        ];

        Self {
            label: machine.label,
            child,
            stdin,
            output,
            readers,
            exited: false,
        }
    }

    fn output(&self) -> String {
        self.output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Block until `needle` appears in the captured stdout/stderr. Panics with
    /// the full captured output on timeout.
    fn wait_for(&self, needle: &str, timeout: Duration) -> String {
        let started = Instant::now();
        loop {
            let output = self.output();
            if output.contains(needle) {
                return output;
            }
            if started.elapsed() >= timeout {
                panic!(
                    "[{}] timed out after {:?} waiting for {:?}\n\
                     --- captured output ---\n{}",
                    self.label, timeout, needle, output
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn send_line(&mut self, line: &str) {
        let result = {
            let stdin = self
                .stdin
                .as_mut()
                .unwrap_or_else(|| panic!("[{}] chat stdin is closed", self.label));
            writeln!(stdin, "{line}").and_then(|()| stdin.flush())
        };
        if let Err(error) = result {
            panic!(
                "[{}] failed to write {:?} to chat stdin: {error}\n\
                 --- captured output ---\n{}",
                self.label,
                line,
                self.output()
            );
        }
    }

    /// Send `/quit`, wait for a clean exit, and return the captured output.
    fn quit_and_wait(&mut self) -> String {
        if !self.exited {
            self.send_line("/quit");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    _ => {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break;
                    }
                }
            }
            self.exited = true;
            self.join_readers();
        }
        self.output()
    }

    /// SIGKILL the child (simulating a machine crash) and reap it.
    fn kill_and_wait(&mut self) -> ExitStatus {
        let _ = self.child.kill();
        let status = self
            .child
            .wait()
            .unwrap_or_else(|error| panic!("[{}] failed to reap killed chat: {error}", self.label));
        self.exited = true;
        self.join_readers();
        status
    }

    fn join_readers(&mut self) {
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

impl Drop for ChatProcess {
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.exited = true;
            self.join_readers();
        }
    }
}

/// Read one complete line at a time into the shared buffer. Line-at-a-time
/// pumping keeps stdout and stderr from being spliced mid-line when their
/// reader threads interleave.
fn pump<R: Read + Send + 'static>(pipe: R, output: Arc<Mutex<String>>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&line);
                    let mut buffer = output
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    buffer.push_str(&text);
                    if !text.ends_with('\n') {
                        buffer.push('\n');
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pick an unused TCP port on one specific loopback address.
fn free_port(ip: &str) -> u16 {
    let listener = TcpListener::bind((ip, 0))
        .unwrap_or_else(|error| panic!("failed to bind a probe listener on {ip}: {error}"));
    listener
        .local_addr()
        .expect("probe listener local address")
        .port()
}

fn dial_address(ip: &str, port: u16, peer: &str) -> String {
    format!("/ip4/{ip}/tcp/{port}/p2p/{peer}")
}

/// Return the value of the first line starting with `prefix`.
fn field<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
}

/// Extract message bodies from `ovvc history` output.
///
/// History renders `[<rfc3339>] <event-short> <author-short>: <body>`. The
/// timestamp only contains `:` characters that are not followed by a space, so
/// the first `": "` is always the one after the author.
fn message_bodies(history: &str) -> Vec<String> {
    history
        .lines()
        .filter(|line| line.starts_with('['))
        .map(|line| {
            line.split_once(": ")
                .unwrap_or_else(|| panic!("unexpected history line: {line:?}"))
                .1
                .to_string()
        })
        .collect()
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

/// Re-run a CLI command until its stdout contains `needle`.
///
/// Profile and direct-message changes cross the network as ordinary events,
/// so a command run immediately after a peer's change can legitimately see the
/// old state for a moment.
fn wait_for_cli(machine: &Machine, args: &[&str], timeout: Duration, needle: &str) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let output = machine.run(args);
        if output.status.success() && output.stdout.contains(needle) {
            return output.stdout;
        }
        if Instant::now() >= deadline {
            panic!(
                "[{}] timed out after {timeout:?} waiting for {needle:?} in `{}`\n{}",
                machine.label,
                args.join(" "),
                output.describe()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

// ---------------------------------------------------------------------------
// Shared two-machine setup.
// ---------------------------------------------------------------------------

/// A created on A, A chatting on a real listener, B joined and chatting.
///
/// Fields that own processes come first so they are killed/reaped before the
/// temporary directories are removed when the value is dropped.
struct Connected {
    a_chat: ChatProcess,
    b_chat: ChatProcess,
    a: Machine,
    b: Machine,
    guild: String,
    /// A's dialable address, as embedded in the invite.
    dial: String,
}

fn connect_pair(guild: &str) -> Connected {
    let a = Machine::new("A", IP_A);
    let b = Machine::new("B", IP_B);
    a.create_guild(guild);

    // The store takes an exclusive lock while `chat` runs, so the invite is
    // created offline first. An invite only needs the identity's peer id and
    // the address A is about to listen on; the listener itself is not needed
    // until B joins.
    let a_port = free_port(IP_A);
    let peer = a.peer_id();
    let dial = dial_address(IP_A, a_port, &peer);
    let invite = a.create_invite(guild, &dial);

    let a_chat = a.start_chat(guild, Some(a_port), None);
    let chat_peer = field(&a_chat.wait_for("peer id: ", STARTUP), "peer id: ")
        .expect("A did not print `peer id:`")
        .to_string();
    assert_eq!(
        chat_peer, peer,
        "A's `chat` peer id does not match the peer id embedded in its invite"
    );
    a_chat.wait_for("listening: ", STARTUP);

    let join = b.join_guild(&invite);
    assert!(
        join.status.success(),
        "B failed to join the guild:\n{}",
        join.describe()
    );
    assert!(
        join.stdout.contains(guild),
        "B join output does not mention guild {guild:?}:\n{}",
        join.describe()
    );

    let b_chat = b.start_chat(guild, None, Some(&dial));

    // B's chat process is fresh, so `synced with` here can only come from this
    // connection. Once it appears, A has received B's Hello and knows B.
    b_chat.wait_for("synced with", NET);
    a_chat.wait_for("synced with", NET);

    Connected {
        a_chat,
        b_chat,
        a,
        b,
        guild: guild.to_string(),
        dial,
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[test]
fn first_run_create_invite_and_join() {
    let _guard = serial();
    let mut pair = connect_pair("e2e-first-run");

    // Contract exercised by the setup itself: A advertises a peer id and B's
    // join output names the guild. Assert them explicitly here as well.
    assert!(
        pair.a_chat.output().contains("peer id: "),
        "A chat never printed a peer id:\n{}",
        pair.a_chat.output()
    );
    pair.b_chat
        .wait_for(&format!("{} / #general", pair.guild), STARTUP);
    pair.b_chat.wait_for("synced with", NET);

    pair.a_chat.quit_and_wait();
    pair.b_chat.quit_and_wait();
}

#[test]
fn bidirectional_messages() {
    let _guard = serial();
    let mut pair = connect_pair("e2e-bidirectional");

    pair.a_chat.send_line("hello from A");
    pair.b_chat.wait_for("hello from A", NET);

    pair.b_chat.send_line("hello from B");
    pair.a_chat.wait_for("hello from B", NET);

    pair.a_chat.quit_and_wait();
    pair.b_chat.quit_and_wait();
}

#[test]
fn restart_keeps_history_and_reconnects() {
    let _guard = serial();
    let mut pair = connect_pair("e2e-restart");

    pair.a_chat.send_line("before restart from A");
    pair.b_chat.wait_for("before restart from A", NET);
    pair.b_chat.send_line("before restart from B");
    pair.a_chat.wait_for("before restart from B", NET);

    pair.a_chat.quit_and_wait();
    pair.b_chat.quit_and_wait();

    // Both machines come back up. A gets a fresh port; B dials its address.
    let a_port = free_port(IP_A);
    let peer = pair.a.peer_id();
    let dial = dial_address(IP_A, a_port, &peer);
    let mut a_chat = pair.a.start_chat(&pair.guild, Some(a_port), None);
    a_chat.wait_for("listening: ", STARTUP);

    // Startup history is printed from local storage, before any peer is up.
    a_chat.wait_for("before restart from A", STARTUP);
    a_chat.wait_for("before restart from B", STARTUP);

    let mut b_chat = pair.b.start_chat(&pair.guild, None, Some(&dial));

    b_chat.wait_for("before restart from A", STARTUP);
    b_chat.wait_for("before restart from B", STARTUP);

    b_chat.wait_for("synced with", NET);
    a_chat.wait_for("synced with", NET);

    a_chat.quit_and_wait();
    b_chat.quit_and_wait();
}

#[test]
fn offline_catchup() {
    let _guard = serial();
    let mut pair = connect_pair("e2e-offline-catchup");

    // B goes away; A keeps writing locally.
    pair.b_chat.quit_and_wait();
    pair.a_chat.send_line("missed by B while offline");
    pair.a_chat.wait_for("missed by B while offline", NET);

    // B comes back and must learn about the message it missed.
    let mut b_chat = pair.b.start_chat(&pair.guild, None, Some(&pair.dial));
    b_chat.wait_for("missed by B while offline", NET);
    b_chat.wait_for("synced with", NET);

    pair.a_chat.quit_and_wait();
    b_chat.quit_and_wait();
}

#[test]
fn concurrent_offline_posts_converge() {
    let _guard = serial();
    let mut pair = connect_pair("e2e-converge");

    pair.a_chat.quit_and_wait();
    pair.b_chat.quit_and_wait();

    // Both sides write while offline. Both messages land on the same Lamport
    // value, so this exercises tie-breaking as well as convergence.
    pair.a.send(&pair.guild, "general", "offline from A");
    pair.b.send(&pair.guild, "general", "offline from B");

    let a_port = free_port(IP_A);
    let peer = pair.a.peer_id();
    let dial = dial_address(IP_A, a_port, &peer);
    let mut a_chat = pair.a.start_chat(&pair.guild, Some(a_port), None);
    a_chat.wait_for("listening: ", STARTUP);

    let mut b_chat = pair.b.start_chat(&pair.guild, None, Some(&dial));

    // Sync is bidirectional. Wait until each side has imported the other's
    // offline message before shutting down again.
    a_chat.wait_for("offline from B", NET);
    b_chat.wait_for("offline from A", NET);
    b_chat.wait_for("synced with", NET);
    a_chat.wait_for("synced with", NET);

    a_chat.quit_and_wait();
    b_chat.quit_and_wait();

    let a_history = pair.a.history(&pair.guild, "general", 50);
    let b_history = pair.b.history(&pair.guild, "general", 50);

    let a_bodies = sorted(message_bodies(&a_history));
    let b_bodies = sorted(message_bodies(&b_history));

    assert_eq!(
        a_bodies,
        vec!["offline from A".to_string(), "offline from B".to_string()],
        "A's persisted history did not converge:\n{a_history}"
    );
    assert_eq!(
        b_bodies,
        vec!["offline from A".to_string(), "offline from B".to_string()],
        "B's persisted history did not converge:\n{b_history}"
    );
    assert_eq!(
        a_bodies.len(),
        b_bodies.len(),
        "machines disagree on history size\nA:\n{a_history}\nB:\n{b_history}"
    );
}

#[test]
fn three_processes_propagate() {
    let _guard = serial();
    let a = Machine::new("A", IP_A);
    let b = Machine::new("B", IP_B);
    let c = Machine::new("C", IP_C);
    let guild = "e2e-three";

    a.create_guild(guild);

    let a_port = free_port(IP_A);
    let peer = a.peer_id();
    let dial = dial_address(IP_A, a_port, &peer);
    let invite = a.create_invite(guild, &dial);

    let mut a_chat = a.start_chat(guild, Some(a_port), None);
    a_chat.wait_for("listening: ", STARTUP);

    for machine in [&b, &c] {
        let join = machine.join_guild(&invite);
        assert!(
            join.status.success(),
            "{} failed to join the guild:\n{}",
            machine.label,
            join.describe()
        );
        assert!(
            join.stdout.contains(guild),
            "{} join output does not mention guild {guild:?}:\n{}",
            machine.label,
            join.describe()
        );
    }

    // B and C only dial A; neither is given the other's address.
    let mut b_chat = b.start_chat(guild, None, Some(&dial));
    let mut c_chat = c.start_chat(guild, None, Some(&dial));
    b_chat.wait_for("synced with", NET);
    c_chat.wait_for("synced with", NET);

    // B -> C must be relayed by A (or delivered over a direct B-C link).
    b_chat.send_line("from B to everyone");
    c_chat.wait_for("from B to everyone", NET);

    // C -> B must work the same way.
    c_chat.send_line("from C to everyone");
    b_chat.wait_for("from C to everyone", NET);

    a_chat.quit_and_wait();
    b_chat.quit_and_wait();
    c_chat.quit_and_wait();
}

#[test]
fn pagination_large_history() {
    let _guard = serial();
    let a = Machine::new("A", IP_A);
    let b = Machine::new("B", IP_B);
    let guild = "e2e-pagination";
    const TOTAL: usize = 600;

    a.create_guild(guild);

    // Post entirely offline through the real CLI. Local `chat` mode is used
    // instead of 600 separate `send` invocations: the latter re-opens the
    // store and its log for every message (~110 s for 600), while the local
    // chat process uses exactly the same `post_message` path in under 2 s.
    let mut poster = a.start_chat(guild, None, None);
    for index in 0..TOTAL {
        poster.send_line(&format!("bulk-{index:04}"));
    }
    poster.wait_for(&format!("bulk-{:04}", TOTAL - 1), BULK);
    poster.quit_and_wait();

    // A comes online so B can fetch a history that needs several sync pages.
    let a_port = free_port(IP_A);
    let peer = a.peer_id();
    let dial = dial_address(IP_A, a_port, &peer);
    let invite = a.create_invite(guild, &dial);

    let mut a_chat = a.start_chat(guild, Some(a_port), None);
    a_chat.wait_for("listening: ", STARTUP);

    let join = b.join_guild(&invite);
    assert!(
        join.status.success(),
        "B failed to join the large guild:\n{}",
        join.describe()
    );
    a_chat.quit_and_wait();

    let a_history = a.history(guild, "general", TOTAL + 50);
    let b_history = b.history(guild, "general", TOTAL + 50);
    let a_bodies = sorted(message_bodies(&a_history));
    let b_bodies = sorted(message_bodies(&b_history));

    assert_eq!(
        a_bodies.len(),
        TOTAL,
        "A should have posted {TOTAL} messages, found {}",
        a_bodies.len()
    );
    assert_eq!(
        b_bodies.len(),
        TOTAL,
        "B received {} of {TOTAL} messages after join; sync paging lost events\n\
         --- B history (first 5 / last 5) ---\n{}\n...\n{}",
        b_bodies.len(),
        b_history.lines().take(5).collect::<Vec<_>>().join("\n"),
        b_history
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(
        a_bodies, b_bodies,
        "A and B disagree about the message set after paging"
    );
}

#[test]
fn bad_invite_is_inline_error() {
    let _guard = serial();
    let a = Machine::new("A", IP_A);

    let join = a.join_guild("garbage");
    assert!(
        !join.status.success(),
        "joining a garbage invite should fail:\n{}",
        join.describe()
    );
    let combined = format!("{}{}", join.stdout, join.stderr);
    assert!(
        combined.to_lowercase().contains("invite"),
        "error should mention the invite:\n{}",
        join.describe()
    );

    // The failed join must not have created a guild.
    let list = a.ok(&["guild", "list"]);
    assert!(
        list.stdout.contains("no communes"),
        "a failed join created a guild:\n{}",
        list.describe()
    );
}

#[test]
fn kill_and_restart_recovers() {
    use std::os::unix::process::ExitStatusExt;

    let _guard = serial();
    let mut a = Machine::new("A", IP_A);
    let guild = "e2e-recovery";
    a.create_guild(guild);

    let a_port = free_port(IP_A);
    let mut chat = a.start_chat(guild, Some(a_port), None);
    chat.wait_for("listening: ", STARTUP);

    chat.send_line("survives crash 1");
    chat.wait_for("survives crash 1", NET);
    chat.send_line("survives crash 2");
    chat.wait_for("survives crash 2", NET);

    // SIGKILL: no `/quit`, no graceful redb close, no log flush on shutdown.
    let status = chat.kill_and_wait();
    assert!(
        !status.success(),
        "chat unexpectedly exited cleanly before the simulated crash: {status:?}"
    );
    assert_eq!(status.signal(), Some(9), "expected SIGKILL, got {status:?}");

    // Kill the daemon too, with no graceful redb close, then start it again
    // against the same data directory. The store must recover the log.
    a.restart_daemon();

    // The data directory must reopen and retain the messages.
    let history = a.history(guild, "general", 50);
    assert!(
        history.contains("survives crash 1"),
        "history lost the first message after SIGKILL:\n{history}"
    );
    assert!(
        history.contains("survives crash 2"),
        "history lost the second message after SIGKILL:\n{history}"
    );
    assert_eq!(
        message_bodies(&history).len(),
        2,
        "unexpected history after SIGKILL:\n{history}"
    );

    // And a new chat session must start cleanly and print the recovered history.
    let a_port = free_port(IP_A);
    let mut restarted = a.start_chat(guild, Some(a_port), None);
    restarted.wait_for("listening: ", STARTUP);
    restarted.wait_for("survives crash 1", STARTUP);
    restarted.wait_for("survives crash 2", STARTUP);
    restarted.quit_and_wait();
}

/// A peer that goes away and comes back must be reached again with no new
/// configuration on either side.
///
/// A stays up the whole time. B listens on a fixed port and dials A once; A
/// learns B's address from the `Hello` exchange. B then quits and restarts
/// *without* dialling anyone. Only A's automatic redial can reconnect them, and
/// the message B posts after the restart proves it did.
#[test]
fn peer_is_redialled_after_it_restarts() {
    let _guard = serial();
    let a = Machine::new("A", IP_A);
    let b = Machine::new("B", IP_B);
    let guild = "e2e-redial";
    a.create_guild(guild);

    let a_port = free_port(IP_A);
    let b_port = free_port(IP_B);
    let a_dial = dial_address(IP_A, a_port, &a.peer_id());
    let invite = a.create_invite(guild, &a_dial);

    let mut a_chat = a.start_chat(guild, Some(a_port), None);
    a_chat.wait_for("listening: ", STARTUP);
    a_chat.wait_for("peer id: ", STARTUP);

    let join = b.join_guild(&invite);
    assert!(
        join.status.success(),
        "B failed to join the guild:\n{}",
        join.describe()
    );

    let mut b_chat = b.start_chat(guild, Some(b_port), Some(&a_dial));
    b_chat.wait_for("listening: ", STARTUP);
    b_chat.wait_for("synced with", NET);
    a_chat.wait_for("synced with", NET);

    // B vanishes cleanly; A sees the disconnect and keeps the address.
    b_chat.quit_and_wait();
    drop(b_chat);

    // B comes back on the same port, dialling nobody. A must redial it.
    let mut b_chat = b.start_chat(guild, Some(b_port), None);
    b_chat.wait_for("listening: ", STARTUP);

    b_chat.send_line("hello after restart");
    a_chat.wait_for("hello after restart", NET);

    // The reverse direction must work too, over the same re-established
    // connection.
    a_chat.send_line("answer after restart");
    b_chat.wait_for("answer after restart", NET);
}

/// An invite can carry several addresses for the same peer (several
/// interfaces, a stale address first, …). Every one of them must be handed to a
/// single dial: dialing them one at a time makes libp2p's
/// `DisconnectedAndNotDialing` condition skip all but the first, so a dead
/// first address would make the invite unusable.
#[test]
fn invite_with_a_dead_first_address_still_joins() {
    let _guard = serial();
    let a = Machine::new("A", IP_A);
    let b = Machine::new("B", IP_B);
    let guild = "e2e-multiaddr";
    a.create_guild(guild);

    let a_port = free_port(IP_A);
    let peer = a.peer_id();
    let good = dial_address(IP_A, a_port, &peer);
    // A port on A where nothing listens, placed first in the invite.
    let dead = dial_address(IP_A, free_port(IP_A), &peer);

    let invite = a
        .ok(&["guild", "invite", guild, "--peer", &dead, "--peer", &good])
        .stdout
        .trim()
        .to_string();
    assert!(!invite.is_empty(), "invite command printed nothing");

    let a_chat = a.start_chat(guild, Some(a_port), None);
    a_chat.wait_for("listening: ", STARTUP);

    let join = b.join_guild(&invite);
    assert!(
        join.status.success(),
        "B could not join through the second (working) address:\n{}",
        join.describe()
    );

    let mut b_chat = b.start_chat(guild, None, Some(&good));
    b_chat.wait_for("synced with", NET);
    b_chat.send_line("joined via the second address");
    a_chat.wait_for("joined via the second address", NET);
}

/// Joining an invite whose inviter is not reachable must fail, not report a
/// guild that only exists in this machine's local history. `join_guild` used
/// to return success as soon as the guild row existed, even if the sync timed
/// out, which made an offline inviter look online.
#[test]
fn join_fails_when_the_inviter_is_offline() {
    let _guard = serial();
    let a = Machine::new("A", IP_A);
    let b = Machine::new("B", IP_B);
    let guild = "e2e-offline-join";
    a.create_guild(guild);

    // Nothing is listening on this port.
    let invite = a.create_invite(guild, &dial_address(IP_A, free_port(IP_A), &a.peer_id()));
    let join = b.join_guild(&invite);
    assert!(
        !join.status.success(),
        "join claimed success while the inviter was offline:\n{}",
        join.describe()
    );

    let list = b.ok(&["guild", "list"]);
    assert!(
        list.stdout.contains("no communes"),
        "a failed join left a guild behind:\n{}",
        list.describe()
    );
}

/// The short event ids printed at the start of each history line.
fn history_message_ids(history: &str) -> Vec<String> {
    history
        .lines()
        .filter(|line| line.starts_with('['))
        .filter_map(|line| {
            line.split_once("] ")?
                .1
                .split_whitespace()
                .next()
                .map(str::to_string)
        })
        .collect()
}

#[test]
fn replies_and_mentions_work_offline() {
    let _guard = serial();
    let a = Machine::new("A", IP_A);
    let guild = "e2e-replies";
    a.create_guild(guild);
    a.ok(&["profile", guild, "alice"]);
    a.send(guild, "general", "the question");

    let history = a.history(guild, "general", 10);
    let ids = history_message_ids(&history);
    assert_eq!(ids.len(), 1, "unexpected history:\n{history}");

    // The reply command resolves the target by its history id, and @alice is
    // resolved through the roster into a stored author id.
    let reply = a.ok(&["reply", guild, "general", &ids[0], "the answer for @alice"]);
    let reply_id = reply
        .stdout
        .trim()
        .strip_prefix("replied ")
        .expect("reply prints the new event id")
        .to_string();
    assert!(!reply_id.is_empty());

    let history = a.history(guild, "general", 10);
    assert!(
        history.contains("replying to "),
        "reply context missing from history:\n{history}"
    );
    assert!(
        history.contains("the question"),
        "reply target missing from history:\n{history}"
    );
    assert!(
        history.contains("the answer for @alice"),
        "reply body missing from history:\n{history}"
    );
    assert!(
        history.contains("(+1 mentioned)"),
        "mention count missing from history:\n{history}"
    );
    assert!(
        history.contains(&reply_id[..8]),
        "reply event id missing from history:\n{history}"
    );

    // A target that does not exist is an error, not a panic or a bad event.
    let bad = a.run(&["reply", guild, "general", "deadbeef", "nope"]);
    assert!(
        !bad.status.success(),
        "a reply to an unknown message should fail:\n{}",
        bad.describe()
    );
}

/// The event id from a chat transcript line. A live chat line and a `/history`
/// line both start `[<time>] <id>`, so the id is the token after `"] "`.
fn chat_line_id(output: &str, needle: &str) -> Option<String> {
    output.lines().rev().find_map(|line| {
        if !line.starts_with('[') || !line.contains(needle) {
            return None;
        }
        let rest = line.split_once("] ")?.1;
        let id = rest.split_whitespace().next()?;
        (id.len() == 8).then(|| id.to_string())
    })
}

#[test]
fn networked_replies_and_mentions_propagate() {
    let _guard = serial();
    let mut pair = connect_pair("e2e-reply-net");

    // B needs a roster name before A can resolve @bob through its roster.
    pair.b_chat.send_line("/name bob");
    pair.b_chat.wait_for("display name set to bob", STARTUP);
    pair.a_chat.send_line("/members");
    pair.a_chat.wait_for("bob", NET);

    pair.a_chat.send_line("question from A");
    let output = pair.a_chat.wait_for("question from A", NET);
    let target =
        chat_line_id(&output, "question from A").expect("A's message id in its transcript");

    pair.a_chat
        .send_line(&format!("/reply {target} answer for @bob"));
    pair.b_chat.wait_for("answer for @bob", NET);
    pair.b_chat.wait_for("mentioned you", NET);
    pair.b_chat.wait_for("(+1 mentioned)", NET);

    pair.a_chat.quit_and_wait();
    pair.b_chat.quit_and_wait();

    let history = pair.b.history(&pair.guild, "general", 50);
    assert!(
        history.contains("replying to "),
        "reply context did not propagate to B:\n{history}"
    );
    assert!(
        history.contains("replying to") && history.contains("question from A"),
        "reply target did not propagate to B:\n{history}"
    );
    assert!(
        history.contains("answer for @bob"),
        "reply body did not propagate to B:\n{history}"
    );
}

#[test]
fn search_presence_theme_and_attachments_work_through_the_cli() {
    let _guard = serial();
    let machine = Machine::new("e2e-cli-features", IP_A);
    machine.create_guild("Features");
    machine.send("Features", "general", "the quick brown fox");

    // --- search -----------------------------------------------------------
    let search = machine.ok(&["search", "Features", "quick"]);
    assert!(
        search.stdout.contains("quick brown fox"),
        "search did not print the matching message:\n{}",
        search.describe()
    );
    assert!(
        search.stdout.contains("general"),
        "search did not name the channel:\n{}",
        search.describe()
    );
    let missing = machine.ok(&["search", "Features", "zebra"]);
    assert!(
        missing.stdout.contains("no matches"),
        "an empty search should say so:\n{}",
        missing.describe()
    );

    // --- attach, history and download ------------------------------------
    let source = machine.path().join("note.txt");
    std::fs::write(&source, b"attachment bytes\n").expect("write the attachment");
    let source_arg = source.display().to_string();
    let attach = machine.ok(&[
        "attach",
        "Features",
        "general",
        source_arg.as_str(),
        "see this",
    ]);
    assert!(
        attach.stdout.contains("sent ") && attach.stdout.contains("note.txt"),
        "attach did not report the sent file:\n{}",
        attach.describe()
    );
    let hash = attach
        .stdout
        .split_whitespace()
        .last()
        .expect("the attach output ends with a hash")
        .to_string();
    assert_eq!(hash.len(), 64, "attach printed {hash:?}, not a full hash");
    let prefix = hash[..8].to_string();

    let history = machine.history("Features", "general", 20);
    assert!(
        history.contains("see this") && history.contains("file: note.txt"),
        "history did not show the attachment:\n{history}"
    );
    assert!(
        history.contains(prefix.as_str()),
        "history did not show the attachment id:\n{history}"
    );

    let target = machine.path().join("downloaded.txt");
    let target_arg = target.display().to_string();
    let download = machine.ok(&[
        "download",
        "Features",
        prefix.as_str(),
        "--out",
        target_arg.as_str(),
    ]);
    assert!(
        download.stdout.contains("saved note.txt"),
        "download did not report the saved file:\n{}",
        download.describe()
    );
    assert_eq!(
        std::fs::read(&target).expect("read the downloaded file"),
        b"attachment bytes\n"
    );

    let again = machine.run(&[
        "download",
        "Features",
        prefix.as_str(),
        "--out",
        target_arg.as_str(),
    ]);
    assert!(
        !again.status.success(),
        "download must not overwrite an existing file:\n{}",
        again.describe()
    );

    // The same file can also be named by its announced name or by the full
    // hash, not just a prefix, and an existing target is refused either way.
    let by_name = machine.path().join("by-name.txt");
    let by_name_arg = by_name.display().to_string();
    let download = machine.ok(&[
        "download",
        "Features",
        "note.txt",
        "--out",
        by_name_arg.as_str(),
    ]);
    assert!(
        download.stdout.contains("saved note.txt"),
        "download by announced name did not work:\n{}",
        download.describe()
    );
    let by_hash = machine.path().join("by-hash.txt");
    let by_hash_arg = by_hash.display().to_string();
    let download = machine.ok(&[
        "download",
        "Features",
        hash.as_str(),
        "--out",
        by_hash_arg.as_str(),
    ]);
    assert!(
        download.stdout.contains("saved note.txt"),
        "download by full hash did not work:\n{}",
        download.describe()
    );
    assert_eq!(
        std::fs::read(&by_hash).expect("read the file saved by hash"),
        b"attachment bytes\n"
    );

    // --- presence ---------------------------------------------------------
    let presence = machine.ok(&["presence", "Features", "away"]);
    assert!(
        presence.stdout.contains("presence away"),
        "presence did not report the new status:\n{}",
        presence.describe()
    );
    // A short hold should announce the status, keep it up for the requested
    // time and then clear it, all in one process.
    let held = machine.ok(&["presence", "Features", "dnd", "--hold", "2"]);
    assert!(
        held.stdout.contains("presence dnd for 2s") && held.stdout.contains("presence offline"),
        "a held presence did not announce and clear itself:\n{}",
        held.describe()
    );
    let unknown = machine.run(&["presence", "Features", "sleepy"]);
    assert!(
        !unknown.status.success(),
        "an unknown presence must be an error:\n{}",
        unknown.describe()
    );

    // --- theme ------------------------------------------------------------
    let theme_file = machine.path().join("theme.toml");
    let set = Command::new(BIN)
        .arg("--data-dir")
        .arg(machine.path())
        .args(["theme", "amber"])
        .env("OVVC_THEME", &theme_file)
        .output()
        .expect("run ovvc theme amber");
    assert!(
        set.status.success(),
        "theme amber failed:\n{}",
        String::from_utf8_lossy(&set.stderr)
    );
    let written = std::fs::read_to_string(&theme_file).expect("read the theme file");
    assert!(
        written.contains("preset = \"amber\""),
        "theme.toml does not select amber:\n{written}"
    );
    let list = Command::new(BIN)
        .arg("--data-dir")
        .arg(machine.path())
        .args(["theme", "--list"])
        .env("OVVC_THEME", &theme_file)
        .output()
        .expect("run ovvc theme --list");
    let listing = String::from_utf8_lossy(&list.stdout);
    assert!(
        listing.contains("* amber"),
        "the chosen preset is not marked in the list:\n{listing}"
    );
}

#[test]
fn member_profiles_and_direct_messages_work_through_the_cli() {
    let _guard = serial();
    let mut pair = connect_pair("e2e-profiles-dms");
    let a_author = pair.a.author_id();
    let b_author = pair.b.author_id();
    let a_short = &a_author[..8];
    let b_short = &b_author[..8];

    // `guild join` does not announce the joiner's roster membership by
    // itself; the TUI names itself after `/join`, and the positional form of
    // `profile` is the CLI equivalent. This is also the first real network
    // check: A must learn both the name and the profile from B's signed
    // events.
    let b_profile = pair
        .b
        .ok(&["profile", &pair.guild, "bea", "--about", "peer person"]);
    assert!(
        b_profile.stdout.contains("display name set to bea")
            && b_profile.stdout.contains("about 11 byte(s)"),
        "B could not name and profile itself after joining:\n{}",
        b_profile.describe()
    );
    let b_member = wait_for_cli(
        &pair.a,
        &["member", &pair.guild, b_short],
        NET,
        "peer person",
    );
    assert!(
        b_member.contains("name:       bea") && b_member.contains("you:        no"),
        "A did not render B's profile correctly:\n{b_member}"
    );

    // --- profile publishing and member lookup ----------------------------
    let profile = pair.a.ok(&[
        "profile",
        &pair.guild,
        "ada",
        "--about",
        "terminal person",
        "--pronouns",
        "she/her",
        "--link",
        "https://example.com",
        "--link",
        "https://second.example",
    ]);
    assert!(
        profile.stdout.contains("display name set to ada"),
        "profile did not set the display name:\n{}",
        profile.describe()
    );
    assert!(
        profile.stdout.contains("about 15 byte(s)") && profile.stdout.contains("2 link(s)"),
        "profile did not report the saved fields:\n{}",
        profile.describe()
    );

    let member = pair.a.ok(&["member", &pair.guild, a_short]);
    for needle in [
        "name:       ada",
        "joined:     ",
        "messages:   ",
        "activity:   ",
        "pronouns:   she/her",
        "you:        yes",
        "  terminal person",
        "  https://example.com",
        "  https://second.example",
    ] {
        assert!(
            member.stdout.contains(needle),
            "member output is missing {needle:?}:\n{}",
            member.describe()
        );
    }

    // B reads the same profile through the daemon once the signed profile
    // event has synced over the real connection.
    let member = wait_for_cli(
        &pair.b,
        &["member", &pair.guild, a_short],
        NET,
        "terminal person",
    );
    assert!(
        member.contains("you:        no"),
        "B rendered A's profile as its own:\n{member}"
    );

    let unknown = pair.a.run(&["member", &pair.guild, "nobody-here"]);
    assert!(
        !unknown.status.success() && unknown.stderr.contains("no member matches"),
        "an unknown member must be an error:\n{}",
        unknown.describe()
    );

    // --- direct messages --------------------------------------------------
    let opened = pair.a.ok(&["dm", &pair.guild, "bea"]);
    assert!(
        opened.stdout.contains("opened direct message with"),
        "A did not open a direct message:\n{}",
        opened.describe()
    );
    let dm_id = field(&opened.stdout, "id: ")
        .unwrap_or_else(|| panic!("no id in the dm output:\n{}", opened.describe()))
        .to_string();
    assert_eq!(dm_id.len(), 64, "dm printed {dm_id:?}, not a guild id");
    let dm_short = &dm_id[..8];

    let again = pair.a.ok(&["dm", &pair.guild, "bea"]);
    assert!(
        again.stdout.contains("already exists") && again.stdout.contains(&dm_id),
        "reopening the same conversation did not reuse it:\n{}",
        again.describe()
    );

    let dms = pair.a.ok(&["dms"]);
    assert!(
        dms.stdout.contains(dm_short) && dms.stdout.contains("dm:"),
        "dms did not list the new conversation:\n{}",
        dms.describe()
    );

    // B learns about the conversation from the announcement in the commune,
    // then derives the same id when opening it.
    let b_dms = wait_for_cli(&pair.b, &["dms"], NET, dm_short);
    assert!(
        b_dms.contains("dm:"),
        "B's dms output has no peer name:\n{b_dms}"
    );
    // B knows the conversation from the announcement. Reopening it must not
    // create a second founder; if the guild has not synced yet, the command
    // reports it as existing and leaves the guild to the normal sync path.
    let b_open = pair.b.ok(&["dm", &pair.guild, a_short]);
    assert!(
        b_open.stdout.contains("already exists") && b_open.stdout.contains(&dm_id),
        "B derived a different direct-message id:\n{}",
        b_open.describe()
    );

    // The DM guild itself is needed before B can send into its channel.
    wait_for_cli(&pair.b, &["guild", "list"], NET, dm_short);

    // A message in the DM is a normal signed event; both sides see it once
    // the DM nodes sync.
    let sent = pair.b.ok(&["send", dm_short, "general", "hello in dm"]);
    assert!(
        sent.stdout.contains("sent "),
        "B could not send into the conversation:\n{}",
        sent.describe()
    );
    wait_for_cli(
        &pair.a,
        &["history", dm_short, "general", "--limit", "20"],
        NET,
        "hello in dm",
    );

    let reply = pair.a.ok(&["send", dm_short, "general", "reply in dm"]);
    assert!(
        reply.stdout.contains("sent "),
        "A could not reply in the conversation:\n{}",
        reply.describe()
    );
    wait_for_cli(
        &pair.b,
        &["history", dm_short, "general", "--limit", "20"],
        NET,
        "reply in dm",
    );

    pair.a_chat.quit_and_wait();
    pair.b_chat.quit_and_wait();
}

/// A DM announcement reaches the recipient before the opener's DM log does.
/// In that window the conversation is listed, but the guild itself is not in
/// the store. `history` and `send` used to answer `not found: commune ...`,
/// which reads as "this conversation does not exist" and is not retryable.
///
/// The state is seeded directly into the two stores so the test does not race
/// the DM nodes: Bob's store holds Alice's context log (including the
/// announcement) and no DM guild at all. The daemon and CLI under test are
/// real processes.
#[test]
fn an_unsynced_dm_announcement_reports_syncing_not_missing() {
    let _guard = serial();

    let alice_dir = tempfile::tempdir().expect("alice data directory");
    let bob_dir = tempfile::tempdir().expect("bob data directory");
    let alice = Client::open_local(alice_dir.path()).expect("alice client");
    let bob = Client::open_local(bob_dir.path()).expect("bob client");

    let guild = alice.create_guild("dm-announcement").expect("guild");
    alice.set_display_name(guild.id, "ada").expect("alice name");
    let context = alice.events_since(guild.id, None, 100).expect("events");
    let join = {
        let bob_store = bob.storage();
        let mut store = bob_store.lock().expect("bob store");
        for event in context.events {
            store.import_event(event).expect("import context");
        }
        store
            .append_member_joined(guild.id, "bob", bob.identity())
            .expect("join")
            .expect("a fresh guild accepts the join")
    };
    let alice_store = alice.storage();
    alice_store
        .lock()
        .expect("alice store")
        .import_event(join)
        .expect("import bob's join");

    let channel = alice.default_channel(guild.id).expect("general");
    let dm_id = direct_guild_id(&alice.author_id(), &bob.author_id());
    let announcement = alice_store
        .lock()
        .expect("alice store")
        .announce_direct_message(
            guild.id,
            channel.id,
            bob.author_id(),
            dm_id,
            alice.identity(),
        )
        .expect("announce the DM");
    let bob_store = bob.storage();
    bob_store
        .lock()
        .expect("bob store")
        .import_event(announcement)
        .expect("import the announcement");

    // Release every handle on the store before the daemon opens it.
    drop(bob_store);
    drop(alice_store);
    drop(alice);
    drop(bob);

    let mut machine = Machine {
        label: "announced-dm",
        ip: "127.0.0.9",
        dir: bob_dir,
        daemon: None,
    };
    machine.start_daemon();

    let dm_hex = dm_id.to_hex();
    let dm_short = &dm_hex[..8];
    let dms = machine.ok(&["dms"]);
    assert!(
        dms.stdout.contains(dm_short) && dms.stdout.contains("dm:ada"),
        "the announcement should be visible before its log syncs:\n{}",
        dms.describe()
    );
    let guilds = machine.ok(&["guild", "list"]);
    assert!(
        !guilds.stdout.contains(dm_short),
        "this test needs the DM guild to be absent locally:\n{}",
        guilds.describe()
    );

    let history = machine.run(&["history", &dm_hex, "general"]);
    assert!(
        !history.status.success(),
        "a DM whose log has not synced must not look like a successful read:\n{}",
        history.describe()
    );
    assert!(
        history.stderr.contains("still syncing"),
        "history should report a retryable sync state:\n{}",
        history.describe()
    );
    assert!(
        !history.stderr.contains("not found"),
        "history must not claim an announced conversation does not exist:\n{}",
        history.describe()
    );

    let send = machine.run(&["send", &dm_hex, "general", "hello"]);
    assert!(
        !send.status.success(),
        "send into an unsynced DM must fail rather than post elsewhere:\n{}",
        send.describe()
    );
    assert!(
        send.stderr.contains("still syncing") && !send.stderr.contains("not found"),
        "send should report the real DM state:\n{}",
        send.describe()
    );
}
