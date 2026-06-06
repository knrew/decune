use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command as HostCommand,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::join_all;
use serde_json::Value;

use crate::{
    config::{
        resolved::{ResolvedConfig, ResolvedHook},
        types::{Command, HookLocation},
        variables::VariableContext,
    },
    devcontainer::metadata::LifecycleProperty,
    docker::{
        client::DockerClient,
        dotfiles::setup_dotfiles,
        exec::{ExecCommandSpec, ExecOutput, exec_capture_output, resolve_exec_env},
        user::ResolvedRemoteUser,
    },
    host::credentials::{
        install_staged_host_gitconfig, remove_staged_host_gitconfig, setup_git_credentials,
        setup_github_cli_credentials,
    },
    state::{LifecycleCompletion, LifecycleState},
};

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleCommand {
    Shell(String),
    Args(Vec<String>),
    Parallel(BTreeMap<String, LifecycleCommand>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LifecycleDefinition {
    commands: BTreeMap<LifecycleStage, Vec<LifecycleCommand>>,
    wait_for: WaitFor,
}

impl LifecycleDefinition {
    #[allow(dead_code)]
    pub(crate) fn command(&self, stage: LifecycleStage) -> Option<&LifecycleCommand> {
        self.commands
            .get(&stage)
            .and_then(|commands| commands.first())
    }

    pub(crate) fn commands(&self, stage: LifecycleStage) -> &[LifecycleCommand] {
        self.commands.get(&stage).map(Vec::as_slice).unwrap_or(&[])
    }

    pub(crate) fn wait_for(&self) -> WaitFor {
        self.wait_for
    }

    pub(crate) fn merge_layer(&mut self, layer: LayerLifecycleDefinition) {
        for (stage, commands) in layer.commands {
            self.commands.entry(stage).or_default().extend(commands);
        }
        if let Some(wait_for) = layer.wait_for {
            self.wait_for = wait_for;
        }
    }

    #[cfg(test)]
    pub(crate) fn into_layer(self) -> Option<LayerLifecycleDefinition> {
        if self.commands.is_empty() {
            return None;
        }

        Some(LayerLifecycleDefinition {
            commands: self.commands,
            wait_for: Some(self.wait_for),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerLifecycleDefinition {
    commands: BTreeMap<LifecycleStage, Vec<LifecycleCommand>>,
    wait_for: Option<WaitFor>,
}

impl LayerLifecycleDefinition {
    pub(crate) fn into_resolved(self) -> LifecycleDefinition {
        LifecycleDefinition {
            commands: self.commands,
            wait_for: self.wait_for.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LifecycleStage {
    Initialize,
    OnCreate,
    UpdateContent,
    PostCreate,
    PostStart,
    PostAttach,
}

impl LifecycleStage {
    #[allow(dead_code)]
    pub(crate) fn execution_location(self) -> LifecycleExecutionLocation {
        match self {
            Self::Initialize => LifecycleExecutionLocation::Host,
            Self::OnCreate
            | Self::UpdateContent
            | Self::PostCreate
            | Self::PostStart
            | Self::PostAttach => LifecycleExecutionLocation::Container,
        }
    }

    pub(crate) fn property_name(self) -> &'static str {
        match self {
            Self::Initialize => "initializeCommand",
            Self::OnCreate => "onCreateCommand",
            Self::UpdateContent => "updateContentCommand",
            Self::PostCreate => "postCreateCommand",
            Self::PostStart => "postStartCommand",
            Self::PostAttach => "postAttachCommand",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleRunPath {
    New,
    Started,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookStage {
    BeforeInitialize,
    AfterInitialize,
    BeforeOnCreate,
    AfterOnCreate,
    BeforeUpdateContent,
    AfterUpdateContent,
    BeforePostCreate,
    AfterPostCreate,
    BeforePostStart,
    AfterPostStart,
    BeforePostAttach,
    AfterPostAttach,
}

impl HookStage {
    fn property_name(self) -> &'static str {
        match self {
            Self::BeforeInitialize => "before_initialize",
            Self::AfterInitialize => "after_initialize",
            Self::BeforeOnCreate => "before_on_create",
            Self::AfterOnCreate => "after_on_create",
            Self::BeforeUpdateContent => "before_update_content",
            Self::AfterUpdateContent => "after_update_content",
            Self::BeforePostCreate => "before_post_create",
            Self::AfterPostCreate => "after_post_create",
            Self::BeforePostStart => "before_post_start",
            Self::AfterPostStart => "after_post_start",
            Self::BeforePostAttach => "before_post_attach",
            Self::AfterPostAttach => "after_post_attach",
        }
    }

    fn default_location(self) -> HookLocation {
        match self {
            Self::BeforeInitialize | Self::AfterInitialize => HookLocation::Host,
            Self::BeforeOnCreate
            | Self::AfterOnCreate
            | Self::BeforeUpdateContent
            | Self::AfterUpdateContent
            | Self::BeforePostCreate
            | Self::AfterPostCreate
            | Self::BeforePostStart
            | Self::AfterPostStart
            | Self::BeforePostAttach
            | Self::AfterPostAttach => HookLocation::Container,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleStep {
    Hooks(HookStage),
    Lifecycle(LifecycleStage),
    ImagePreparation,
    ContainerCreate,
    HostDaemonStart,
    ContainerStart,
    DecuneSetup,
    PortForwardingStart,
    ShellAttach,
}

#[derive(Clone)]
pub(crate) struct LifecycleRunContext<'a> {
    pub(crate) client: &'a DockerClient,
    pub(crate) container: &'a str,
    pub(crate) config: &'a ResolvedConfig,
    pub(crate) workspace_root: &'a Path,
    pub(crate) workspace_basename: &'a str,
    pub(crate) workspace_id: &'a str,
    pub(crate) workspace_folder: &'a str,
    pub(crate) runtime_dir: &'a Path,
    pub(crate) remote_user: ResolvedRemoteUser,
}

pub(crate) struct PreparedLifecycleRunContext<'a> {
    client: &'a DockerClient,
    container: &'a str,
    config: &'a ResolvedConfig,
    workspace_root: &'a Path,
    workspace_folder: &'a str,
    remote_user: ResolvedRemoteUser,
    remote_process_env: BTreeMap<String, String>,
}

#[allow(dead_code)]
pub(crate) fn lifecycle_plan(path: LifecycleRunPath) -> Vec<LifecycleStep> {
    let mut plan = container_start_lifecycle_plan(path);
    plan.extend(attach_lifecycle_plan());
    plan
}

#[allow(dead_code)]
pub(crate) fn container_start_lifecycle_plan(path: LifecycleRunPath) -> Vec<LifecycleStep> {
    container_start_lifecycle_plan_with_state(path, &LifecycleState::default())
}

pub(crate) fn container_start_lifecycle_plan_with_state(
    path: LifecycleRunPath,
    state: &LifecycleState,
) -> Vec<LifecycleStep> {
    match path {
        LifecycleRunPath::New => {
            let mut steps = vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ImagePreparation,
                LifecycleStep::ContainerCreate,
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
            ];
            push_creation_lifecycle_steps(
                &mut steps,
                *state,
                LifecycleCompletion::OnCreate,
                HookStage::BeforeOnCreate,
                LifecycleStage::OnCreate,
            );
            push_creation_lifecycle_steps(
                &mut steps,
                *state,
                LifecycleCompletion::UpdateContent,
                HookStage::BeforeUpdateContent,
                LifecycleStage::UpdateContent,
            );
            push_creation_lifecycle_steps(
                &mut steps,
                *state,
                LifecycleCompletion::PostCreate,
                HookStage::BeforePostCreate,
                LifecycleStage::PostCreate,
            );
            steps.extend([
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
            ]);
            steps
        }
        LifecycleRunPath::Started => vec![
            LifecycleStep::Hooks(HookStage::BeforeInitialize),
            LifecycleStep::Lifecycle(LifecycleStage::Initialize),
            LifecycleStep::Hooks(HookStage::AfterInitialize),
            LifecycleStep::ContainerStart,
            LifecycleStep::HostDaemonStart,
            LifecycleStep::DecuneSetup,
            LifecycleStep::Hooks(HookStage::BeforePostStart),
            LifecycleStep::Lifecycle(LifecycleStage::PostStart),
            LifecycleStep::Hooks(HookStage::AfterPostStart),
        ],
        LifecycleRunPath::Running => vec![
            LifecycleStep::Hooks(HookStage::BeforeInitialize),
            LifecycleStep::Lifecycle(LifecycleStage::Initialize),
            LifecycleStep::Hooks(HookStage::AfterInitialize),
            LifecycleStep::HostDaemonStart,
            LifecycleStep::DecuneSetup,
        ],
    }
}

fn push_creation_lifecycle_steps(
    steps: &mut Vec<LifecycleStep>,
    state: LifecycleState,
    completion: LifecycleCompletion,
    before_hook: HookStage,
    lifecycle_stage: LifecycleStage,
) {
    if state.is_completed(completion) {
        return;
    }

    steps.extend([
        LifecycleStep::Hooks(before_hook),
        LifecycleStep::Lifecycle(lifecycle_stage),
        LifecycleStep::Hooks(after_hook_stage(before_hook).expect("creation hook has after hook")),
    ]);
}

#[allow(dead_code)]
pub(crate) fn attach_lifecycle_plan() -> Vec<LifecycleStep> {
    vec![
        LifecycleStep::PortForwardingStart,
        LifecycleStep::Hooks(HookStage::BeforePostAttach),
        LifecycleStep::Lifecycle(LifecycleStage::PostAttach),
        LifecycleStep::Hooks(HookStage::AfterPostAttach),
        LifecycleStep::ShellAttach,
    ]
}

pub(crate) fn run_host_initialize_lifecycle(
    config: &ResolvedConfig,
    workspace_root: &Path,
) -> Result<()> {
    run_hook_stage_without_container(config, workspace_root, HookStage::BeforeInitialize)?;
    run_host_lifecycle_command(config, workspace_root, LifecycleStage::Initialize)?;
    run_hook_stage_without_container(config, workspace_root, HookStage::AfterInitialize)?;

    Ok(())
}

pub(crate) async fn prepare_container_lifecycle(
    context: LifecycleRunContext<'_>,
) -> Result<PreparedLifecycleRunContext<'_>> {
    let dotfile_variables = dotfile_variable_context(&context);
    let dotfiles_result = setup_dotfiles(
        context.client,
        context.container,
        context.config,
        &context.remote_user,
        &dotfile_variables,
    )
    .await;
    if dotfiles_result.is_err() {
        remove_staged_host_gitconfig(context.runtime_dir)?;
    }
    dotfiles_result?;

    let install_result = install_staged_host_gitconfig(
        context.client,
        context.container,
        context.config,
        &context.remote_user,
    )
    .await;
    let cleanup_result = remove_staged_host_gitconfig(context.runtime_dir);
    install_result?;
    cleanup_result?;

    setup_git_credentials(
        context.client,
        context.container,
        context.config,
        &context.remote_user,
    )
    .await?;
    setup_github_cli_credentials(
        context.client,
        context.container,
        context.config,
        &context.remote_user,
    )
    .await?;
    Ok(PreparedLifecycleRunContext {
        remote_process_env: resolve_exec_env(
            context.client,
            context.container,
            &context.remote_user.user,
            context.remote_user.shell.as_deref(),
            &context.config.devcontainer.remote_env,
            context.config.devcontainer.user_env_probe,
        )
        .await?,
        client: context.client,
        container: context.container,
        config: context.config,
        workspace_root: context.workspace_root,
        workspace_folder: context.workspace_folder,
        remote_user: context.remote_user,
    })
}

fn dotfile_variable_context(context: &LifecycleRunContext<'_>) -> VariableContext {
    VariableContext::new(
        context.workspace_root.to_path_buf(),
        context.workspace_basename.to_owned(),
        context.workspace_folder.to_owned(),
        container_workspace_folder_basename(context.workspace_folder, context.workspace_basename),
        context.workspace_id.to_owned(),
        current_uid(),
        current_gid(),
        context.remote_user.user.clone(),
        context.remote_user.home.clone(),
    )
}

fn container_workspace_folder_basename(workspace_folder: &str, workspace_basename: &str) -> String {
    Path::new(workspace_folder)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(workspace_basename)
        .to_owned()
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(unix)]
fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

#[cfg(not(unix))]
fn current_gid() -> u32 {
    0
}

pub(crate) async fn run_container_start_lifecycle(
    path: LifecycleRunPath,
    context: &PreparedLifecycleRunContext<'_>,
    state: &mut LifecycleState,
    mut save_state: impl FnMut(&LifecycleState) -> Result<()>,
) -> Result<()> {
    match path {
        LifecycleRunPath::New => {
            run_container_creation_stage(
                context,
                state,
                LifecycleCompletion::OnCreate,
                HookStage::BeforeOnCreate,
                LifecycleStage::OnCreate,
                &mut save_state,
            )
            .await?;
            run_container_creation_stage(
                context,
                state,
                LifecycleCompletion::UpdateContent,
                HookStage::BeforeUpdateContent,
                LifecycleStage::UpdateContent,
                &mut save_state,
            )
            .await?;
            run_container_creation_stage(
                context,
                state,
                LifecycleCompletion::PostCreate,
                HookStage::BeforePostCreate,
                LifecycleStage::PostCreate,
                &mut save_state,
            )
            .await?;
            run_container_stage(
                context,
                HookStage::BeforePostStart,
                LifecycleStage::PostStart,
            )
            .await?;
        }
        LifecycleRunPath::Started => {
            run_container_stage(
                context,
                HookStage::BeforePostStart,
                LifecycleStage::PostStart,
            )
            .await?;
        }
        LifecycleRunPath::Running => {}
    }

    Ok(())
}

async fn run_container_creation_stage(
    context: &PreparedLifecycleRunContext<'_>,
    state: &mut LifecycleState,
    completion: LifecycleCompletion,
    before_hook: HookStage,
    lifecycle_stage: LifecycleStage,
    save_state: &mut impl FnMut(&LifecycleState) -> Result<()>,
) -> Result<()> {
    if state.is_completed(completion) {
        return Ok(());
    }

    run_container_stage(context, before_hook, lifecycle_stage).await?;
    state.mark_completed(completion);
    save_state(state)?;

    Ok(())
}

pub(crate) async fn run_attach_lifecycle(context: &PreparedLifecycleRunContext<'_>) -> Result<()> {
    run_container_stage(
        context,
        HookStage::BeforePostAttach,
        LifecycleStage::PostAttach,
    )
    .await?;

    Ok(())
}

async fn run_container_stage(
    context: &PreparedLifecycleRunContext<'_>,
    before_hook: HookStage,
    lifecycle_stage: LifecycleStage,
) -> Result<()> {
    run_hook_stage(context, before_hook).await?;
    run_lifecycle_stage(context, lifecycle_stage).await?;
    run_hook_stage(context, after_hook_stage(before_hook)?).await?;

    Ok(())
}

async fn run_lifecycle_stage(
    context: &PreparedLifecycleRunContext<'_>,
    stage: LifecycleStage,
) -> Result<()> {
    let Some(lifecycle) = &context.config.devcontainer.lifecycle else {
        return Ok(());
    };
    if lifecycle.commands(stage).is_empty() {
        return Ok(());
    }

    for command in lifecycle.commands(stage) {
        run_container_lifecycle_command(context, stage, command).await?;
    }

    Ok(())
}

fn run_host_lifecycle_command(
    config: &ResolvedConfig,
    workspace_root: &Path,
    stage: LifecycleStage,
) -> Result<()> {
    let Some(lifecycle) = &config.devcontainer.lifecycle else {
        return Ok(());
    };
    if lifecycle.commands(stage).is_empty() {
        return Ok(());
    }

    for command in lifecycle.commands(stage) {
        run_host_lifecycle_command_value(workspace_root, stage, command)?;
    }

    Ok(())
}

fn run_hook_stage_without_container(
    config: &ResolvedConfig,
    workspace_root: &Path,
    stage: HookStage,
) -> Result<()> {
    for hook in hooks_for_stage(config, stage) {
        let location = hook.location.unwrap_or_else(|| stage.default_location());
        if location != HookLocation::Host {
            bail!(
                "Hook {} must run on host before container creation",
                stage.property_name()
            );
        }
        run_host_hook(workspace_root, stage, hook)?;
    }

    Ok(())
}

async fn run_hook_stage(context: &PreparedLifecycleRunContext<'_>, stage: HookStage) -> Result<()> {
    for hook in hooks_for_stage(context.config, stage) {
        match hook.location.unwrap_or_else(|| stage.default_location()) {
            HookLocation::Host => run_host_hook(context.workspace_root, stage, hook)?,
            HookLocation::Container => run_container_hook(context, stage, hook).await?,
        }
    }

    Ok(())
}

fn hooks_for_stage(config: &ResolvedConfig, stage: HookStage) -> &[ResolvedHook] {
    match stage {
        HookStage::BeforeInitialize => &config.hooks.before_initialize,
        HookStage::AfterInitialize => &config.hooks.after_initialize,
        HookStage::BeforeOnCreate => &config.hooks.before_on_create,
        HookStage::AfterOnCreate => &config.hooks.after_on_create,
        HookStage::BeforeUpdateContent => &config.hooks.before_update_content,
        HookStage::AfterUpdateContent => &config.hooks.after_update_content,
        HookStage::BeforePostCreate => &config.hooks.before_post_create,
        HookStage::AfterPostCreate => &config.hooks.after_post_create,
        HookStage::BeforePostStart => &config.hooks.before_post_start,
        HookStage::AfterPostStart => &config.hooks.after_post_start,
        HookStage::BeforePostAttach => &config.hooks.before_post_attach,
        HookStage::AfterPostAttach => &config.hooks.after_post_attach,
    }
}

fn run_host_lifecycle_command_value(
    workspace_root: &Path,
    stage: LifecycleStage,
    command: &LifecycleCommand,
) -> Result<()> {
    match command {
        LifecycleCommand::Shell(_) | LifecycleCommand::Args(_) => run_host_process(
            stage.property_name(),
            &lifecycle_command_argv(command),
            workspace_root,
        ),
        LifecycleCommand::Parallel(commands) => run_host_parallel(workspace_root, stage, commands),
    }
}

async fn run_container_lifecycle_command(
    context: &PreparedLifecycleRunContext<'_>,
    stage: LifecycleStage,
    command: &LifecycleCommand,
) -> Result<()> {
    match command {
        LifecycleCommand::Shell(_) | LifecycleCommand::Args(_) => {
            let argv = lifecycle_command_argv(command);
            run_container_process(context, stage.property_name(), argv, None).await
        }
        LifecycleCommand::Parallel(commands) => {
            let futures = commands.iter().map(|(name, command)| async move {
                let argv = lifecycle_command_argv(command);
                let stage_name = format!("{}.{name}", stage.property_name());
                run_container_process(context, &stage_name, argv, None).await
            });
            let results = join_all(futures).await;
            let mut first_error = None;
            for result in results {
                if let Err(error) = result
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
            Ok(())
        }
    }
}

fn run_host_hook(workspace_root: &Path, stage: HookStage, hook: &ResolvedHook) -> Result<()> {
    if hook.user.is_some() {
        bail!(
            "Host hook {} must not specify a container user",
            stage.property_name()
        );
    }

    let argv = hook_command_argv(&hook.command, hook.shell);
    let workdir = host_hook_workdir(workspace_root, hook);
    run_host_process(stage.property_name(), &argv, &workdir)
}

async fn run_container_hook(
    context: &PreparedLifecycleRunContext<'_>,
    stage: HookStage,
    hook: &ResolvedHook,
) -> Result<()> {
    let argv = hook_command_argv(&hook.command, hook.shell);
    let user = hook_user(&context.remote_user, hook);
    let workdir = hook
        .workdir
        .clone()
        .unwrap_or_else(|| context.workspace_folder.to_owned());

    run_container_process(context, stage.property_name(), argv, Some((user, workdir))).await
}

fn run_host_parallel(
    workspace_root: &Path,
    stage: LifecycleStage,
    commands: &BTreeMap<String, LifecycleCommand>,
) -> Result<()> {
    let handles = commands
        .iter()
        .map(|(name, command)| {
            let stage_name = format!("{}.{}", stage.property_name(), name);
            let argv = lifecycle_command_argv(command);
            let workdir = workspace_root.to_path_buf();
            std::thread::spawn(move || run_host_process(&stage_name, &argv, &workdir))
        })
        .collect::<Vec<_>>();

    let mut first_error = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Err(_) => {
                if first_error.is_none() {
                    first_error = Some(anyhow!(
                        "Lifecycle stage {} failed because a host command thread panicked",
                        stage.property_name()
                    ));
                }
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn run_container_process(
    context: &PreparedLifecycleRunContext<'_>,
    stage_name: &str,
    command: Vec<String>,
    hook_context: Option<(String, String)>,
) -> Result<()> {
    let (user, working_dir) = hook_context.unwrap_or_else(|| {
        (
            context.remote_user.user.clone(),
            context.workspace_folder.to_owned(),
        )
    });
    let output = exec_capture_output(
        context.client,
        context.container,
        &ExecCommandSpec {
            command: command.clone(),
            user: Some(user.clone()),
            working_dir: Some(working_dir),
            env: lifecycle_process_env(context, &user),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to run lifecycle stage {stage_name}"))?;

    ensure_lifecycle_success(stage_name, &command, output)
}

fn lifecycle_process_env(
    context: &PreparedLifecycleRunContext<'_>,
    user: &str,
) -> BTreeMap<String, String> {
    if same_container_user(user, &context.remote_user.user) {
        return context.remote_process_env.clone();
    }

    context.config.devcontainer.remote_env.clone()
}

fn same_container_user(left: &str, right: &str) -> bool {
    let left = docker_user_lookup_key(left);
    let right = docker_user_lookup_key(right);

    left == right || (is_root_user(left) && is_root_user(right))
}

fn docker_user_lookup_key(user: &str) -> &str {
    user.split_once(':')
        .map(|(name, _)| name)
        .unwrap_or(user)
        .trim()
}

fn is_root_user(user: &str) -> bool {
    matches!(user, "root" | "0")
}

fn run_host_process(stage_name: &str, argv: &[String], workdir: &Path) -> Result<()> {
    let (program, args) = argv
        .split_first()
        .with_context(|| format!("Lifecycle stage {stage_name} command must not be empty"))?;
    let output = HostCommand::new(program)
        .args(args)
        .current_dir(workdir)
        .output()
        .with_context(|| {
            format!(
                "Failed to run lifecycle stage {stage_name} in directory: {}",
                workdir.display()
            )
        })?;
    let exit_code = output.status.code().map(i64::from).unwrap_or(-1);

    ensure_lifecycle_success(
        stage_name,
        argv,
        ExecOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code,
        },
    )
}

pub(crate) fn lifecycle_command_argv(command: &LifecycleCommand) -> Vec<String> {
    match command {
        LifecycleCommand::Shell(command) => shell_argv(command),
        LifecycleCommand::Args(args) => args.clone(),
        LifecycleCommand::Parallel(_) => Vec::new(),
    }
}

pub(crate) fn hook_command_argv(command: &Command, shell: bool) -> Vec<String> {
    match (command, shell) {
        (Command::Shell(command), true) => shell_argv(command),
        (Command::Shell(command), false) => vec![command.clone()],
        (Command::Args(args), false) => args.clone(),
        (Command::Args(args), true) => shell_argv(&args.join(" ")),
    }
}

fn shell_argv(command: &str) -> Vec<String> {
    vec!["/bin/sh".to_owned(), "-lc".to_owned(), command.to_owned()]
}

fn host_hook_workdir(workspace_root: &Path, hook: &ResolvedHook) -> PathBuf {
    let Some(workdir) = &hook.workdir else {
        return workspace_root.to_path_buf();
    };
    let path = PathBuf::from(workdir);
    if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    }
}

fn hook_user(remote_user: &ResolvedRemoteUser, hook: &ResolvedHook) -> String {
    match hook.user.as_deref() {
        None | Some("remote") => remote_user.user.clone(),
        Some("root") => "root".to_owned(),
        Some(user) => user.to_owned(),
    }
}

fn after_hook_stage(before_hook: HookStage) -> Result<HookStage> {
    match before_hook {
        HookStage::BeforeOnCreate => Ok(HookStage::AfterOnCreate),
        HookStage::BeforeUpdateContent => Ok(HookStage::AfterUpdateContent),
        HookStage::BeforePostCreate => Ok(HookStage::AfterPostCreate),
        HookStage::BeforePostStart => Ok(HookStage::AfterPostStart),
        HookStage::BeforePostAttach => Ok(HookStage::AfterPostAttach),
        HookStage::BeforeInitialize | HookStage::AfterInitialize => {
            bail!("Hook stage does not have a container after hook")
        }
        HookStage::AfterOnCreate
        | HookStage::AfterUpdateContent
        | HookStage::AfterPostCreate
        | HookStage::AfterPostStart
        | HookStage::AfterPostAttach => bail!("Hook stage is already an after hook"),
    }
}

fn ensure_lifecycle_success(
    stage_name: &str,
    command: &[String],
    output: ExecOutput,
) -> Result<()> {
    if output.exit_code == 0 {
        return Ok(());
    }

    bail!(
        "Lifecycle stage {stage_name} failed: command `{}` exited with exit code {}. stdout tail: `{}` stderr tail: `{}`",
        command_display(command),
        output.exit_code,
        output_tail(&output.stdout),
        output_tail(&output.stderr),
    );
}

fn command_display(command: &[String]) -> String {
    command.join(" ")
}

fn output_tail(output: &[u8]) -> String {
    const MAX_TAIL_BYTES: usize = 4096;

    let start = output.len().saturating_sub(MAX_TAIL_BYTES);
    String::from_utf8_lossy(&output[start..]).trim().to_owned()
}

impl TryFrom<LifecycleProperty> for LifecycleStage {
    type Error = anyhow::Error;

    fn try_from(value: LifecycleProperty) -> Result<Self> {
        match value {
            LifecycleProperty::InitializeCommand => Ok(Self::Initialize),
            LifecycleProperty::OnCreateCommand => Ok(Self::OnCreate),
            LifecycleProperty::UpdateContentCommand => Ok(Self::UpdateContent),
            LifecycleProperty::PostCreateCommand => Ok(Self::PostCreate),
            LifecycleProperty::PostStartCommand => Ok(Self::PostStart),
            LifecycleProperty::PostAttachCommand => Ok(Self::PostAttach),
            LifecycleProperty::WaitFor => Err(anyhow!("waitFor is not a lifecycle command")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum LifecycleExecutionLocation {
    Host,
    Container,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum WaitFor {
    Initialize,
    OnCreate,
    #[default]
    UpdateContent,
    PostCreate,
    PostStart,
}

pub(crate) fn parse_lifecycle_command(
    stage: LifecycleStage,
    value: &Value,
) -> Result<LifecycleCommand> {
    match value {
        Value::String(command) => Ok(LifecycleCommand::Shell(command.clone())),
        Value::Array(values) => parse_args(stage, values),
        Value::Object(entries) => parse_parallel(stage, entries),
        _ => Err(anyhow!(
            "{} must be a string, string array, or object command",
            stage.property_name()
        )),
    }
}

#[allow(dead_code)]
pub(crate) fn parse_lifecycle_definition(
    values: &BTreeMap<LifecycleProperty, Value>,
) -> Result<LifecycleDefinition> {
    Ok(match parse_lifecycle_layer_definition(values)? {
        Some(layer) => layer.into_resolved(),
        None => LifecycleDefinition {
            commands: BTreeMap::new(),
            wait_for: WaitFor::default(),
        },
    })
}

pub(crate) fn parse_lifecycle_layer_definition(
    values: &BTreeMap<LifecycleProperty, Value>,
) -> Result<Option<LayerLifecycleDefinition>> {
    let mut commands = BTreeMap::new();

    for (property, value) in values {
        if *property == LifecycleProperty::WaitFor {
            continue;
        }

        let stage = LifecycleStage::try_from(*property)?;
        commands.insert(stage, vec![parse_lifecycle_command(stage, value)?]);
    }

    let wait_for = values
        .get(&LifecycleProperty::WaitFor)
        .map(|value| parse_wait_for(Some(value)))
        .transpose()?;

    if commands.is_empty() && wait_for.is_none() {
        return Ok(None);
    }

    Ok(Some(LayerLifecycleDefinition { commands, wait_for }))
}

pub(crate) fn parse_wait_for(value: Option<&Value>) -> Result<WaitFor> {
    match value {
        None => Ok(WaitFor::default()),
        Some(Value::String(stage)) => match stage.as_str() {
            "initializeCommand" => Ok(WaitFor::Initialize),
            "onCreateCommand" => Ok(WaitFor::OnCreate),
            "updateContentCommand" => Ok(WaitFor::UpdateContent),
            "postCreateCommand" => Ok(WaitFor::PostCreate),
            "postStartCommand" => Ok(WaitFor::PostStart),
            _ => Err(anyhow!("Unsupported waitFor lifecycle stage: {stage}")),
        },
        Some(_) => Err(anyhow!("waitFor must be a lifecycle stage string")),
    }
}

fn parse_args(stage: LifecycleStage, values: &[Value]) -> Result<LifecycleCommand> {
    if values.is_empty() {
        return Err(anyhow!(
            "{} command array must not be empty",
            stage.property_name()
        ));
    }

    let args = values
        .iter()
        .map(|value| match value {
            Value::String(arg) => Ok(arg.clone()),
            _ => Err(anyhow!(
                "{} command array entries must be strings",
                stage.property_name()
            )),
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(LifecycleCommand::Args(args))
}

fn parse_parallel(
    stage: LifecycleStage,
    entries: &serde_json::Map<String, Value>,
) -> Result<LifecycleCommand> {
    if entries.is_empty() {
        return Err(anyhow!(
            "{} command object must not be empty",
            stage.property_name()
        ));
    }

    let mut commands = BTreeMap::new();
    for (name, value) in entries {
        let command = match value {
            Value::String(_) | Value::Array(_) => parse_lifecycle_command(stage, value)?,
            _ => {
                return Err(anyhow!(
                    "{} parallel command entry {name} must be a string or string array",
                    stage.property_name()
                ));
            }
        };
        commands.insert(name.clone(), command);
    }

    Ok(LifecycleCommand::Parallel(commands))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::devcontainer::metadata::LifecycleProperty;

    use super::*;

    #[test]
    fn parses_string_command_as_shell_command() {
        let command =
            parse_lifecycle_command(LifecycleStage::PostCreate, &json!("npm install")).unwrap();

        assert_eq!(command, LifecycleCommand::Shell("npm install".to_owned()));
    }

    #[test]
    fn parses_array_command_as_exec_args() {
        let command = parse_lifecycle_command(
            LifecycleStage::PostStart,
            &json!(["bash", "-lc", "echo ready"]),
        )
        .unwrap();

        assert_eq!(
            command,
            LifecycleCommand::Args(vec![
                "bash".to_owned(),
                "-lc".to_owned(),
                "echo ready".to_owned()
            ])
        );
    }

    #[test]
    fn rejects_empty_array_command() {
        let error = parse_lifecycle_command(LifecycleStage::OnCreate, &json!([])).unwrap_err();

        assert!(error.to_string().contains("onCreateCommand"));
        assert!(error.to_string().contains("must not be empty"));
    }

    #[test]
    fn rejects_non_string_array_entries() {
        let error =
            parse_lifecycle_command(LifecycleStage::PostCreate, &json!(["echo", 1])).unwrap_err();

        assert!(error.to_string().contains("postCreateCommand"));
        assert!(error.to_string().contains("entries must be strings"));
    }

    #[test]
    fn parses_object_command_as_parallel_entries() {
        let command = parse_lifecycle_command(
            LifecycleStage::UpdateContent,
            &json!({
                "frontend": "npm install",
                "backend": ["cargo", "fetch"]
            }),
        )
        .unwrap();

        assert_eq!(
            command,
            LifecycleCommand::Parallel(
                [
                    (
                        "backend".to_owned(),
                        LifecycleCommand::Args(vec!["cargo".to_owned(), "fetch".to_owned()])
                    ),
                    (
                        "frontend".to_owned(),
                        LifecycleCommand::Shell("npm install".to_owned())
                    )
                ]
                .into()
            )
        );
    }

    #[test]
    fn rejects_empty_object_command() {
        let error = parse_lifecycle_command(LifecycleStage::PostStart, &json!({})).unwrap_err();

        assert!(error.to_string().contains("postStartCommand"));
        assert!(error.to_string().contains("object must not be empty"));
    }

    #[test]
    fn rejects_invalid_parallel_entry_type() {
        let error = parse_lifecycle_command(
            LifecycleStage::UpdateContent,
            &json!({"api": {"nested": true}}),
        )
        .unwrap_err();

        assert!(error.to_string().contains("updateContentCommand"));
        assert!(error.to_string().contains("parallel command entry api"));
    }

    #[test]
    fn classifies_initialize_as_host_and_other_stages_as_container() {
        assert_eq!(
            LifecycleStage::Initialize.execution_location(),
            LifecycleExecutionLocation::Host
        );

        for stage in [
            LifecycleStage::OnCreate,
            LifecycleStage::UpdateContent,
            LifecycleStage::PostCreate,
            LifecycleStage::PostStart,
            LifecycleStage::PostAttach,
        ] {
            assert_eq!(
                stage.execution_location(),
                LifecycleExecutionLocation::Container
            );
        }
    }

    #[test]
    fn parses_wait_for_with_update_content_default() {
        assert_eq!(WaitFor::default(), WaitFor::UpdateContent);
        assert_eq!(parse_wait_for(None).unwrap(), WaitFor::UpdateContent);
        assert_eq!(
            parse_wait_for(Some(&json!("postCreateCommand"))).unwrap(),
            WaitFor::PostCreate
        );
    }

    #[test]
    fn rejects_unknown_wait_for_stage() {
        let error = parse_wait_for(Some(&json!("postAttachCommand"))).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Unsupported waitFor lifecycle stage")
        );
    }

    #[test]
    fn rejects_non_string_wait_for() {
        let error = parse_wait_for(Some(&json!(["postCreateCommand"]))).unwrap_err();

        assert!(error.to_string().contains("waitFor must be"));
    }

    #[test]
    fn parses_lifecycle_definition_from_metadata_values() {
        let lifecycle = parse_lifecycle_definition(&BTreeMap::from([
            (
                LifecycleProperty::InitializeCommand,
                json!("scripts/init.sh"),
            ),
            (
                LifecycleProperty::PostStartCommand,
                json!(["bash", "-lc", "echo ready"]),
            ),
            (LifecycleProperty::WaitFor, json!("postStartCommand")),
        ]))
        .unwrap();

        assert_eq!(lifecycle.wait_for(), WaitFor::PostStart);
        assert_eq!(
            lifecycle.command(LifecycleStage::Initialize),
            Some(&LifecycleCommand::Shell("scripts/init.sh".to_owned()))
        );
        assert_eq!(
            lifecycle.command(LifecycleStage::PostStart),
            Some(&LifecycleCommand::Args(vec![
                "bash".to_owned(),
                "-lc".to_owned(),
                "echo ready".to_owned()
            ]))
        );
    }

    #[test]
    fn lifecycle_merge_collects_same_stage_commands_in_order() {
        let mut lifecycle = parse_lifecycle_definition(&BTreeMap::from([(
            LifecycleProperty::PostStartCommand,
            json!("feature-one"),
        )]))
        .unwrap();
        let second = parse_lifecycle_layer_definition(&BTreeMap::from([(
            LifecycleProperty::PostStartCommand,
            json!("feature-two"),
        )]))
        .unwrap()
        .unwrap();
        let user = parse_lifecycle_layer_definition(&BTreeMap::from([(
            LifecycleProperty::PostStartCommand,
            json!("user-command"),
        )]))
        .unwrap()
        .unwrap();

        lifecycle.merge_layer(second);
        lifecycle.merge_layer(user);

        assert_eq!(
            lifecycle.commands(LifecycleStage::PostStart),
            &[
                LifecycleCommand::Shell("feature-one".to_owned()),
                LifecycleCommand::Shell("feature-two".to_owned()),
                LifecycleCommand::Shell("user-command".to_owned()),
            ]
        );
    }

    #[test]
    fn lifecycle_plan_for_new_container_matches_documented_order() {
        assert_eq!(
            lifecycle_plan(LifecycleRunPath::New),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ImagePreparation,
                LifecycleStep::ContainerCreate,
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforeOnCreate),
                LifecycleStep::Lifecycle(LifecycleStage::OnCreate),
                LifecycleStep::Hooks(HookStage::AfterOnCreate),
                LifecycleStep::Hooks(HookStage::BeforeUpdateContent),
                LifecycleStep::Lifecycle(LifecycleStage::UpdateContent),
                LifecycleStep::Hooks(HookStage::AfterUpdateContent),
                LifecycleStep::Hooks(HookStage::BeforePostCreate),
                LifecycleStep::Lifecycle(LifecycleStage::PostCreate),
                LifecycleStep::Hooks(HookStage::AfterPostCreate),
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
                LifecycleStep::PortForwardingStart,
                LifecycleStep::Hooks(HookStage::BeforePostAttach),
                LifecycleStep::Lifecycle(LifecycleStage::PostAttach),
                LifecycleStep::Hooks(HookStage::AfterPostAttach),
                LifecycleStep::ShellAttach,
            ]
        );
    }

    #[test]
    fn lifecycle_plan_for_existing_paths_matches_documented_order() {
        assert_eq!(
            lifecycle_plan(LifecycleRunPath::Started),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
                LifecycleStep::PortForwardingStart,
                LifecycleStep::Hooks(HookStage::BeforePostAttach),
                LifecycleStep::Lifecycle(LifecycleStage::PostAttach),
                LifecycleStep::Hooks(HookStage::AfterPostAttach),
                LifecycleStep::ShellAttach,
            ]
        );
        assert_eq!(
            lifecycle_plan(LifecycleRunPath::Running),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::PortForwardingStart,
                LifecycleStep::Hooks(HookStage::BeforePostAttach),
                LifecycleStep::Lifecycle(LifecycleStage::PostAttach),
                LifecycleStep::Hooks(HookStage::AfterPostAttach),
                LifecycleStep::ShellAttach,
            ]
        );
    }

    #[test]
    fn lifecycle_start_phase_plans_exclude_attach_steps() {
        assert_eq!(
            container_start_lifecycle_plan(LifecycleRunPath::New),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ImagePreparation,
                LifecycleStep::ContainerCreate,
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforeOnCreate),
                LifecycleStep::Lifecycle(LifecycleStage::OnCreate),
                LifecycleStep::Hooks(HookStage::AfterOnCreate),
                LifecycleStep::Hooks(HookStage::BeforeUpdateContent),
                LifecycleStep::Lifecycle(LifecycleStage::UpdateContent),
                LifecycleStep::Hooks(HookStage::AfterUpdateContent),
                LifecycleStep::Hooks(HookStage::BeforePostCreate),
                LifecycleStep::Lifecycle(LifecycleStage::PostCreate),
                LifecycleStep::Hooks(HookStage::AfterPostCreate),
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
            ]
        );
        assert_eq!(
            container_start_lifecycle_plan(LifecycleRunPath::Started),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
            ]
        );
        assert_eq!(
            container_start_lifecycle_plan(LifecycleRunPath::Running),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
            ]
        );
    }

    #[test]
    fn lifecycle_start_phase_plan_skips_completed_creation_stages_from_state() {
        assert_eq!(
            container_start_lifecycle_plan_with_state(
                LifecycleRunPath::New,
                &crate::state::LifecycleState {
                    on_create_completed: true,
                    update_content_completed: false,
                    post_create_completed: true,
                },
            ),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ImagePreparation,
                LifecycleStep::ContainerCreate,
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforeUpdateContent),
                LifecycleStep::Lifecycle(LifecycleStage::UpdateContent),
                LifecycleStep::Hooks(HookStage::AfterUpdateContent),
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
            ]
        );
    }

    #[test]
    fn lifecycle_attach_phase_plan_contains_forwarding_post_attach_and_shell_boundary() {
        assert_eq!(
            attach_lifecycle_plan(),
            vec![
                LifecycleStep::PortForwardingStart,
                LifecycleStep::Hooks(HookStage::BeforePostAttach),
                LifecycleStep::Lifecycle(LifecycleStage::PostAttach),
                LifecycleStep::Hooks(HookStage::AfterPostAttach),
                LifecycleStep::ShellAttach,
            ]
        );
    }

    #[test]
    fn lifecycle_commands_map_to_process_argv() {
        assert_eq!(
            lifecycle_command_argv(&LifecycleCommand::Shell("echo ready".to_owned())),
            vec!["/bin/sh", "-lc", "echo ready"]
        );
        assert_eq!(
            lifecycle_command_argv(&LifecycleCommand::Args(vec![
                "bash".to_owned(),
                "-lc".to_owned(),
                "echo ready".to_owned()
            ])),
            vec!["bash", "-lc", "echo ready"]
        );
    }

    #[test]
    fn host_parallel_lifecycle_waits_for_all_siblings_after_failure() {
        let workspace = tempfile::Builder::new()
            .prefix("decune-host-parallel-lifecycle-")
            .tempdir()
            .unwrap();
        let marker = workspace.path().join("slow-finished");
        let command = LifecycleCommand::Parallel(BTreeMap::from([
            (
                "a_fail".to_owned(),
                LifecycleCommand::Shell("exit 7".to_owned()),
            ),
            (
                "z_slow".to_owned(),
                LifecycleCommand::Shell("sleep 1; printf done > slow-finished".to_owned()),
            ),
        ]));

        let error = run_host_lifecycle_command_value(
            workspace.path(),
            LifecycleStage::Initialize,
            &command,
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("Lifecycle stage initializeCommand.a_fail failed"));
        assert!(marker.exists());
    }

    #[test]
    fn host_lifecycle_failure_reports_command_exit_code_and_output_tails() {
        let workspace = tempfile::Builder::new()
            .prefix("decune-host-lifecycle-failure-")
            .tempdir()
            .unwrap();
        let command = LifecycleCommand::Shell(
            "printf stdout-sentinel; printf stderr-sentinel >&2; exit 7".to_owned(),
        );

        let error = run_host_lifecycle_command_value(
            workspace.path(),
            LifecycleStage::Initialize,
            &command,
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("Lifecycle stage initializeCommand failed"));
        assert!(message.contains(
            "command `/bin/sh -lc printf stdout-sentinel; printf stderr-sentinel >&2; exit 7`"
        ));
        assert!(message.contains("exit code 7"));
        assert!(message.contains("stdout tail: `stdout-sentinel`"));
        assert!(message.contains("stderr tail: `stderr-sentinel`"));
    }

    #[test]
    fn hook_commands_respect_shell_flag() {
        assert_eq!(
            hook_command_argv(&Command::Shell("scripts/setup.sh".to_owned()), true),
            vec!["/bin/sh", "-lc", "scripts/setup.sh"]
        );
        assert_eq!(
            hook_command_argv(
                &Command::Args(vec!["bash".to_owned(), "scripts/setup.sh".to_owned()]),
                false,
            ),
            vec!["bash", "scripts/setup.sh"]
        );
        assert_eq!(
            hook_command_argv(
                &Command::Args(vec!["bash".to_owned(), "scripts/setup.sh".to_owned()]),
                true,
            ),
            vec!["/bin/sh", "-lc", "bash scripts/setup.sh"]
        );
    }

    #[test]
    fn host_hook_workdir_defaults_to_workspace_and_resolves_relative_paths() {
        let workspace_root = Path::new("/workspace/project");
        let default_hook = ResolvedHook {
            command: Command::Shell("true".to_owned()),
            location: None,
            user: None,
            shell: true,
            workdir: None,
        };
        let relative_hook = ResolvedHook {
            workdir: Some("scripts".to_owned()),
            ..default_hook.clone()
        };
        let absolute_hook = ResolvedHook {
            workdir: Some("/tmp".to_owned()),
            ..default_hook.clone()
        };

        assert_eq!(
            host_hook_workdir(workspace_root, &default_hook),
            workspace_root
        );
        assert_eq!(
            host_hook_workdir(workspace_root, &relative_hook),
            PathBuf::from("/workspace/project/scripts")
        );
        assert_eq!(
            host_hook_workdir(workspace_root, &absolute_hook),
            PathBuf::from("/tmp")
        );
    }

    #[test]
    fn same_container_user_matches_group_suffix_and_root_alias() {
        assert!(same_container_user("vscode", "vscode:shared"));
        assert!(same_container_user("root", "0"));
        assert!(!same_container_user("root", "vscode"));
    }
}
