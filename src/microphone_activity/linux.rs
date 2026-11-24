//! Microphone users on Linux: PipeWire's running capture streams, watched
//! through one long-running `pw-dump --monitor`. Streams that capture an
//! output's monitor (`stream.capture.sink`), such as Hex's own meeting
//! system audio, are not microphone users. The same graph tracks running
//! playback streams, so a call app still playing counts as present even off
//! the microphone. Only stream metadata is read; no audio is captured. A
//! playback node's process tree (`/proc/<pid>/status`) is read only to leave
//! Hex's own children, such as a `pw-cat` playback, out of the app list.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::BufReader;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde_json::Value;

use super::{ActiveMicrophoneApplication, deduplicated};

const INPUT_STREAM: &str = "Stream/Input/Audio";
const OUTPUT_STREAM: &str = "Stream/Output/Audio";

#[derive(Clone, Debug, Default, PartialEq)]
struct Node {
    input: bool,
    output: bool,
    running: bool,
    client: Option<u64>,
    /// The node's own name, which tells Hex's streams apart from apps'.
    node_name: Option<String>,
    /// The stable number a capture stream names this node by, from
    /// `object.serial`; a node without one is named by its graph id.
    serial: Option<String>,
    process: Process,
}

/// One application playing through PipeWire now, for Labs › Choose meeting
/// audio: its stable id, its display name, and the node targets its running
/// output streams answer to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlayingApplication {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) targets: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Process {
    pid: Option<i32>,
    binary: Option<String>,
    name: Option<String>,
}

impl Process {
    fn from_props(props: &Value) -> Self {
        let text = |key: &str| props.get(key).and_then(Value::as_str).map(str::to_owned);
        Self {
            pid: props.get("application.process.id").and_then(|pid| {
                pid.as_i64()
                    .or_else(|| pid.as_str().and_then(|pid| pid.parse().ok()))
                    .and_then(|pid| i32::try_from(pid).ok())
            }),
            binary: text("application.process.binary"),
            name: text("application.name"),
        }
    }

    /// Node properties, completed by the client's where the node lacks them.
    fn or(&self, other: &Process) -> Process {
        Process {
            pid: self.pid.or(other.pid),
            binary: self.binary.clone().or_else(|| other.binary.clone()),
            name: self.name.clone().or_else(|| other.name.clone()),
        }
    }
}

/// The PipeWire graph as far as microphone use goes.
#[derive(Default)]
struct Graph {
    nodes: HashMap<u64, Node>,
    clients: HashMap<u64, Process>,
    /// Whether a first dump has been applied. Before it the graph is empty,
    /// which says nothing about what is playing or recording.
    ready: bool,
}

impl Graph {
    /// Applies one object of `pw-dump` output: a node or client added or
    /// changed, or any object removed (`"info": null`).
    fn apply(&mut self, object: &Value) {
        let Some(id) = object.get("id").and_then(Value::as_u64) else {
            return;
        };
        self.ready = true;
        let info = object.get("info").filter(|info| !info.is_null());
        let Some(info) = info else {
            self.nodes.remove(&id);
            self.clients.remove(&id);
            return;
        };
        let props = info.get("props").cloned().unwrap_or(Value::Null);
        match object.get("type").and_then(Value::as_str) {
            Some("PipeWire:Interface:Node") => {
                let class = props.get("media.class").and_then(Value::as_str);
                let monitor = props.get("stream.capture.sink").is_some_and(|sink| {
                    sink.as_bool() == Some(true) || sink.as_str() == Some("true")
                });
                self.nodes.insert(
                    id,
                    Node {
                        input: class == Some(INPUT_STREAM) && !monitor,
                        output: class == Some(OUTPUT_STREAM),
                        running: info.get("state").and_then(Value::as_str) == Some("running"),
                        client: props.get("client.id").and_then(|client| {
                            client
                                .as_u64()
                                .or_else(|| client.as_str().and_then(|client| client.parse().ok()))
                        }),
                        node_name: props
                            .get("node.name")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        serial: props.get("object.serial").and_then(|serial| {
                            serial
                                .as_u64()
                                .map(|serial| serial.to_string())
                                .or_else(|| serial.as_str().map(str::to_owned))
                        }),
                        process: Process::from_props(&props),
                    },
                );
            }
            Some("PipeWire:Interface:Client") => {
                self.clients.insert(id, Process::from_props(&props));
            }
            _ => {}
        }
    }

    fn microphone_users(&self, own_pid: i32) -> Vec<ActiveMicrophoneApplication> {
        let applications = self
            .nodes
            .values()
            .filter(|node| node.input && node.running)
            .filter_map(|node| {
                let client = node
                    .client
                    .and_then(|client| self.clients.get(&client))
                    .cloned()
                    .unwrap_or_default();
                let process = node.process.or(&client);
                let pid = process.pid?;
                let binary = process.binary?;
                (pid != own_pid).then(|| ActiveMicrophoneApplication {
                    pid,
                    name: process.name.unwrap_or_else(|| binary.clone()),
                    bundle_id: binary,
                })
            })
            .collect();
        deduplicated(applications)
    }

    /// The applications with running output streams, each once, with every
    /// node target its streams present now. Hex's own sounds are left out.
    ///
    /// The graph holds its nodes and clients in a hash map, so the answer is
    /// collected per app and sorted rather than taken from whichever node
    /// came first: a browser whose first stream reports no
    /// `application.name` still lists the name its other stream reports, and
    /// two readings of one graph always compare equal.
    fn playing_applications(&self, own_pid: i32) -> Vec<PlayingApplication> {
        self.playing_applications_with(own_pid, parent_pid)
    }

    /// [`Self::playing_applications`] with the process tree given, so a test
    /// can name Hex's children without spawning them.
    fn playing_applications_with(
        &self,
        own_pid: i32,
        parent_of: impl Fn(i32) -> Option<i32>,
    ) -> Vec<PlayingApplication> {
        let mut applications: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> =
            BTreeMap::new();
        for (node_id, node) in &self.nodes {
            if !node.output || !node.running {
                continue;
            }
            let client = node
                .client
                .and_then(|client| self.clients.get(&client))
                .cloned()
                .unwrap_or_default();
            let process = node.process.or(&client);
            if is_hex_output(&process, node.node_name.as_deref(), own_pid, &parent_of) {
                continue;
            }
            let Some(id) = process.binary.clone().or_else(|| process.name.clone()) else {
                continue;
            };
            let target = node.serial.clone().unwrap_or_else(|| node_id.to_string());
            let (names, targets) = applications.entry(id).or_default();
            targets.insert(target);
            names.extend(process.name);
        }
        let mut applications: Vec<PlayingApplication> = applications
            .into_iter()
            .map(|(id, (names, targets))| {
                let name = names.into_iter().next().unwrap_or_else(|| id.clone());
                PlayingApplication {
                    id,
                    name,
                    targets: targets.into_iter().collect(),
                }
            })
            .collect();
        applications.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        applications
    }

    /// Whether any running output stream belongs to `canonical_bundle_id`.
    fn playing_output(&self, canonical_bundle_id: &str) -> bool {
        self.nodes
            .values()
            .filter(|node| node.output && node.running)
            .any(|node| {
                let client = node
                    .client
                    .and_then(|client| self.clients.get(&client))
                    .cloned()
                    .unwrap_or_default();
                let process = node.process.or(&client);
                let Some(binary) = process.binary else {
                    return false;
                };
                super::canonical_bundle_id_for(&binary, process.name.as_deref())
                    == Some(canonical_bundle_id)
            })
    }
}

/// The names Hex gives its own PipeWire nodes: its ALSA streams (`hex`), Hex
/// Microphone's `pw-cat` playback (`hex_microphone`), and the meeting
/// monitors (`hex-meeting-system-audio`, and one numbered node per target).
const HEX_ALSA_STREAM: &str = "hex";
const HEX_MICROPHONE_NODE: &str = "hex_microphone";
const MEETING_MONITOR_NODE: &str = "hex-meeting-system-audio";

/// Whether `name` is exactly one of Hex's own PipeWire node names. A third-
/// party app whose name merely starts with `hex-` or `hex_` is not Hex.
fn is_hex_node_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name == HEX_ALSA_STREAM || name == HEX_MICROPHONE_NODE || name == MEETING_MONITOR_NODE {
        return true;
    }
    name.strip_prefix(MEETING_MONITOR_NODE)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Whether an output stream node is Hex's own sound: Hex's process, a child
/// of Hex (its `pw-cat` playback, such as Hex Microphone's), or one of Hex's
/// node names. The Meetings pane lists apps, and Hex is never one of them.
fn is_hex_output(
    process: &Process,
    node_name: Option<&str>,
    own_pid: i32,
    parent_of: impl Fn(i32) -> Option<i32>,
) -> bool {
    if process.pid == Some(own_pid) {
        return true;
    }
    if process
        .pid
        .is_some_and(|pid| parent_of(pid) == Some(own_pid))
    {
        return true;
    }
    node_name.is_some_and(is_hex_node_name)
}

/// The parent pid of the running process `pid`, from `/proc`; `None` when it
/// has exited or its entry cannot be read.
fn parent_pid(pid: i32) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|parent| parent.trim().parse().ok())
}

/// The one PipeWire graph, watched for the life of the process.
///
/// `pw-dump --monitor` is spawned with `PR_SET_PDEATHSIG`, which follows the
/// thread that spawned it, so it must not be owned by a caller's short-lived
/// thread: a pane listing thread or a meeting capture retry would otherwise
/// kill the watcher when it finished. The thread below spawns and reads the
/// child, lives as long as Hex, and restarts the child after [`RETRY`] when
/// it ends.
struct Watcher {
    graph: Mutex<Graph>,
}

static WATCHER: OnceLock<Arc<Watcher>> = OnceLock::new();

/// The one watcher, started on first use and kept for the process lifetime.
fn watcher() -> Arc<Watcher> {
    WATCHER
        .get_or_init(|| {
            let watcher = Arc::new(Watcher {
                graph: Mutex::new(Graph::default()),
            });
            let owned = Arc::clone(&watcher);
            if let Err(error) = thread::Builder::new()
                .name("pipewire-monitor".into())
                .spawn(move || watch(owned))
            {
                // The graph then never answers, which every caller already
                // treats as "not known", instead of taking Hex down.
                tracing::error!(%error, "could not start the PipeWire watcher thread");
            }
            watcher
        })
        .clone()
}

/// How long a `pw-dump` that ended waits before it is started again.
const RETRY: Duration = Duration::from_secs(60);

/// Spawns `pw-dump --monitor` from this long-lived thread, applies each dump
/// to the graph, and starts it again after [`RETRY`] when it ends.
fn watch(watcher: Arc<Watcher>) {
    let mut last_error: Option<String> = None;
    loop {
        match dump(&watcher) {
            Ok(()) => {
                last_error = None;
                tracing::debug!("PipeWire's pw-dump ended; watching it again");
            }
            Err(error) => {
                // An install that is missing stays missing: say so once per
                // distinct failure, not on every retry.
                let error = format!("{error:#}");
                if last_error.as_deref() != Some(error.as_str()) {
                    tracing::warn!(%error, "could not watch PipeWire (is pw-dump installed?)");
                }
                last_error = Some(error);
            }
        }
        thread::sleep(RETRY);
    }
}

/// Runs one `pw-dump --monitor` until it ends, applying every dump to the
/// graph. Only the watcher thread calls this, and it spawns the child, so the
/// child's `PR_SET_PDEATHSIG` follows the thread that lives for the process.
fn dump(watcher: &Watcher) -> Result<()> {
    let mut command = Command::new("pw-dump");
    command
        .arg("--monitor")
        .arg("--no-colors")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: `prctl` is async-signal-safe, and it is called on the watcher
    // thread, which lives for the process, so the child ends with Hex rather
    // than with whichever caller first needed the graph.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .wrap_err("could not start pw-dump --monitor")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("pw-dump has no output"))?;
    let updates =
        serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<Vec<Value>>();
    let mut first = true;
    for update in updates {
        let Ok(objects) = update else {
            break;
        };
        let mut graph = watcher
            .graph
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if first {
            // A restarted pw-dump starts with the whole graph: an object
            // that vanished while it was down never reports its removal, so
            // the old nodes go before the new dump is applied. The lock is
            // held, so no reader sees the cleared graph.
            graph.nodes.clear();
            graph.clients.clear();
            first = false;
        }
        for object in &objects {
            graph.apply(object);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

/// The applications whose PipeWire capture streams are running now. Before
/// the first dump this reports none, as it does while PipeWire is missing.
pub fn active_microphone_applications() -> Result<Vec<ActiveMicrophoneApplication>> {
    let watcher = watcher();
    let graph = watcher
        .graph
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !graph.ready {
        return Ok(Vec::new());
    }
    Ok(graph.microphone_users(std::process::id() as i32))
}

/// The applications playing through PipeWire now, for Labs › Choose meeting
/// audio, or `None` before the first dump. An empty graph would look like
/// every app stopped playing, so the meeting source keeps the captures it has
/// until PipeWire has answered once.
pub(crate) fn playing_applications() -> Option<Vec<PlayingApplication>> {
    let watcher = watcher();
    let graph = watcher
        .graph
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    graph
        .ready
        .then(|| graph.playing_applications(std::process::id() as i32))
}

/// Whether any running PipeWire output stream belongs to the app
/// `canonical_bundle_id`: a call keeps its output open, muted or not, and a
/// hung-up call closes it with the microphone (#835). Before the first dump
/// this is an error, which the caller treats as playing.
pub(crate) fn application_running_output(canonical_bundle_id: &str) -> Result<bool> {
    let watcher = watcher();
    let graph = watcher
        .graph
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !graph.ready {
        return Err(eyre!("PipeWire hasn't delivered its first dump yet"));
    }
    Ok(graph.playing_output(canonical_bundle_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: u64, state: &str, props: Value) -> Value {
        json!({"id": id, "type": "PipeWire:Interface:Node", "info": {"state": state, "props": props}})
    }

    #[test]
    fn running_capture_streams_are_microphone_users() {
        let mut graph = Graph::default();
        for object in [
            node(
                1,
                "running",
                json!({"media.class": "Stream/Input/Audio", "application.process.binary": "zoom",
                       "application.process.id": 41, "application.name": "ZOOM VoiceEngine"}),
            ),
            // A monitor capture (meeting system audio, level meters).
            node(
                2,
                "running",
                json!({"media.class": "Stream/Input/Audio", "stream.capture.sink": true,
                       "application.process.binary": "pavucontrol", "application.process.id": 42}),
            ),
            // Idle, and playback.
            node(
                3,
                "suspended",
                json!({"media.class": "Stream/Input/Audio", "application.process.binary": "chrome",
                       "application.process.id": 43}),
            ),
            node(
                4,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "spotify",
                       "application.process.id": 44}),
            ),
            // Hex itself.
            node(
                5,
                "running",
                json!({"media.class": "Stream/Input/Audio", "application.process.binary": "hex",
                       "application.process.id": 7}),
            ),
            // Properties only on the client.
            json!({"id": 9, "type": "PipeWire:Interface:Client",
                   "info": {"props": {"application.process.binary": "firefox", "application.process.id": "45"}}}),
            node(
                6,
                "running",
                json!({"media.class": "Stream/Input/Audio", "client.id": 9}),
            ),
        ] {
            graph.apply(&object);
        }
        let users = graph.microphone_users(7);
        let binaries = users
            .iter()
            .map(|user| user.bundle_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(binaries, ["firefox", "zoom"]);
        assert_eq!(users[1].name, "ZOOM VoiceEngine");
        assert_eq!(users[0].pid, 45);

        graph.apply(&json!({"id": 1, "info": null}));
        let users = graph.microphone_users(7);
        assert_eq!(users.len(), 1);
    }

    /// Before its first dump an empty graph must not answer: it would look
    /// like no app is playing or recording, which would stop a chosen app's
    /// captures and hide every app from the pane.
    #[test]
    fn a_graph_answers_only_after_its_first_dump() {
        let mut graph = Graph::default();
        assert!(!graph.ready);
        graph.apply(&node(
            1,
            "running",
            json!({"media.class": "Stream/Output/Audio", "application.process.binary": "spotify",
                   "application.process.id": 41}),
        ));
        assert!(graph.ready, "the first dump makes the graph answer");
    }

    #[test]
    fn running_output_streams_name_their_apps_and_node_targets() {
        let mut graph = Graph::default();
        for object in [
            node(
                11,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "chrome",
                       "application.process.id": 41, "application.name": "Google Chrome",
                       "object.serial": 88}),
            ),
            // A second stream of the same app that reports no name: the app
            // keeps the name its other stream reports, whichever node the
            // graph yields first.
            node(
                12,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "chrome",
                       "application.process.id": 41, "object.serial": "89"}),
            ),
            // A serial-less node is named by its graph id.
            node(
                13,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "spotify",
                       "application.process.id": 42}),
            ),
            // Its process properties are on the client.
            json!({"id": 20, "type": "PipeWire:Interface:Client",
                   "info": {"props": {"application.process.binary": "teams-for-linux",
                                      "application.process.id": "43",
                                      "application.name": "Teams"}}}),
            node(
                14,
                "running",
                json!({"media.class": "Stream/Output/Audio", "client.id": 20, "object.serial": 90}),
            ),
            // Suspended, a capture stream, and Hex itself are not sources.
            node(
                15,
                "suspended",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "discord",
                       "application.process.id": 44}),
            ),
            node(
                16,
                "running",
                json!({"media.class": "Stream/Input/Audio", "application.process.binary": "zoom",
                       "application.process.id": 45}),
            ),
            node(
                17,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "hex",
                       "application.process.id": 7}),
            ),
        ] {
            graph.apply(&object);
        }
        let apps = graph.playing_applications(7);
        assert_eq!(
            apps,
            [
                PlayingApplication {
                    id: "chrome".into(),
                    name: "Google Chrome".into(),
                    targets: vec!["88".into(), "89".into()],
                },
                PlayingApplication {
                    id: "spotify".into(),
                    name: "spotify".into(),
                    targets: vec!["13".into()],
                },
                PlayingApplication {
                    id: "teams-for-linux".into(),
                    name: "Teams".into(),
                    targets: vec!["90".into()],
                },
            ]
        );
    }

    /// Hex's own sounds are never an app the pane offers: its in-process
    /// playback, a `pw-cat` playback child of it (Hex Microphone), and any
    /// node carrying one of Hex's names.
    #[test]
    fn hex_own_playback_is_never_an_app_to_choose() {
        let mut graph = Graph::default();
        for object in [
            node(
                1,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "hex",
                       "application.process.id": 7, "node.name": "hex"}),
            ),
            // Hex Microphone's `pw-cat` playback: a child of Hex that also
            // carries one of its names.
            node(
                2,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "pw-cat",
                       "application.process.id": 77, "application.name": "Hex",
                       "node.name": "hex_microphone"}),
            ),
            // Another child of Hex, known only by its process tree.
            node(
                3,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "pw-cat",
                       "application.process.id": 78}),
            ),
            node(
                4,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "spotify",
                       "application.process.id": 41}),
            ),
            // A third-party app whose name merely starts with Hex's own is
            // not Hex: only the exact names are.
            node(
                5,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "hex-player",
                       "application.process.id": 42, "node.name": "hex-player"}),
            ),
        ] {
            graph.apply(&object);
        }
        let apps = graph.playing_applications_with(7, |pid| matches!(pid, 77 | 78).then_some(7));
        assert_eq!(
            apps,
            [
                PlayingApplication {
                    id: "hex-player".into(),
                    name: "hex-player".into(),
                    targets: vec!["5".into()],
                },
                PlayingApplication {
                    id: "spotify".into(),
                    name: "spotify".into(),
                    targets: vec!["4".into()],
                },
            ]
        );
        // The numbered meeting monitors are Hex's, the bare name too.
        for name in [
            "hex-meeting-system-audio",
            "hex-meeting-system-audio-3",
            "hex_microphone",
            "hex",
        ] {
            assert!(is_hex_node_name(name), "{name}");
        }
        for name in [
            "hex-player",
            "hex_",
            "hex-meeting-system-audiox",
            "hex-meeting-system-audio-",
        ] {
            assert!(!is_hex_node_name(name), "{name}");
        }
    }

    /// A muted call can leave the microphone and still play: a running
    /// output stream of its app is the safeguard that keeps it recording
    /// (#835). A suspended stream, or one of another app, is not.
    #[test]
    fn running_output_streams_say_whether_an_app_still_plays() {
        let mut graph = Graph::default();
        for object in [
            node(
                1,
                "running",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "zoom",
                       "application.process.id": 41}),
            ),
            node(
                2,
                "suspended",
                json!({"media.class": "Stream/Output/Audio", "application.process.binary": "discord",
                       "application.process.id": 42}),
            ),
            // Its process properties are on the client.
            json!({"id": 9, "type": "PipeWire:Interface:Client",
                   "info": {"props": {"application.process.binary": "teams-for-linux",
                                      "application.process.id": "43"}}}),
            node(
                3,
                "running",
                json!({"media.class": "Stream/Output/Audio", "client.id": 9}),
            ),
            // A capture stream, whose output it is not.
            node(
                4,
                "running",
                json!({"media.class": "Stream/Input/Audio", "application.process.binary": "chrome",
                       "application.process.id": 44}),
            ),
        ] {
            graph.apply(&object);
        }
        assert!(graph.playing_output("us.zoom.xos"));
        assert!(graph.playing_output("com.microsoft.teams2"));
        assert!(!graph.playing_output("com.hnc.Discord"), "suspended");
        assert!(!graph.playing_output("com.google.Chrome"), "capturing");
        let users = graph
            .microphone_users(7)
            .into_iter()
            .map(|user| user.bundle_id)
            .collect::<Vec<_>>();
        assert_eq!(users, ["chrome"], "playback is never a microphone user");
    }
}
