# decune の複数クローン同時利用ガイド (Compose clone isolation)

この文書は、同じ Docker Compose-based リポジトリの複数クローンを同じ Docker デーモン上で同時に使うための clone isolation の使い方と対処をまとめた利用者向けガイドです。挙動の契約は [specification.md 8.9 節](specification.md#89-clone-isolation)を正とします。published port の relocation 一般は [ports.md](ports.md)、設定ファイルの場所と重ね合わせは [configuration.md](configuration.md) を参照してください。

## 解決する問題

同じ Docker Compose-based ワークスペースの複数クローンを同時に起動すると、Compose ファイルに固定値で書かれた次の要素がクローン間で衝突します。

- fixed TCP published host port(例: `3000:3000`)
- 明示的な `container_name`
- トップレベルの `networks` / `volumes` / `configs` / `secrets` の固定 `name`
- 固定 IPv4 サブネット(`ipam.config` の `subnet`)
- 固定のゲートウェイ / サブネットをサービスの環境変数に埋め込んだエンドポイント

Compose プロジェクト名と既定命名のネットワーク / ボリュームは、clone isolation の有効無効にかかわらず常にワークスペースごとの名前になります。clone isolation は、上記のような固定値をワークスペースごとの値へ書き換えるオプトインの機能で、既定では無効です。有効にしても `external: true` のリソースは共有契約とみなして書き換えません。

固定サブネットと固定名の衝突は、clone isolation の有効無効にかかわらず `docker compose up` の前に preflight で検出されます。オプトインしていない対象は検出のみを行い、衝突があれば diagnostic code で停止します([specification.md 8.9.2 節](specification.md#892-preflight))。

## 有効化

各クローンの `<workspace>/.decune/config.toml` に同じ設定を置きます。

```toml
version = 1

[compose.clone_isolation]
enabled = true

[compose.clone_isolation.networks]
relocation = true
subnet_pool = "10.224.0.0/16"

[[compose.clone_isolation.endpoints]]
service = "app"
env = "AGENT_ENDPOINT"
value = "http://${decune.network.appnet.gateway}:9000"
```

`enabled = true` にすると次が有効になります。

- fixed TCP published port は空いているホスト側ポートへ relocation されます。`[compose.published_ports].automatic_relocation` 未指定時の既定が true に切り替わります。確認方法と explicit published port mapping は [ports.md](ports.md) を参照してください。
- 明示的な `container_name` と、external ではないトップレベルリソースの固定 `name` はワークスペース固有名(`<name>-<workspace_id>`)へ書き換えられます。`[compose.clone_isolation.names]` の `rewrite_container_names` / `rewrite_resource_names`(いずれも既定 true)で個別に無効化できます。
- `[compose.clone_isolation.networks].relocation = true` と `subnet_pool` を設定した場合、固定 IPv4 サブネットはプール内のワークスペース固有サブネットへ移ります。

設定キーのスキーマと既定値は [specification.md 5.12 節](specification.md#512-composeclone_isolation)を参照してください。

## name rewrite

- 固定名ボリュームはクローンごとに別ボリュームになるため、データもクローン間で分離されます。
- コンテナ間の通信では、元の `container_name` が DNS エイリアスとして維持されます。`network_mode` / `ipc` / `pid` / `volumes_from` / `external_links` が書き換え対象の固定名を参照している場合は、同じワークスペース固有名へ追随します。サービス名による参照と、外部コンテナへの参照は変更されません。書き換え規則は [specification.md 8.9.5 節](specification.md#895-name-rewrite)を参照してください。
- ホスト側から固定名を直接使うツール(`docker exec <元名>` など)は、書き換え後の名前へ更新してください。実際のコンテナ名は `docker ps` で確認できます。
- `volumes_from` / `external_links` の参照を書き換える構成では Docker Compose v2.24.4 以上が必要です。`network_mode` / `ipc` / `pid` の参照だけを書き換える構成には、この追加要件はありません(条件の一覧は [specification.md 2.2 節](specification.md#22-docker-compose-v2244-が必要になる条件))。

## network relocation

固定 IPv4 サブネットをワークスペースごとに分離するには、`relocation = true` と `subnet_pool` を設定します。

```toml
[compose.clone_isolation.networks]
relocation = true
subnet_pool = "10.224.0.0/16"
# subnet_prefix = 24
```

- `subnet_pool` は relocation 先を割り当てる IPv4 CIDR のプールで、`relocation = true` のとき必須です。`subnet_prefix` を省略すると元のサブネットのプレフィックス長を維持します。
- 元の IPAM 設定の `gateway` / `ip_range` / `aux_addresses` は、元のサブネット内の相対位置(オフセット)を保って新しいサブネットへ移ります。`subnet_prefix` を狭めた結果それらを収容できない場合は、起動前にエラーになります。
- 固定 IPv4 サブネットを検出した構成では、割り当て結果が元のサブネットと同じでも decune-generated Compose override に Compose の `!override` タグを使うため、Docker Compose v2.24.4 以上が必要です。

割り当て規則と再利用の契約は [specification.md 8.9.3 節](specification.md#893-network-relocation)を参照してください。

## clone isolation endpoint 宣言

固定のゲートウェイやサブネットをサービスの環境変数に埋め込んでいる構成では、`[[compose.clone_isolation.endpoints]]` で対象のサービス・環境変数・値のテンプレートを宣言します。

```toml
[[compose.clone_isolation.endpoints]]
service = "app"
env = "HOST_AGENT_ENDPOINT"
value = "grpc://${decune.network.grpc.gateway}:50051"
```

- `${decune.network.<network-key>.gateway}` と `${decune.network.<network-key>.subnet}` は relocation 後の値へ展開されます。それ以外の `$` は Compose のホスト環境変数の展開を行わず、そのままの文字列としてコンテナへ渡されます。
- relocation されたネットワークに接続するサービスの環境変数に旧ゲートウェイ / 旧サブネットのアドレスが残っていると、`decune up` は起動前に停止します。1 つの環境変数から複数の relocation されたネットワークを参照している場合は、各ネットワークの旧アドレスを対応するプレースホルダーに置き換えてください。
- この stale 検出の対象はサービスの環境変数だけです。`extra_hosts`、`command`、設定ファイル内の旧アドレスは自動で検出・書き換えされないため、利用者が追随させてください。
- endpoint を宣言したまま `[compose.clone_isolation].enabled = false` にすると、宣言は無効として扱われ警告が表示されます。

プレースホルダーと展開の契約は [specification.md 8.9.4 節](specification.md#894-clone-isolation-endpoint-宣言)を参照してください。

## 制限

- `external: true` のネットワーク / ボリューム / config / secret は書き換えません。
- IPv6 サブネットと、サービスの `ipv4_address` / `ipv6_address` / `link_local_ips` は relocation できず、該当する構成では起動前に停止します。
- `aux_addresses` の値は IPAM 設定内で移されますが、その元アドレスを環境変数や他の設定から直接参照している箇所は追随しません。

詳細は [specification.md 8.9.3 節](specification.md#893-network-relocation)を参照してください。

## トラブルシューティング

diagnostic code への対処は次のとおりです。発生条件の定義は [specification.md 13.2 節](specification.md#132-compose-clone-isolation)を参照してください。

- `compose_fixed_name_conflict`: 固定 `container_name` またはトップレベルリソースの固定 `name` が、既存の Docker リソースと衝突しています。診断に表示される衝突相手を確認して停止・削除するか、clone isolation の name rewrite を有効化してください。
- `compose_network_subnet_overlap`: 固定 IPv4 サブネットが既存 Docker ネットワークのサブネットと重複しています。衝突相手のネットワークを確認して削除するか、network relocation(`relocation = true` と `subnet_pool`)を有効化してください。
- `compose_clone_isolation_invalid`: clone isolation の設定または対象ネットワークの状態が不正です。network relocation を無効にしたままプレースホルダーを参照していないか、`subnet_prefix` がゲートウェイ / `ip_range` / `aux_addresses` を収容できるかを見直してください。既存ネットワークの再作成が必要でコンテナが接続されたままの場合は、`decune down` の後に `decune rebuild` を実行してください。
- `compose_clone_isolation_unsupported`: IPv6 / 固定アドレス、解釈できない IPAM 設定、`!override` を必要とする構成での古い Docker Compose など、decune が安全に書き換えできない構成です。該当する構成を Compose ファイルから外すか、Docker Compose を v2.24.4 以上へ更新してください。
- `compose_clone_isolation_pool_exhausted`: `subnet_pool` 内に割り当て可能な空きスロットがありません。プールを広げるか、`subnet_prefix` を見直すか、不要になったワークスペースのネットワークを削除してください。
- `compose_clone_isolation_endpoint_unsafe`: relocation 後のサービスの環境変数に旧サブネット / 旧ゲートウェイのアドレスが残っています。該当の環境変数を `[[compose.clone_isolation.endpoints]]` で宣言し、旧アドレスをプレースホルダーへ置き換えてください。

### 復旧手順

- 既存ネットワークのサブネット変更が必要でコンテナが接続されたままの場合は、`decune down` の後に `decune rebuild` を実行します。サブネットが planned の値と一致していても、ゲートウェイや `ip_range` / `aux_addresses` の不一致で再作成が必要になる場合があります。
- 別プロセスで複数の `decune up` を同時実行すると、preflight 後のネットワーク作成までに同じサブネットを選び、Docker 側でサブネット重複エラーになる場合があります。先に成功した起動の完了後に、失敗した `decune up` を再実行してください。
