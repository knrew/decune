//! `remove` が削除する volume と、削除の後に残った named volume の報告。

use std::collections::BTreeSet;

use anyhow::{Context, Result};

use crate::{
    docker::{client::DockerClient, container::ContainerInspect, volume::remove_volume},
    runtime::docker_cli::VolumeRemoval,
    ui,
};

/// Docker が匿名 volume を作るときに付けるラベル。
const ANONYMOUS_VOLUME_LABEL: &str = "com.docker.volume.anonymous";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeptVolumeReason {
    /// 削除したワークスペースの decune-managed ボリュームではない。
    NotManaged,
    /// decune-managed ボリュームだが、他のコンテナが参照しているため Docker が削除を拒否した。
    InUse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KeptVolume {
    pub(super) name: String,
    pub(super) reason: KeptVolumeReason,
}

/// 一回の `remove` で、削除したコンテナが mount していた named volume と、使用中で削除を
/// 拒否された decune-managed ボリュームを集める。残した volume は、すべての削除が終わった後に
/// `kept_volumes` で確かめる。`--all-workspaces` で、あるワークスペースの削除の後に残っても、
/// 後のワークスペースの削除で消えた volume を残したものとして示さないためである。
#[derive(Debug, Default)]
pub(super) struct VolumeRemovalReport {
    mounted: BTreeSet<String>,
    in_use: BTreeSet<String>,
}

impl VolumeRemovalReport {
    pub(super) fn record_mounted_volumes(&mut self, volumes: &[String]) {
        self.mounted.extend(volumes.iter().cloned());
    }

    /// decune-managed ボリュームを削除する。使用中で拒否されたら、失敗にせずに記録する。
    pub(super) async fn remove_managed_volume(
        &mut self,
        client: &DockerClient,
        volume: &str,
    ) -> Result<VolumeRemoval> {
        let removal = remove_volume(client, volume, true).await?;
        match removal {
            VolumeRemoval::Removed => ui::done(&format!("Removed Docker volume: {volume}")),
            VolumeRemoval::InUse => {
                self.in_use.insert(volume.to_owned());
            }
        }
        Ok(removal)
    }

    /// 残した named volume を名前の順に返す。匿名 volume は含めない。
    pub(super) async fn kept_volumes(&self, client: &DockerClient) -> Result<Vec<KeptVolume>> {
        let mut kept = Vec::new();
        for name in self.mounted.union(&self.in_use) {
            if self.in_use.contains(name) {
                kept.push(KeptVolume {
                    name: name.clone(),
                    reason: KeptVolumeReason::InUse,
                });
                continue;
            }
            let Some(volume) = client
                .cli()
                .inspect_volume_if_present(name)
                .await
                .with_context(|| format!("Failed to inspect Docker volume: {name}"))?
            else {
                continue;
            };
            if volume
                .labels
                .as_ref()
                .is_some_and(|labels| labels.contains_key(ANONYMOUS_VOLUME_LABEL))
            {
                continue;
            }
            kept.push(KeptVolume {
                name: name.clone(),
                reason: KeptVolumeReason::NotManaged,
            });
        }
        Ok(kept)
    }
}

/// コンテナが mount している volume の名前。bind mount と tmpfs は含めない。匿名 volume は
/// 名前だけでは見分けられないので含め、`kept_volumes` でラベルから除く。
pub(super) fn mounted_volume_names(container: &ContainerInspect) -> Vec<String> {
    container
        .mounts
        .iter()
        .flatten()
        .filter(|mount| mount.typ.as_deref() == Some("volume"))
        .filter_map(|mount| mount.name.clone())
        .filter(|name| !name.trim().is_empty())
        .collect()
}

/// 残した volume を出力の最後に示す。`owner` は、decune-managed ボリュームかどうかを判断した
/// ワークスペースの呼び方(`this workspace` など)である。
pub(super) fn print_kept_volumes(kept: &[KeptVolume], owner: &str) {
    for volume in kept {
        match volume.reason {
            KeptVolumeReason::NotManaged => ui::notice(&format!(
                "Kept Docker volume: {} (not a decune-managed volume of {owner})",
                volume.name
            )),
            KeptVolumeReason::InUse => ui::warn(&format!(
                "Kept Docker volume: {} (in use by another container)",
                volume.name
            )),
        }
    }
}
