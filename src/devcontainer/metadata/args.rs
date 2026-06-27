use anyhow::{Result, anyhow};

use super::DevcontainerRunArg;

pub(super) fn validate_build_options(values: &[String]) -> Result<()> {
    let mut index = 0;

    while index < values.len() {
        let current = &values[index];
        if current.is_empty() {
            return Err(anyhow!("build.options entries must not be empty"));
        }
        if current == "--" {
            return Err(anyhow!("build.options must not contain --"));
        }
        if !current.starts_with('-') {
            return Err(anyhow!(
                "build.options entries must be Docker build options, not context paths or values: {current}"
            ));
        }

        let option = current
            .split_once('=')
            .map_or(current.as_str(), |(option, _)| option);
        if is_reserved_build_option(option) {
            return Err(anyhow!(
                "build.options must not specify decune-managed Docker build option: {option}"
            ));
        }
        if current.contains('=') {
            if current.ends_with('=') {
                return Err(anyhow!(
                    "build.options option value must not be empty: {option}"
                ));
            }
            index += 1;
            continue;
        }

        if build_option_allows_separate_value(option) {
            let value = values
                .get(index + 1)
                .ok_or_else(|| anyhow!("build.options option requires a value: {option}"))?;
            if value.is_empty() || value == "--" || value.starts_with('-') {
                return Err(anyhow!(
                    "build.options option requires a value before another option: {option}"
                ));
            }
            index += 2;
            continue;
        }

        index += 1;
    }

    Ok(())
}

fn is_reserved_build_option(option: &str) -> bool {
    matches!(
        option,
        "-f" | "-t"
            | "-o"
            | "--file"
            | "--tag"
            | "--label"
            | "--build-arg"
            | "--target"
            | "--cache-from"
            | "--rm"
            | "--force-rm"
            | "--no-cache"
            | "--pull"
            | "--iidfile"
            | "--metadata-file"
            | "--output"
    ) || option.starts_with("-f")
        || option.starts_with("-t")
        || option.starts_with("-o")
}

fn build_option_allows_separate_value(option: &str) -> bool {
    matches!(
        option,
        "--add-host"
            | "--allow"
            | "--attest"
            | "--build-context"
            | "--cache-to"
            | "--cgroup-parent"
            | "--network"
            | "--platform"
            | "--progress"
            | "--secret"
            | "--shm-size"
            | "--ssh"
    )
}

pub(super) fn normalize_run_args(values: &[String]) -> Result<Vec<DevcontainerRunArg>> {
    let mut args = Vec::new();
    let mut index = 0;

    while index < values.len() {
        let current = &values[index];
        if current.is_empty() {
            return Err(anyhow!("Unsupported runArgs option: {current}"));
        }
        if let Some((option, value)) = current.split_once('=') {
            args.push(run_arg_with_value(option, value.to_owned())?);
            index += 1;
            continue;
        }

        match current.as_str() {
            "--init" => args.push(DevcontainerRunArg::Init),
            "--privileged" => args.push(DevcontainerRunArg::Privileged),
            option if run_arg_allows_separate_value(option) => {
                let value = required_run_arg_value(values, current, index)?;
                args.push(run_arg_with_value(current, value)?);
                index += 1;
            }
            option if is_reserved_run_arg_option(option) => {
                return Err(anyhow!(
                    "Unsupported runArgs option controlled by decune: {option}"
                ));
            }
            option => return Err(anyhow!("Unsupported runArgs option: {option}")),
        }

        index += 1;
    }

    Ok(args)
}

fn required_run_arg_value(values: &[String], option: &str, index: usize) -> Result<String> {
    let value = values
        .get(index + 1)
        .ok_or_else(|| anyhow!("Missing value for runArgs option {option}"))?;

    if value.is_empty() || value.starts_with('-') {
        return Err(anyhow!("Missing value for runArgs option {option}"));
    }

    Ok(value.clone())
}

fn run_arg_with_value(option: &str, value: String) -> Result<DevcontainerRunArg> {
    if value.is_empty() {
        return Err(anyhow!("Missing value for runArgs option {option}"));
    }

    match option {
        "--cap-add" => Ok(DevcontainerRunArg::CapAdd(value)),
        "--security-opt" => Ok(DevcontainerRunArg::SecurityOpt(value)),
        "--add-host" => Ok(DevcontainerRunArg::AddHost(value)),
        "--dns" => Ok(DevcontainerRunArg::Dns(value)),
        "--dns-search" => Ok(DevcontainerRunArg::DnsSearch(value)),
        "--network" | "--network-alias" | "--hostname" | "--device" | "--group-add"
        | "--ulimit" | "--ipc" | "--shm-size" | "--gpus" => Ok(DevcontainerRunArg::Passthrough {
            option: option.to_owned(),
            value,
        }),
        "--init" | "--privileged" => {
            Err(anyhow!("runArgs option {option} does not accept a value"))
        }
        _ if is_reserved_run_arg_option(option) => Err(anyhow!(
            "Unsupported runArgs option controlled by decune: {option}"
        )),
        _ => Err(anyhow!("Unsupported runArgs option: {option}")),
    }
}

fn run_arg_allows_separate_value(option: &str) -> bool {
    matches!(
        option,
        "--cap-add"
            | "--security-opt"
            | "--add-host"
            | "--dns"
            | "--dns-search"
            | "--network"
            | "--network-alias"
            | "--hostname"
            | "--device"
            | "--group-add"
            | "--ulimit"
            | "--ipc"
            | "--shm-size"
            | "--gpus"
    )
}

fn is_reserved_run_arg_option(option: &str) -> bool {
    matches!(
        option,
        "--name"
            | "--entrypoint"
            | "-e"
            | "--env"
            | "--env-file"
            | "-u"
            | "--user"
            | "-w"
            | "--workdir"
            | "-v"
            | "--volume"
            | "--mount"
            | "--tmpfs"
            | "--volumes-from"
            | "-p"
            | "--publish"
            | "-P"
            | "--publish-all"
            | "--expose"
            | "--label"
            | "--label-file"
            | "--rm"
            | "--detach"
            | "-d"
            | "--restart"
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::{DevcontainerBuild, DevcontainerRunArg, DevcontainerSource, parse_metadata};
    use crate::devcontainer::mounts::DevcontainerMount;

    #[test]
    fn parses_dockerfile_based_metadata() {
        let metadata = parse_metadata(json!({
            "build": {
                "dockerfile": "Dockerfile",
                "context": "..",
                "args": {
                    "VARIANT": "bookworm"
                },
                "options": [
                    "--platform=linux/amd64",
                    "--ssh=default",
                    "--secret",
                    "id=npm,env=NPM_TOKEN",
                    "--add-host=host.docker.internal:host-gateway",
                    "--network",
                    "host"
                ],
                "target": "dev",
                "cacheFrom": ["type=registry,ref=example.test/cache"]
            },
            "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
            "mounts": [
                "source=decune-cache,target=/cache,type=volume"
            ]
        }))
        .unwrap();

        assert_eq!(
            metadata.source(),
            Some(&DevcontainerSource::Dockerfile(DevcontainerBuild {
                dockerfile: "Dockerfile".to_owned(),
                context: Some("..".to_owned()),
                args: [("VARIANT".to_owned(), "bookworm".to_owned())].into(),
                options: vec![
                    "--platform=linux/amd64".to_owned(),
                    "--ssh=default".to_owned(),
                    "--secret".to_owned(),
                    "id=npm,env=NPM_TOKEN".to_owned(),
                    "--add-host=host.docker.internal:host-gateway".to_owned(),
                    "--network".to_owned(),
                    "host".to_owned(),
                ],
                target: Some("dev".to_owned()),
                cache_from: vec!["type=registry,ref=example.test/cache".to_owned()],
            }))
        );
        assert_eq!(
            metadata.workspace_mount(),
            Some("source=${localWorkspaceFolder},target=/workspace,type=bind")
        );
        assert_eq!(
            metadata.mounts(),
            &[DevcontainerMount::String(
                "source=decune-cache,target=/cache,type=volume".to_owned()
            )]
        );
    }

    #[test]
    fn rejects_reserved_dockerfile_build_options() {
        for option in [
            "-f",
            "--file=Dockerfile",
            "--file",
            "-texample:test",
            "--tag=example:test",
            "--label",
            "--build-arg=TOKEN=value",
            "--target",
            "--cache-from=example/cache",
            "--rm",
            "--force-rm",
            "--no-cache",
            "--pull",
            "--output=type=local,dest=out",
            "--metadata-file",
        ] {
            let error = parse_metadata(json!({
                "build": {
                    "dockerfile": "Dockerfile",
                    "options": [option]
                }
            }))
            .unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains("decune-managed Docker build option"),
                "unexpected error for {option}: {error}"
            );
        }
    }

    #[test]
    fn rejects_build_options_context_paths_and_missing_values() {
        for options in [
            json!(["."]),
            json!(["--network"]),
            json!(["--network", "--pull"]),
            json!(["--platform="]),
            json!(["--"]),
            json!([""]),
        ] {
            let error = parse_metadata(json!({
                "build": {
                    "dockerfile": "Dockerfile",
                    "options": options
                }
            }))
            .unwrap_err();

            assert!(
                error.to_string().contains("build.options"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn build_args_must_be_strings() {
        let error = parse_metadata(json!({
            "build": {
                "dockerfile": "Dockerfile",
                "args": {
                    "UID": 1000
                }
            }
        }))
        .unwrap_err();

        assert!(error.to_string().contains("build.args"));
    }

    #[test]
    fn supported_run_args_are_normalized() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "runArgs": [
                "--init",
                "--privileged",
                "--cap-add=SYS_PTRACE",
                "--security-opt", "seccomp=unconfined",
                "--add-host", "host.docker.internal:host-gateway",
                "--dns", "1.1.1.1",
                "--dns-search=example.test",
                "--network", "host",
                "--network-alias=api",
                "--hostname=devbox",
                "--device", "/dev/fuse",
                "--group-add=video",
                "--ulimit", "nofile=1024:2048",
                "--ipc=host",
                "--shm-size", "1g",
                "--gpus", "all"
            ]
        }))
        .unwrap();

        assert_eq!(
            metadata.run_args(),
            &[
                DevcontainerRunArg::Init,
                DevcontainerRunArg::Privileged,
                DevcontainerRunArg::CapAdd("SYS_PTRACE".to_owned()),
                DevcontainerRunArg::SecurityOpt("seccomp=unconfined".to_owned()),
                DevcontainerRunArg::AddHost("host.docker.internal:host-gateway".to_owned()),
                DevcontainerRunArg::Dns("1.1.1.1".to_owned()),
                DevcontainerRunArg::DnsSearch("example.test".to_owned()),
                DevcontainerRunArg::Passthrough {
                    option: "--network".to_owned(),
                    value: "host".to_owned()
                },
                DevcontainerRunArg::Passthrough {
                    option: "--network-alias".to_owned(),
                    value: "api".to_owned()
                },
                DevcontainerRunArg::Passthrough {
                    option: "--hostname".to_owned(),
                    value: "devbox".to_owned()
                },
                DevcontainerRunArg::Passthrough {
                    option: "--device".to_owned(),
                    value: "/dev/fuse".to_owned()
                },
                DevcontainerRunArg::Passthrough {
                    option: "--group-add".to_owned(),
                    value: "video".to_owned()
                },
                DevcontainerRunArg::Passthrough {
                    option: "--ulimit".to_owned(),
                    value: "nofile=1024:2048".to_owned()
                },
                DevcontainerRunArg::Passthrough {
                    option: "--ipc".to_owned(),
                    value: "host".to_owned()
                },
                DevcontainerRunArg::Passthrough {
                    option: "--shm-size".to_owned(),
                    value: "1g".to_owned()
                },
                DevcontainerRunArg::Passthrough {
                    option: "--gpus".to_owned(),
                    value: "all".to_owned()
                },
            ]
        );
    }

    #[test]
    fn reserved_run_args_are_rejected() {
        for run_args in [
            json!(["--name", "devcontainer"]),
            json!(["--publish", "3000:3000"]),
            json!(["-p", "3000:3000"]),
            json!(["--mount", "type=bind,source=/tmp,target=/tmp"]),
            json!(["--user", "vscode"]),
            json!(["--env", "RUST_LOG=debug"]),
            json!(["--env-file", ".env"]),
            json!(["--entrypoint", "/bin/sh"]),
            json!(["--label", "x=y"]),
            json!(["--rm"]),
            json!(["--detach"]),
            json!(["--restart", "always"]),
        ] {
            let error = parse_metadata(json!({
                "image": "ubuntu:24.04",
                "runArgs": run_args
            }))
            .unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains("Unsupported runArgs option controlled by decune")
            );
        }
    }

    #[test]
    fn unsupported_run_args_are_rejected() {
        let error = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "runArgs": ["--cpus", "2"]
        }))
        .unwrap_err();

        assert!(error.to_string().contains("Unsupported runArgs option"));
        assert!(!error.to_string().contains("controlled by decune"));
    }

    #[test]
    fn run_args_reserved_equals_form_is_rejected() {
        for run_args in [json!(["--env=RUST_LOG=debug"]), json!(["--label=x=y"])] {
            let error = parse_metadata(json!({
                "image": "ubuntu:24.04",
                "runArgs": run_args
            }))
            .unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains("Unsupported runArgs option controlled by decune")
            );
        }
    }

    #[test]
    fn run_args_missing_values_are_rejected() {
        for run_args in [
            json!(["--cap-add"]),
            json!(["--security-opt"]),
            json!(["--add-host"]),
            json!(["--dns"]),
            json!(["--dns-search"]),
            json!(["--network"]),
            json!(["--network-alias"]),
            json!(["--hostname"]),
            json!(["--device"]),
            json!(["--group-add"]),
            json!(["--ulimit"]),
            json!(["--ipc"]),
            json!(["--shm-size"]),
            json!(["--gpus"]),
        ] {
            let error = parse_metadata(json!({
                "image": "ubuntu:24.04",
                "runArgs": run_args
            }))
            .unwrap_err();

            assert!(error.to_string().contains("Missing value"));
        }
    }

    #[test]
    fn run_args_value_options_reject_following_options_as_values() {
        for run_args in [
            json!(["--cap-add", "--init"]),
            json!(["--security-opt", "--privileged"]),
            json!(["--add-host", "--dns", "1.1.1.1"]),
            json!(["--dns", "--dns-search", "example.test"]),
            json!(["--dns-search", "--init"]),
            json!(["--network", "--hostname", "devbox"]),
            json!(["--device", "--group-add", "video"]),
        ] {
            let error = parse_metadata(json!({
                "image": "ubuntu:24.04",
                "runArgs": run_args
            }))
            .unwrap_err();

            assert!(error.to_string().contains("Missing value"));
        }
    }

    #[test]
    fn run_args_boolean_options_reject_values() {
        for run_args in [
            json!(["--init=true"]),
            json!(["--privileged=false"]),
            json!(["--init", "true"]),
        ] {
            let error = parse_metadata(json!({
                "image": "ubuntu:24.04",
                "runArgs": run_args
            }))
            .unwrap_err();

            assert!(
                error.to_string().contains("does not accept a value")
                    || error.to_string().contains("Unsupported runArgs option")
            );
        }
    }
}
