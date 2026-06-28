use std::{fs, path::PathBuf};

use anyhow::{Context, Result};

use super::{context::ResolvedBuildContext, dockerfile_user, shell_quote};
use crate::docker::image::validate_image_name;

const UID_GID_SYNC_SCRIPT_FILE: &str = "sync-uid-gid.sh";
const UID_GID_SYNC_SCRIPT_TARGET: &str = "/tmp/decune-sync-uid-gid.sh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UidGidSyncLayerBuildInput {
    pub(crate) base_image: String,
    pub(crate) final_user: String,
    pub(crate) target_user: String,
    pub(crate) old_uid: u32,
    pub(crate) old_gid: u32,
    pub(crate) new_uid: u32,
    pub(crate) new_gid: u32,
    pub(crate) context_dir: PathBuf,
}

pub(crate) fn prepare_uid_gid_sync_layer_build_context(
    input: &UidGidSyncLayerBuildInput,
) -> Result<ResolvedBuildContext> {
    if input.context_dir.exists() {
        fs::remove_dir_all(&input.context_dir).with_context(|| {
            format!(
                "Failed to remove existing UID/GID sync build context: {}",
                input.context_dir.display()
            )
        })?;
    }
    fs::create_dir_all(&input.context_dir).with_context(|| {
        format!(
            "Failed to create UID/GID sync build context: {}",
            input.context_dir.display()
        )
    })?;

    let dockerfile_path = input.context_dir.join("Dockerfile");
    fs::write(&dockerfile_path, uid_gid_sync_layer_dockerfile(input)?).with_context(|| {
        format!(
            "Failed to write UID/GID sync Dockerfile: {}",
            dockerfile_path.display()
        )
    })?;
    let script_path = input.context_dir.join(UID_GID_SYNC_SCRIPT_FILE);
    fs::write(&script_path, uid_gid_sync_script(input)).with_context(|| {
        format!(
            "Failed to write UID/GID sync script: {}",
            script_path.display()
        )
    })?;

    Ok(ResolvedBuildContext {
        context_dir: input.context_dir.clone(),
        dockerfile_path,
        dockerfile_in_context: PathBuf::from("Dockerfile"),
        dockerignore_path: None,
    })
}

fn uid_gid_sync_layer_dockerfile(input: &UidGidSyncLayerBuildInput) -> Result<String> {
    validate_image_name(&input.base_image)?;
    let final_user = dockerfile_user(&input.final_user)?;
    Ok(format!(
        "FROM {}\nUSER root\nCOPY {UID_GID_SYNC_SCRIPT_FILE} {UID_GID_SYNC_SCRIPT_TARGET}\nRUN /bin/sh {UID_GID_SYNC_SCRIPT_TARGET} && rm -f {UID_GID_SYNC_SCRIPT_TARGET}\nUSER {final_user}\n",
        input.base_image
    ))
}

fn uid_gid_sync_script(input: &UidGidSyncLayerBuildInput) -> String {
    let target_user = shell_quote(&input.target_user);
    format!(
        r#"set -eu
target_user={target_user}
old_uid={old_uid}
old_gid={old_gid}
new_uid={new_uid}
new_gid={new_gid}

conflict_user="$(awk -F: -v uid="$new_uid" -v user="$target_user" '$3 == uid && $1 != user {{ print $1; exit }}' /etc/passwd)"
if [ -n "$conflict_user" ]; then
    echo "UID/GID sync target UID conflicts with existing user: $conflict_user ($new_uid)" >&2
    exit 1
fi

if [ -f /etc/group ]; then
    target_group_count="$(awk -F: -v gid="$old_gid" '$3 == gid {{ count++ }} END {{ print count + 0 }}' /etc/group)"
    if [ "$target_group_count" -gt 1 ]; then
        echo "UID/GID sync target GID matches multiple groups: $old_gid" >&2
        exit 1
    fi
    if [ "$target_group_count" -eq 1 ]; then
        target_group="$(awk -F: -v gid="$old_gid" '$3 == gid {{ print $1; exit }}' /etc/group)"
    else
        target_group=""
    fi
    conflict_group="$(awk -F: -v gid="$new_gid" -v group="$target_group" '$3 == gid && (group == "" || $1 != group) {{ print $1; exit }}' /etc/group)"
    if [ -n "$conflict_group" ]; then
        echo "UID/GID sync target GID conflicts with existing group: $conflict_group ($new_gid)" >&2
        exit 1
    fi
    if [ -n "$target_group" ]; then
        tmp_group="$(mktemp)"
        awk -F: -v OFS=: -v group="$target_group" -v gid="$new_gid" '
            $1 == group {{ $3 = gid }}
            {{ print }}
        ' /etc/group > "$tmp_group"
        cat "$tmp_group" >/etc/group
        rm -f "$tmp_group"
    fi
fi

if [ "$old_uid" = "$new_uid" ] && [ "$old_gid" = "$new_gid" ]; then
    exit 0
fi

target_home="$(awk -F: -v user="$target_user" '$1 == user {{ print $6; exit }}' /etc/passwd)"
tmp_passwd="$(mktemp)"
status=0
awk -F: -v OFS=: -v user="$target_user" -v uid="$new_uid" -v gid="$new_gid" '
    $1 == user {{ $3 = uid; $4 = gid; found = 1 }}
    {{ print }}
    END {{ if (!found) exit 42 }}
' /etc/passwd > "$tmp_passwd" || status=$?
if [ "$status" -eq 42 ]; then
    echo "UID/GID sync target user is missing: $target_user" >&2
    exit 1
elif [ "$status" -ne 0 ]; then
    exit "$status"
fi
cat "$tmp_passwd" >/etc/passwd
rm -f "$tmp_passwd"

if [ -n "$target_home" ] && [ -d "$target_home" ]; then
    chown -R "$new_uid:$new_gid" "$target_home"
fi
"#,
        target_user = target_user,
        old_uid = input.old_uid,
        old_gid = input.old_gid,
        new_uid = input.new_uid,
        new_gid = input.new_gid,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{
        UID_GID_SYNC_SCRIPT_FILE, UidGidSyncLayerBuildInput,
        prepare_uid_gid_sync_layer_build_context,
    };
    use crate::docker::build::tar::{create_build_context_tar, tar_contains_path};

    #[test]
    fn uid_gid_sync_layer_build_context_writes_sync_dockerfile_and_script() {
        let temp = tempdir("uid-gid-sync-layer-build-context");
        let context_dir = temp.path().join("context");
        let context = prepare_uid_gid_sync_layer_build_context(&UidGidSyncLayerBuildInput {
            base_image: "alpine:3.20".to_owned(),
            final_user: "vscode".to_owned(),
            target_user: "vscode".to_owned(),
            old_uid: 2001,
            old_gid: 2001,
            new_uid: 1000,
            new_gid: 1000,
            context_dir,
        })
        .unwrap();

        let tar = create_build_context_tar(&context).unwrap();
        let dockerfile = fs::read_to_string(context.dockerfile_path).unwrap();
        let script =
            fs::read_to_string(context.context_dir.join(UID_GID_SYNC_SCRIPT_FILE)).unwrap();

        assert!(tar_contains_path(&tar, "Dockerfile"));
        assert!(tar_contains_path(&tar, UID_GID_SYNC_SCRIPT_FILE));
        assert!(dockerfile.contains("FROM alpine:3.20"));
        assert!(dockerfile.contains("USER root"));
        assert!(dockerfile.contains("USER vscode"));
        assert!(script.contains("target_user='vscode'"));
        assert!(script.contains("UID/GID sync target UID conflicts"));
        assert!(script.contains("UID/GID sync target GID conflicts"));
        assert!(script.contains("UID/GID sync target GID matches multiple groups"));
        assert!(script.contains("cat \"$tmp_passwd\" >/etc/passwd"));
        assert!(script.contains("cat \"$tmp_group\" >/etc/group"));
        assert!(script.contains("chown -R \"$new_uid:$new_gid\" \"$target_home\""));
        let uid_conflict_check = script.find("conflict_user=").unwrap();
        let ambiguous_gid_check = script.find("target_group_count=").unwrap();
        let gid_conflict_check = script.find("conflict_group=").unwrap();
        let no_change_exit = script.find("exit 0").unwrap();
        assert!(uid_conflict_check < no_change_exit);
        assert!(ambiguous_gid_check < no_change_exit);
        assert!(gid_conflict_check < no_change_exit);
    }

    fn tempdir(name: &str) -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!("decune-docker-build-{name}-"))
            .tempdir()
            .unwrap()
    }
}
