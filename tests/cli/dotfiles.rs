use crate::harness::*;

#[test]
fn up_detach_sets_up_dotfile_symlink() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/gitconfig", "[user]\nname = decune\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "postStartCommand": "test -L /root/.gitconfig && test \"$(readlink /root/.gitconfig)\" = \"/opt/decune/dotfiles/.gitconfig\" && grep -q decune /root/.gitconfig && if sh -c 'printf x >> /root/.gitconfig' 2>/tmp/decune-dotfile-write-error; then exit 19; fi"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "dotfiles/gitconfig"
target = ".gitconfig"
read_only = true
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let expected_source = workspace_root
        .join("dotfiles/gitconfig")
        .canonicalize()
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root).unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let mount = mounts
                .iter()
                .find(|mount| mount.target.as_deref() == Some("/opt/decune/dotfiles/.gitconfig"))
                .expect("expected dotfile mount");

            assert_eq!(mount.source.as_deref(), expected_source.to_str());
            assert_eq!(mount.read_only, Some(true));
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(unix)]
#[test]
fn up_detach_resolves_dotfile_symlink_source() {
    use std::os::unix::fs as unix_fs;

    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("actual-nvim").unwrap();
    workspace
        .write_file("actual-nvim/init.lua", "return {}\n")
        .unwrap();
    unix_fs::symlink(
        workspace.path().join("actual-nvim"),
        workspace.path().join("linked-nvim"),
    )
    .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "postStartCommand": "test -L /root/.config/nvim && test \"$(readlink /root/.config/nvim)\" = \"/opt/decune/dotfiles/.config/nvim\" && test -f /root/.config/nvim/init.lua"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "linked-nvim"
target = ".config/nvim"
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let expected_source = workspace_root.join("actual-nvim").canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root).unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let mount = mounts
                .iter()
                .find(|mount| mount.target.as_deref() == Some("/opt/decune/dotfiles/.config/nvim"))
                .expect("expected dotfile mount");

            assert_eq!(mount.source.as_deref(), expected_source.to_str());
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(unix)]
#[test]
fn up_detach_mounts_directory_symlink_entries_as_real_files() {
    use std::os::unix::fs as unix_fs;

    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles-real").unwrap();
    workspace.create_dir("lazygit-source").unwrap();
    workspace
        .write_file(
            "dotfiles-real/config.yml",
            "gui:\n  nerdFontsVersion: '3'\n",
        )
        .unwrap();
    workspace
        .write_file("dotfiles-real/extra.yml", "not mounted\n")
        .unwrap();
    unix_fs::symlink(
        workspace.path().join("dotfiles-real/config.yml"),
        workspace.path().join("lazygit-source/config.yml"),
    )
    .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "postStartCommand": "test -L /root/.config/lazygit && test \"$(readlink /root/.config/lazygit)\" = \"/opt/decune/dotfiles/.config/lazygit\" && test -L /root/.config/lazygit/config.yml && readlink /root/.config/lazygit/config.yml | grep -Eq '^/opt/decune/dotfile-backings/[0-9]+/config.yml$' && grep -q nerdFontsVersion /root/.config/lazygit/config.yml && test ! -e /root/.config/lazygit/extra.yml && if sh -c 'printf x >> /root/.config/lazygit/config.yml' 2>/tmp/decune-dotfile-write-error; then exit 19; fi"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[credentials.github]
enabled = false

[[dotfiles]]
source = "lazygit-source"
target = ".config/lazygit"
read_only = true
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let state_root = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })
        .expect("HOME should be set for decune state path");
    let expected_skeleton = state_root
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("dotfile-mount-skeleton")
        .join(".config")
        .join("lazygit");
    let expected_config = workspace_root.join("dotfiles-real").canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root).unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let root_mount = mounts
                .iter()
                .find(|mount| {
                    mount.target.as_deref() == Some("/opt/decune/dotfiles/.config/lazygit")
                })
                .expect("expected dotfile skeleton mount");
            let backing_mount = mounts
                .iter()
                .find(|mount| {
                    mount
                        .target
                        .as_deref()
                        .is_some_and(|target| target.starts_with("/opt/decune/dotfile-backings/"))
                })
                .expect("expected dotfile backing mount");

            assert_eq!(root_mount.source.as_deref(), expected_skeleton.to_str());
            assert_eq!(root_mount.read_only, Some(true));
            assert_eq!(backing_mount.source.as_deref(), expected_config.to_str());
            assert_eq!(backing_mount.read_only, Some(true));
            assert!(!mounts.iter().any(|mount| {
                mount.target.as_deref() == Some("/opt/decune/dotfiles/.config/lazygit/config.yml")
            }));
            assert_eq!(
                std::fs::read_link(expected_skeleton.join("config.yml")).unwrap(),
                PathBuf::from(format!(
                    "{}/config.yml",
                    backing_mount.target.as_deref().unwrap()
                ))
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(unix)]
#[test]
fn up_detach_reuses_running_container_without_refreshing_dotfile_skeleton_mounts() {
    use std::os::unix::fs as unix_fs;

    let workspace = support::TempWorkspace::new().unwrap();
    let state_home = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles-real").unwrap();
    workspace.create_dir("lazygit-source").unwrap();
    workspace
        .write_file(
            "dotfiles-real/config.yml",
            "gui:\n  nerdFontsVersion: '3'\n",
        )
        .unwrap();
    workspace
        .write_file("dotfiles-real/extra.yml", "not mounted\n")
        .unwrap();
    unix_fs::symlink(
        workspace.path().join("dotfiles-real/config.yml"),
        workspace.path().join("lazygit-source/config.yml"),
    )
    .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[credentials.github]
enabled = false

[[dotfiles]]
source = "lazygit-source"
target = ".config/lazygit"
read_only = true
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let state_home_value = state_home.path().to_string_lossy().into_owned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home_value)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home_value)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"));

        let output = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/root/.config/lazygit/config.yml"],
                )
            })
            .unwrap();
        assert!(output.contains("nerdFontsVersion"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(unix)]
#[test]
fn running_container_sees_regular_file_dotfile_replaced_by_host_rename() {
    use std::os::unix::fs as unix_fs;

    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles-source").unwrap();
    workspace.create_dir("external").unwrap();
    workspace
        .write_file("dotfiles-source/config.yml", "before\n")
        .unwrap();
    workspace.write_file("external/marker", "marker\n").unwrap();
    unix_fs::symlink(
        workspace.path().join("external/marker"),
        workspace.path().join("dotfiles-source/marker"),
    )
    .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[credentials.github]
enabled = false

[[dotfiles]]
source = "dotfiles-source"
target = ".config/app"
read_only = true
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let config_path = workspace_root.join("dotfiles-source/config.yml");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        let before = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/root/.config/app/config.yml"],
                )
            })
            .unwrap();
        assert_eq!(before, "before\n");

        let replacement = config_path.with_extension("tmp");
        std::fs::write(&replacement, "after\n").unwrap();
        std::fs::rename(&replacement, &config_path).unwrap();

        let after = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/root/.config/app/config.yml"],
                )
            })
            .unwrap();
        assert_eq!(after, "after\n");
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(unix)]
#[test]
fn running_container_sees_symlink_target_file_replaced_by_host_rename() {
    use std::os::unix::fs as unix_fs;

    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles-real").unwrap();
    workspace.create_dir("dotfiles-source").unwrap();
    workspace
        .write_file("dotfiles-real/config.yml", "before\n")
        .unwrap();
    workspace
        .write_file("dotfiles-real/extra.yml", "not mounted\n")
        .unwrap();
    unix_fs::symlink(
        workspace.path().join("dotfiles-real/config.yml"),
        workspace.path().join("dotfiles-source/config.yml"),
    )
    .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[credentials.github]
enabled = false

[[dotfiles]]
source = "dotfiles-source"
target = ".config/app"
read_only = true
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let config_path = workspace_root.join("dotfiles-real/config.yml");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        let before = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/root/.config/app/config.yml"],
                )
            })
            .unwrap();
        assert_eq!(before, "before\n");

        let replacement = config_path.with_extension("tmp");
        std::fs::write(&replacement, "after\n").unwrap();
        std::fs::rename(&replacement, &config_path).unwrap();

        let after = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/root/.config/app/config.yml"],
                )
            })
            .unwrap();
        assert_eq!(after, "after\n");
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(unix)]
#[test]
fn up_detach_writes_directory_symlink_mounts_back_to_real_file_when_not_read_only() {
    use std::os::unix::fs as unix_fs;

    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles-real").unwrap();
    workspace.create_dir("lazygit-source").unwrap();
    workspace
        .write_file("dotfiles-real/config.yml", "before\n")
        .unwrap();
    workspace
        .write_file("dotfiles-real/extra.yml", "not mounted\n")
        .unwrap();
    unix_fs::symlink(
        workspace.path().join("dotfiles-real/config.yml"),
        workspace.path().join("lazygit-source/config.yml"),
    )
    .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "postStartCommand": "printf 'after\n' > /root/.config/lazygit/config.yml && grep -q after /root/.config/lazygit/config.yml && printf 'new\n' > /root/.config/lazygit/new.json && grep -q new /root/.config/lazygit/new.json"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "lazygit-source"
target = ".config/lazygit"
read_only = false
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let real_config = workspace_root.join("dotfiles-real/config.yml");
    let state_root = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })
        .expect("HOME should be set for decune state path");
    let expected_skeleton = state_root
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("dotfile-mount-skeleton")
        .join(".config")
        .join("lazygit");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        assert_eq!(std::fs::read_to_string(&real_config).unwrap(), "after\n");
        assert_eq!(
            std::fs::read_to_string(expected_skeleton.join("new.json")).unwrap(),
            "new\n"
        );
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_replaces_existing_dotfile_symlink() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/gitconfig", "[user]\nemail = decune@example.com\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "postStartCommand": "test -L /root/.gitconfig && test \"$(readlink /root/.gitconfig)\" = \"/opt/decune/dotfiles/.gitconfig\" && grep -q decune@example.com /root/.gitconfig"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r"
            FROM alpine:3.20
            RUN ln -s /tmp/old-gitconfig /root/.gitconfig
            ",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "dotfiles/gitconfig"
target = ".gitconfig"
on_conflict = "replace-symlink"
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_fails_dotfile_setup_when_conflict_policy_is_fail() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/gitconfig", "[user]\nname = decune\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r"
            FROM alpine:3.20
            RUN printf '[user]\nname = original\n' > /root/.gitconfig
            ",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "dotfiles/gitconfig"
target = ".gitconfig"
on_conflict = "fail"
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .failure()
            .stderr(predicate::str::contains("Failed to setup dotfiles"))
            .stderr(predicate::str::contains("Dotfile target already exists"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_backs_up_existing_dotfile_target_when_requested() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/gitconfig", "[user]\nname = decune\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "postStartCommand": "test -L /root/.gitconfig && test \"$(readlink /root/.gitconfig)\" = \"/opt/decune/dotfiles/.gitconfig\" && backup=$(find /root -maxdepth 1 -name '.gitconfig.decune-backup-*' -type f | head -n 1) && test -n \"$backup\" && grep -q original \"$backup\" && grep -q decune /root/.gitconfig"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r"
            FROM alpine:3.20
            RUN printf '[user]\nname = original\n' > /root/.gitconfig
            ",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "dotfiles/gitconfig"
target = ".gitconfig"
on_conflict = "backup"
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_sets_up_dotfile_in_nonstandard_remote_user_home() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/toolrc", "configured\n")
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/nonstandard-home-dotfiles-{}:latest",
        workspace_id(&workspace_root)
    );
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}",
                  "remoteUser": "node",
                  "postStartCommand": "test -L /usr/local/share/node/.config/tool && test \"$(readlink /usr/local/share/node/.config/tool)\" = \"/opt/decune/dotfiles/.config/tool\" && grep -q configured /usr/local/share/node/.config/tool && test ! -e /home/node/.config/tool"
                }}
                "#
            ),
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "dotfiles/toolrc"
target = ".config/tool"
"#,
        )
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
        remove_image_if_exists(&image_tag).unwrap();
        create_image_with_nonstandard_home_user(&workspace_root, &image_tag).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = remove_image_if_exists(&image_tag);
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}
