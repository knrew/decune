# decune の複数クローン同時利用ガイド (Compose clone isolation)

この文書は、同じ Docker Compose-based リポジトリの複数 clone を同じ Docker daemon 上で同時に使うための clone isolation の使い方と対処をまとめた利用者向けガイドです。挙動の契約は [specification.md 8.9 節](specification.md#89-clone-isolation)を正とします。published port の relocation 一般は [ports.md](ports.md)、設定ファイルの場所と重ね合わせは [configuration.md](configuration.md) を参照してください。

## 解決する問題

同じ Docker Compose-based workspace の複数 clone を同時に起動すると、Compose file に固定値で書かれた次の要素が clone 間で衝突します。

- fixed TCP published host port(例: `3000:3000`)
- 明示的な `container_name`
- top-level `networks` / `volumes` / `configs` / `secrets` の固定 `name`
- 固定 IPv4 subnet(`ipam.config` の `subnet`)
- 固定 gateway / subnet を service の environment に埋め込んだ endpoint

Compose project name と既定命名の network / volume は、clone isolation の有効無効にかかわらず常に workspace ごとの名前になります。clone isolation は、上記のような固定値を workspace ごとの値へ書き換える opt-in 機能で、既定では無効です。有効にしても `external: true` の resource は共有契約とみなして書き換えません。

固定 subnet と固定名の衝突は、clone isolation の有効無効にかかわらず `docker compose up` の前に preflight で検出されます。opt-in していない対象は検出のみを行い、衝突があれば diagnostic code で停止します([specification.md 8.9.2 節](specification.md#892-preflight))。

## 有効化

各 clone の `<workspace>/.decune/config.toml` に同じ設定を置きます。

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

- fixed TCP published port は空いている host port へ relocation されます。`[compose.published_ports].automatic_relocation` 未指定時の既定が true に切り替わります。確認方法と explicit published port mapping は [ports.md](ports.md) を参照してください。
- 明示的な `container_name` と、non-external な top-level resource の固定 `name` は workspace 固有名(`<name>-<workspace_id>`)へ書き換えられます。`[compose.clone_isolation.names]` の `rewrite_container_names` / `rewrite_resource_names`(いずれも既定 true)で個別に無効化できます。
- `[compose.clone_isolation.networks].relocation = true` と `subnet_pool` を設定した場合、固定 IPv4 subnet は pool 内の workspace 固有 subnet へ移ります。

設定 key のスキーマと既定値は [specification.md 5.12 節](specification.md#512-composeclone_isolation)を参照してください。

## name rewrite

- 固定名 volume は clone ごとに別 volume になるため、データも clone 間で分離されます。
- container 間の通信では、元の `container_name` が DNS alias として維持されます。`network_mode` / `ipc` / `pid` / `volumes_from` / `external_links` が書き換え対象の固定名を参照している場合は、同じ workspace 固有名へ追随します。service 名による参照と、外部 container への参照は変更されません。書き換え規則は [specification.md 8.9.5 節](specification.md#895-name-rewrite)を参照してください。
- host 側から固定名を直接使う tool(`docker exec <元名>` など)は、書き換え後の名前へ更新してください。実際の container 名は `docker ps` で確認できます。
- `volumes_from` / `external_links` の参照を書き換える構成では Docker Compose v2.24.4 以上が必要です。`network_mode` / `ipc` / `pid` の参照だけを書き換える構成には、この追加要件はありません(条件の一覧は [specification.md 2.2 節](specification.md#22-docker-compose-v2244-が必要になる条件))。

## network relocation

固定 IPv4 subnet を workspace ごとに分離するには、`relocation = true` と `subnet_pool` を設定します。

```toml
[compose.clone_isolation.networks]
relocation = true
subnet_pool = "10.224.0.0/16"
# subnet_prefix = 24
```

- `subnet_pool` は relocation 先を割り当てる IPv4 CIDR pool で、`relocation = true` のとき必須です。`subnet_prefix` を省略すると元 subnet の prefix 長を維持します。
- 元 IPAM config の `gateway` / `ip_range` / `aux_addresses` は、元 subnet 内の相対位置(offset)を保って新しい subnet へ移ります。`subnet_prefix` を狭めた結果それらを収容できない場合は、起動前に error になります。
- 固定 IPv4 subnet を検出した構成では、割り当て結果が元 subnet と同じでも decune-generated Compose override に Compose `!override` tag を使うため、Docker Compose v2.24.4 以上が必要です。

割り当て規則と再利用の契約は [specification.md 8.9.3 節](specification.md#893-network-relocation)を参照してください。

## clone isolation endpoint 宣言

固定 gateway や subnet を service の environment に埋め込んでいる構成では、`[[compose.clone_isolation.endpoints]]` で対象 service・環境変数・値 template を宣言します。

```toml
[[compose.clone_isolation.endpoints]]
service = "app"
env = "HOST_AGENT_ENDPOINT"
value = "grpc://${decune.network.grpc.gateway}:50051"
```

- `${decune.network.<network-key>.gateway}` と `${decune.network.<network-key>.subnet}` は relocation 後の値へ展開されます。それ以外の `$` は Compose の host environment interpolation を行わず、literal として container へ渡されます。
- relocate された network に接続する service の environment に旧 gateway / 旧 subnet の address が残っていると、`decune up` は起動前に停止します。1 つの環境変数から複数の relocated network を参照している場合は、各 network の旧 address を対応する placeholder に置き換えてください。
- この stale 検出の対象は service の environment だけです。`extra_hosts`、command、config file 内の旧 address は自動で検出・書き換えされないため、利用者が追随させてください。
- endpoint を宣言したまま `[compose.clone_isolation].enabled = false` にすると、宣言は無効として扱われ warning が表示されます。

placeholder と render の契約は [specification.md 8.9.4 節](specification.md#894-clone-isolation-endpoint-宣言)を参照してください。

## 制限

- `external: true` の network / volume / config / secret は書き換えません。
- IPv6 subnet と、service の `ipv4_address` / `ipv6_address` / `link_local_ips` は relocation できず、該当する構成では起動前に停止します。
- `aux_addresses` の値は IPAM config 内で移されますが、その元 address を environment や他の設定から直接参照している箇所は追随しません。

詳細は [specification.md 8.9.3 節](specification.md#893-network-relocation)を参照してください。

## トラブルシューティング

diagnostic code への対処は次のとおりです。発生条件の定義は [specification.md 13.2 節](specification.md#132-compose-clone-isolation)を参照してください。

- `compose_fixed_name_conflict`: 固定 `container_name` または top-level resource の固定 `name` が、既存の Docker resource と衝突しています。診断に表示される衝突相手を確認して停止・削除するか、clone isolation の name rewrite を有効化してください。
- `compose_network_subnet_overlap`: 固定 IPv4 subnet が既存 Docker network の subnet と重複しています。衝突相手の network を確認して削除するか、network relocation(`relocation = true` と `subnet_pool`)を有効化してください。
- `compose_clone_isolation_invalid`: clone isolation の設定または対象 network の状態が invalid です。network relocation を無効にしたまま placeholder を参照していないか、`subnet_prefix` が gateway / `ip_range` / `aux_addresses` を収容できるかを見直してください。既存 network の再作成が必要で container が接続されたままの場合は、`decune down` の後に `decune rebuild` を実行してください。
- `compose_clone_isolation_unsupported`: IPv6 / static address、解釈できない IPAM 設定、`!override` を必要とする構成での古い Docker Compose など、decune が安全に書き換えできない構成です。該当構成を Compose file から外すか、Docker Compose を v2.24.4 以上へ更新してください。
- `compose_clone_isolation_pool_exhausted`: `subnet_pool` 内に割り当て可能な空き slot がありません。pool を広げるか、`subnet_prefix` を見直すか、不要になった workspace の network を削除してください。
- `compose_clone_isolation_endpoint_unsafe`: relocation 後の service environment に旧 subnet / 旧 gateway の address が残っています。該当の環境変数を `[[compose.clone_isolation.endpoints]]` で宣言し、旧 address を placeholder へ置き換えてください。

### 復旧手順

- 既存 network の subnet 変更が必要で container が接続されたままの場合は、`decune down` の後に `decune rebuild` を実行します。subnet が planned 値と一致していても、gateway や `ip_range` / `aux_addresses` の不一致で再作成が必要になる場合があります。
- 別 process で複数の `decune up` を同時実行すると、preflight 後の network 作成までに同じ subnet を選び、Docker 側で subnet 重複エラーになる場合があります。先に成功した起動の完了後に、失敗した `decune up` を再実行してください。
