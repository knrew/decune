//! テスト用の Docker CLI の代わり。コンテナ、volume、network の状態を持ち、`docker` の argv に
//! 状態で答える。返す答えを呼び出しの順に並べる `FakeRuntimeCommand` と違い、呼び出しの順序に
//! よらない結果(何が消え、何が残ったか)を確かめるのに使う。
//!
//! 再現する Docker の振る舞いは、decune の remove が頼るものに限る。
//! - `volume rm` は、実行中か停止中かを問わず、どれかのコンテナが mount している volume を
//!   `volume is in use` で拒否する。
//! - `rm --volumes` は、そのコンテナの匿名 volume(`com.docker.volume.anonymous` のラベルを
//!   持つもの)のうち、他のコンテナが mount していないものだけを消す。

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::runtime::command::{RuntimeCommand, RuntimeCommandRunner, RuntimeOutput, RuntimeStdio};

pub(crate) const ANONYMOUS_VOLUME_LABEL: &str = "com.docker.volume.anonymous";

#[derive(Clone, Default)]
pub(crate) struct FakeDocker {
    state: Arc<Mutex<FakeDockerState>>,
}

#[derive(Default)]
struct FakeDockerState {
    containers: BTreeMap<String, FakeContainer>,
    volumes: BTreeMap<String, BTreeMap<String, String>>,
    networks: BTreeMap<String, BTreeMap<String, String>>,
    commands: Vec<Vec<String>>,
}

#[derive(Clone)]
struct FakeContainer {
    name: String,
    labels: BTreeMap<String, String>,
    running: bool,
    volumes: Vec<String>,
}

impl FakeDocker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn add_volume(&self, name: &str, labels: &[(&str, &str)]) {
        self.state
            .lock()
            .unwrap()
            .volumes
            .insert(name.to_owned(), label_map(labels));
    }

    pub(crate) fn add_network(&self, name: &str, labels: &[(&str, &str)]) {
        self.state
            .lock()
            .unwrap()
            .networks
            .insert(name.to_owned(), label_map(labels));
    }

    /// 停止中のコンテナを足す。`volumes` に挙げた volume は、先に `add_volume` で作っておく。
    pub(crate) fn add_container(&self, id: &str, labels: &[(&str, &str)], volumes: &[&str]) {
        let mut state = self.state.lock().unwrap();
        for volume in volumes {
            assert!(
                state.volumes.contains_key(*volume),
                "fake container {id} mounts missing volume {volume}"
            );
        }
        state.containers.insert(
            id.to_owned(),
            FakeContainer {
                name: id.to_owned(),
                labels: label_map(labels),
                running: false,
                volumes: volumes.iter().map(|volume| (*volume).to_owned()).collect(),
            },
        );
    }

    pub(crate) fn volume_exists(&self, name: &str) -> bool {
        self.state.lock().unwrap().volumes.contains_key(name)
    }

    pub(crate) fn container_exists(&self, id: &str) -> bool {
        self.state.lock().unwrap().containers.contains_key(id)
    }

    pub(crate) fn network_exists(&self, name: &str) -> bool {
        self.state.lock().unwrap().networks.contains_key(name)
    }

    /// Docker の状態を変える呼び出し(削除と停止)の argv。
    pub(crate) fn mutating_commands(&self) -> Vec<Vec<String>> {
        self.state
            .lock()
            .unwrap()
            .commands
            .iter()
            .filter(|args| is_mutating(args))
            .cloned()
            .collect()
    }

    fn handle(&self, args: &[String]) -> Result<RuntimeOutput> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake docker state lock is poisoned: {error}"))?;
        state.commands.push(args.to_vec());
        let words = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = match words.as_slice() {
            ["ps", rest @ ..] => state.list_containers(rest),
            ["container", "inspect", ids @ ..] => state.inspect_containers(ids),
            ["volume", "ls", rest @ ..] => lines(&names_matching(&state.volumes, rest)),
            ["volume", "inspect", names @ ..] => state.inspect_volumes(names),
            ["volume", "rm", .., name] => state.remove_volume(name),
            ["network", "ls", rest @ ..] => lines(&names_matching(&state.networks, rest)),
            ["network", "rm", name] => {
                state.networks.remove(*name);
                success(format!("{name}\n"))
            }
            ["stop", .., id] => {
                if let Some(container) = state.containers.get_mut(*id) {
                    container.running = false;
                }
                success(format!("{id}\n"))
            }
            ["rm", options @ .., id] => state.remove_container(id, options.contains(&"--volumes")),
            ["image", "ls", ..] => success(String::new()),
            _ => bail!("unexpected fake docker command: {}", args.join(" ")),
        };
        drop(state);
        Ok(output)
    }
}

impl FakeDockerState {
    fn list_containers(&self, args: &[&str]) -> RuntimeOutput {
        let filters = label_filters(args);
        let include_stopped = args.contains(&"--all");
        lines(
            &self
                .containers
                .iter()
                .filter(|(_, container)| include_stopped || container.running)
                .filter(|(_, container)| matches_filters(&container.labels, &filters))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
        )
    }

    fn inspect_containers(&self, ids: &[&str]) -> RuntimeOutput {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for id in ids.iter().filter(|id| **id != "--") {
            match self.containers.get(*id) {
                Some(container) => found.push(container_json(id, container)),
                None => missing.push(format!("Error: No such container: {id}\n")),
            }
        }
        partial_output(&found, &missing)
    }

    fn inspect_volumes(&self, names: &[&str]) -> RuntimeOutput {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for name in names.iter().filter(|name| **name != "--") {
            match self.volumes.get(*name) {
                Some(labels) => found.push(json!({"Name": name, "Labels": labels})),
                None => missing.push(format!("Error: No such volume: {name}\n")),
            }
        }
        partial_output(&found, &missing)
    }

    fn remove_volume(&mut self, name: &str) -> RuntimeOutput {
        if !self.volumes.contains_key(name) {
            return failure(format!(
                "Error response from daemon: get {name}: no such volume\n"
            ));
        }
        if let Some(user) = self
            .containers
            .iter()
            .find(|(_, container)| container.volumes.iter().any(|volume| volume == name))
            .map(|(id, _)| id)
        {
            return failure(format!(
                "Error response from daemon: remove {name}: volume is in use - [{user}]\n"
            ));
        }
        self.volumes.remove(name);
        success(format!("{name}\n"))
    }

    fn remove_container(&mut self, id: &str, remove_anonymous_volumes: bool) -> RuntimeOutput {
        let Some(container) = self.containers.remove(id) else {
            return failure(format!(
                "Error response from daemon: No such container: {id}\n"
            ));
        };
        if remove_anonymous_volumes {
            for volume in &container.volumes {
                let anonymous = self
                    .volumes
                    .get(volume)
                    .is_some_and(|labels| labels.contains_key(ANONYMOUS_VOLUME_LABEL));
                let used = self
                    .containers
                    .values()
                    .any(|other| other.volumes.contains(volume));
                if anonymous && !used {
                    self.volumes.remove(volume);
                }
            }
        }
        success(format!("{id}\n"))
    }
}

impl RuntimeCommandRunner for FakeDocker {
    fn run_capture<'a>(
        &'a self,
        command: RuntimeCommand,
    ) -> Pin<Box<dyn Future<Output = Result<RuntimeOutput>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(command.program(), "docker");
            self.handle(command.args_vec())
        })
    }

    fn run_capture_with_stdin<'a>(
        &'a self,
        command: RuntimeCommand,
        _stdin: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<RuntimeOutput>> + Send + 'a>> {
        self.run_capture(command)
    }

    fn run_status<'a>(
        &'a self,
        command: RuntimeCommand,
        _stdio: RuntimeStdio,
    ) -> Pin<Box<dyn Future<Output = Result<i32>> + Send + 'a>> {
        Box::pin(async move {
            self.handle(command.args_vec())
                .map(|output| output.exit_code)
        })
    }
}

fn is_mutating(args: &[String]) -> bool {
    let words = args.iter().map(String::as_str).collect::<Vec<_>>();
    matches!(
        words.as_slice(),
        ["stop" | "rm", ..] | ["volume" | "network" | "image", "rm", ..]
    )
}

fn label_map(labels: &[(&str, &str)]) -> BTreeMap<String, String> {
    labels
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn label_filters<'a>(args: &[&'a str]) -> Vec<(&'a str, Option<&'a str>)> {
    args.windows(2)
        .filter(|pair| pair[0] == "--filter")
        .filter_map(|pair| pair[1].strip_prefix("label="))
        .map(|filter| match filter.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (filter, None),
        })
        .collect()
}

fn matches_filters(labels: &BTreeMap<String, String>, filters: &[(&str, Option<&str>)]) -> bool {
    filters.iter().all(|(key, value)| {
        value.map_or_else(
            || labels.contains_key(*key),
            |value| labels.get(*key).is_some_and(|actual| actual == value),
        )
    })
}

fn names_matching(
    resources: &BTreeMap<String, BTreeMap<String, String>>,
    args: &[&str],
) -> Vec<String> {
    let filters = label_filters(args);
    resources
        .iter()
        .filter(|(_, labels)| matches_filters(labels, &filters))
        .map(|(name, _)| name.clone())
        .collect()
}

fn container_json(id: &str, container: &FakeContainer) -> Value {
    let mounts = container
        .volumes
        .iter()
        .map(|volume| {
            json!({
                "Type": "volume",
                "Name": volume,
                "Source": format!("/var/lib/docker/volumes/{volume}/_data"),
                "Destination": format!("/mnt/{volume}"),
                "RW": true,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "Id": id,
        "Name": format!("/{}", container.name),
        "Config": {"Labels": container.labels},
        "State": {"Running": container.running},
        "Mounts": mounts,
    })
}

fn lines(values: &[String]) -> RuntimeOutput {
    let mut stdout = values.join("\n");
    if !stdout.is_empty() {
        stdout.push('\n');
    }
    success(stdout)
}

/// 見つからないものがあっても、見つかったものを出力して終了コード 1 で終わる、Docker の
/// inspect の形。
fn partial_output(found: &[Value], missing: &[String]) -> RuntimeOutput {
    let exit_code = i32::from(!missing.is_empty());
    output(
        Value::Array(found.to_vec()).to_string(),
        missing.concat(),
        exit_code,
    )
}

fn success(stdout: String) -> RuntimeOutput {
    output(stdout, String::new(), 0)
}

fn failure(stderr: String) -> RuntimeOutput {
    output(String::new(), stderr, 1)
}

fn output(stdout: String, stderr: String, exit_code: i32) -> RuntimeOutput {
    RuntimeOutput {
        stdout: stdout.into_bytes(),
        stderr: stderr.into_bytes(),
        exit_code,
    }
}
