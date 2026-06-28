use std::{collections::BTreeMap, path::Path};

use crate::{
    config::{
        ComposeGeneratedOverrideHashInput, StartupCommandHashInput,
        canonical::{CanonicalWriter, sha256_hex},
        resolved::ResolvedDevcontainerSource,
        types::MountType,
    },
    docker::mounts::DockerMountSpec,
    runtime::compose_ports::ComposePublishedPortOverride,
    up::types::UpPlan,
};

pub(super) fn compose_generated_override_hash_input(
    path: &Path,
    plan: &UpPlan,
    mounts: &[DockerMountSpec],
    startup_command: Option<&StartupCommandHashInput>,
    published_port_override: &ComposePublishedPortOverride,
) -> Option<ComposeGeneratedOverrideHashInput> {
    let Some(ResolvedDevcontainerSource::Compose(compose)) = &plan.config.devcontainer.source
    else {
        return None;
    };

    let mut writer = CanonicalWriter::default();
    writer.object("ComposeGeneratedOverrideContent", |writer| {
        write_generated_override_service_fields(writer, &compose.service, plan);
        writer.field("mounts", |writer| {
            write_generated_override_mounts(writer, mounts);
        });
        writer.field("startup_command", |writer| {
            write_generated_override_startup_command(writer, startup_command);
        });
        writer.field("published_port_override", |writer| {
            write_generated_override_published_ports(writer, published_port_override);
        });
    });

    Some(ComposeGeneratedOverrideHashInput {
        path: path.display().to_string(),
        content_hash: sha256_hex(writer.finish().as_bytes()),
    })
}

fn write_generated_override_service_fields(
    writer: &mut CanonicalWriter,
    service: &str,
    plan: &UpPlan,
) {
    writer.field("primary_service", |writer| writer.string(service));
    writer.field("image", |writer| {
        writer.string(generated_override_semantic_image(plan));
    });
    writer.field("pull_policy", |writer| {
        if generated_override_semantic_pull_policy_never(plan) {
            writer.string("never");
        } else {
            writer.none();
        }
    });
    writer.field("labels", |writer| {
        let labels = generated_override_semantic_labels(&plan.resources.labels);
        writer.map(labels.iter(), |writer, value| writer.string(value));
    });
    writer.field("environment", |writer| {
        let environment = generated_override_semantic_environment(plan);
        writer.map(environment.iter(), |writer, value| writer.string(value));
    });
    writer.field("container_user", |writer| {
        writer.option_string(plan.config.devcontainer.container_user.as_deref());
    });
    writer.field("init", |writer| {
        write_option_bool(writer, plan.config.devcontainer.init);
    });
    writer.field("privileged", |writer| {
        write_option_bool(writer, plan.config.devcontainer.privileged);
    });
    writer.field("cap_add", |writer| {
        writer.seq(plan.config.devcontainer.cap_add.iter(), |writer, value| {
            writer.string(value);
        });
    });
    writer.field("security_opt", |writer| {
        writer.seq(
            plan.config.devcontainer.security_opt.iter(),
            |writer, value| {
                writer.string(value);
            },
        );
    });
}

fn generated_override_semantic_environment(plan: &UpPlan) -> BTreeMap<String, String> {
    plan.config
        .devcontainer
        .container_env
        .iter()
        .map(|(key, value)| {
            let value = if plan.sensitive_container_env.contains_key(key) {
                "<localEnv-derived-value>".to_owned()
            } else {
                value.clone()
            };
            (key.clone(), value)
        })
        .collect()
}

fn write_generated_override_mounts(writer: &mut CanonicalWriter, mounts: &[DockerMountSpec]) {
    let inputs = crate::up::mount_hash_inputs(mounts);
    writer.seq(inputs.iter(), |writer, mount| {
        writer.object("Mount", |writer| {
            writer.field("source", |writer| {
                writer.option_string(mount.source.as_deref());
            });
            writer.field("target", |writer| writer.string(&mount.target));
            writer.field("type", |writer| {
                writer.string(match mount.mount_type {
                    MountType::Bind => "bind",
                    MountType::Volume => "volume",
                    MountType::Tmpfs => "tmpfs",
                });
            });
            writer.field("read_only", |writer| writer.bool(mount.read_only));
            writer.field("consistency", |writer| {
                writer.option_string(mount.consistency.as_deref());
            });
        });
    });
}

fn write_generated_override_startup_command(
    writer: &mut CanonicalWriter,
    startup_command: Option<&StartupCommandHashInput>,
) {
    match startup_command {
        Some(startup_command) => {
            writer.object("StartupCommand", |writer| {
                writer.field("entrypoint", |writer| {
                    writer.seq(startup_command.entrypoint.iter(), |writer, value| {
                        writer.string(value);
                    });
                });
                writer.field("command", |writer| {
                    writer.seq(startup_command.command.iter(), |writer, value| {
                        writer.string(value);
                    });
                });
            });
        }
        None => writer.none(),
    }
}

fn write_generated_override_published_ports(
    writer: &mut CanonicalWriter,
    published_port_override: &ComposePublishedPortOverride,
) {
    writer.map(published_port_override.services(), |writer, ports| {
        writer.seq(ports.iter(), |writer, port| {
            writer.map(port.iter(), |writer, value| {
                writer.json_value(value);
            });
        });
    });
}

fn generated_override_semantic_image(plan: &UpPlan) -> &str {
    if plan.image == plan.base_image {
        &plan.image
    } else {
        "<decune-generated-image>"
    }
}

fn generated_override_semantic_pull_policy_never(plan: &UpPlan) -> bool {
    plan.image != plan.base_image
}

fn write_option_bool(writer: &mut CanonicalWriter, value: Option<bool>) {
    match value {
        Some(value) => writer.bool(value),
        None => writer.none(),
    }
}

fn generated_override_semantic_labels(
    labels: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    labels
        .iter()
        .filter(|(key, _)| generated_override_label_is_semantic_hash_input(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn generated_override_label_is_semantic_hash_input(key: &str) -> bool {
    key != "decune.config_hash" && !key.starts_with("com.docker.compose.")
}
