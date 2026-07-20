# decune のポート利用ガイド

この文書は、decune の port forwarding と Docker published port の使い分け、設定方法、トラブル時の対処をまとめた利用者向けガイドです。挙動の契約は [specification.md 9 章](specification.md#9-ポート)と [8.8 節](specification.md#88-published-port-mapping-と-relocation)を正とします。日常操作は [usage.md](usage.md)、複数 clone の同時利用に伴う分離は [clone-isolation.md](clone-isolation.md)、ポート以外の設定は [configuration.md](configuration.md) を参照してください。

## forwarding と published port の違い

container の port を host 側で使えるようにする経路は 2 つあります。

- port forwarding: `forwardPorts`、decune `[[ports]]`、CLI `-p` による decune の機能です。Docker published port ではありません。既定では host 側 `127.0.0.1` で listen し、container-side agent 経由で container port へ転送します。container 内で localhost にだけ listen しているプロセスにも届きます。
- published port: Docker が publish する host 側 binding です。image-based / Dockerfile-based configuration では Dev Container `appPort`、Docker Compose-based configuration では Compose service の `ports` で指定します。

使い分けの目安:

- 手元の開発作業からアクセスできれば十分な port は port forwarding を使います。attached `decune up` session の間だけ維持され、既定では `127.0.0.1` に閉じます。
- attached session と無関係に公開し続けたい port や、`decune up --detach` で使う port は published port を使います。

decune の port forwarding と、`appPort` から decune が生成する published port metadata は TCP-only です。これらの設定で `/udp` を指定すると unsupported error になります。Compose service `ports` などで Docker が実際に publish している UDP binding は、`decune ports` の一覧には表示されます。区別の契約は [specification.md 9.1 節](specification.md#91-forwarding-と-published-port-の区別)と [9.2 節](specification.md#92-tcp-only)を参照してください。

## manual port forwarding

port forwarding は次の 3 か所で指定できます。

- `devcontainer.json` の `forwardPorts`(例: `[5173]`)
- decune config の `[[ports]]`(スキーマは [specification.md 5.9 節](specification.md#59-ports))
- CLI の `decune up -p <SPEC>`(例: `-p 3000`、`-p 8080:3000`、`-p 127.0.0.1:8080:3000`。全形式は [specification.md 3.2 節](specification.md#32-up))

```toml
[[ports]]
container = 3000
host = 3000
host_ip = "127.0.0.1"
label = "web"
```

- `host` を省略すると container port と同じ番号を試し、使用中なら空き port へ fallback します。fallback を warning で知りたい場合は `require_local = true` を設定します。実際に使われた endpoint は `decune ports` で確認できます。
- 同じ port を複数の場所で指定した場合の優先順位と fallback の契約は [specification.md 9.5 節](specification.md#95-manual-forwarding-の優先順位と-fallback)を参照してください。
- Docker Compose-based configuration で primary service(`service` で指定した Compose service)以外へ転送する場合は、`forwardPorts` の `"service:port"` 形式(例: `"db:5432"`)または `[[ports]].service` を明示します。対象の sidecar service には forwarding 用の artifact だけが配置されます([specification.md 9.6 節](specification.md#96-compose-モードの-service-解決と-sidecar-forwarding))。

## automatic port forwarding

decune は既定で、primary container 内の TCP listening port を検出して自動的に forwarding します。

- 無効化するには `decune up --no-auto-forward` を使うか、`[ports.auto].enabled = false` を設定します。
- 検出範囲(`min` / `max`)、除外 port(`ignore`)、検出時の通知(`on_auto_forward`)は `[ports.auto]` で調整します。スキーマは [specification.md 5.10 節](specification.md#510-portsauto)、検出の挙動は [9.7 節](specification.md#97-automatic-forwarding)を参照してください。

```toml
[ports.auto]
ignore = [5432]
on_auto_forward = "silent"
```

## published port の指定

### image-based / Dockerfile-based configuration: `appPort`

`appPort` は Docker published port としてコンテナ作成時に確定します。既存コンテナへ後付けできないため、変更を反映するには `decune rebuild` を実行します。host IP を指定しない場合は Docker の既定で全 interface に公開され得るため、warning が表示されます([specification.md 9.3 節](specification.md#93-appport))。

### Docker Compose-based configuration: service の `ports`

Docker published port は Compose service の `ports` に書きます。Compose モードでは `appPort` は unsupported error です([specification.md 8.1 節](specification.md#81-委譲原則と制限))。

## `--detach` とポート

`decune up --detach` では `up` 終了時に host daemon も停止するため、manual / automatic port forwarding は維持されません。`--detach` と CLI `-p` の併用は error になり、設定由来の `forwardPorts` / `[[ports]]` は warning を出して無視されます。detached container で公開が必要な port は published port(`appPort` または Compose service の `ports`)を使ってください([specification.md 3.2 節](specification.md#32-up))。

relocation(後述)された endpoint も Docker/Compose の published binding のままであり、decune forwarding ではありません。そのため `--detach` で起動した後も維持されます。

## Compose published port の mapping と relocation

複数 clone の同時起動などで requested endpoint が使えない場合に備えて、decune は Compose の fixed TCP published port の host endpoint を調整する 2 つの仕組みを持ちます。planning の規則は [specification.md 8.8 節](specification.md#88-published-port-mapping-と-relocation)を正とします。

- automatic relocation: requested host port が使えないとき、次に利用可能な host port を自動探索する policy です。既定は無効です。
- explicit mapping: 特定の published port の host endpoint を明示的に固定します。automatic relocation の有効/無効とは独立に適用されます。

対象は fixed TCP published port(`3000:3000` や `127.0.0.1:3000:3000` のように host port を明示した entry)だけです。UDP、port range、host port を省略した entry(`3000` だけ等)、`network_mode: host` の service にある port は対象外です。

### automatic relocation の有効化

```toml
[compose.published_ports]
automatic_relocation = true
```

- この実行だけ切り替えるには `decune up --automatic-published-port-relocation` / `--no-automatic-published-port-relocation` を使います。
- `[compose.clone_isolation].enabled = true` の場合、`automatic_relocation` 未指定時の既定は true になります([clone-isolation.md](clone-isolation.md))。
- `--no-auto-forward` は automatic port forwarding だけを無効化し、この policy には影響しません。
- いったん relocation された published port は、元の port を塞いでいた process が終了しても `decune up` では維持されます。requested port へ戻すには `decune rebuild` を実行してください。
- relocation を warning で知りたい場合は `warn_on_relocation = true` を設定します。既定では warning を出しませんが、既存 Compose project の published binding 変更で container の再作成が必要になる場合の warning は、この設定に関係なく常に表示されます。

設定 key の定義は [specification.md 5.11 節](specification.md#511-composepublished_ports)を参照してください。

### explicit mapping

特定の fixed TCP published port を常に同じ host endpoint へ割り当てる場合は `[[compose.published_ports.mappings]]` を使います。

```toml
[[compose.published_ports.mappings]]
service = "app"
target = 502
protocol = "tcp" # 省略時も tcp
host = 1502
host_ip = "127.0.0.1" # 省略時は Compose ports の host IP を継承
```

- mapping は `service + protocol + target` で対象の port entry を選び、その host endpoint を `host` / `host_ip` で明示します。
- mapping の endpoint が使用中でも、automatic relocation へは fallback せず error になります。
- mapping の追加・変更・削除を既存 project へ反映するには `decune rebuild` を実行してください。`host_ip` だけの変更も endpoint の変更として container を再作成します。
- 複数 mapping の endpoint を相互に入れ替える場合は、`decune down` で既存 binding を解放してから `decune rebuild` を実行してください。

field と layer 間 merge の定義は [specification.md 5.11 節](specification.md#511-composepublished_ports)を参照してください。

### Docker Compose v2.24.4 が必要になる場合

mapping または relocation で実際に host port か host IP が変わる場合、generated Compose override に Compose `!override` tag を使うため Docker Compose v2.24.4 以上が必要です。version を判定できない、または古い Compose では起動前に error になります。条件の一覧は [specification.md 2.2 節](specification.md#22-docker-compose-v2244-が必要になる条件)を参照してください。

## `decune ports` での確認

現在有効な host 側 port は `decune ports` で確認します。forwarding と published port が同じ一覧に表示されます。

- `TYPE` は `forwarded` / `published`、`SOURCE` は forwarding では `configured` / `auto`、published port では `appPort` / `compose` です。
- forwarding が別の host port へ fallback した場合や、mapping / relocation により requested endpoint と異なる endpoint を使っている場合は、`REQUESTED` に要求 endpoint が表示されます。relocation は `STATE` に `relocated` と表示され、host IP だけが異なる場合も含みます。
- host IP を省略した Compose published port は `*:<port>` と表示され、明示的な `0.0.0.0` と区別されます。
- workspace 横断は `decune ports --all`、機械可読な出力は `decune ports --json` を使います。JSON では `requested` / `planned` / `actual_bindings` / `relocated` で relocation の詳細を確認できます。

列と JSON schema の契約は [specification.md 3.6 節](specification.md#36-ports)を参照してください。

## トラブルシューティング

Compose published port の diagnostic code への対処は次のとおりです。発生条件の定義は [specification.md 13.1 節](specification.md#131-compose-published-port)を参照してください。

- `compose_published_port_collision`: requested host endpoint が使用中です。使用中の process、container、workspace を停止するか、Compose `ports` を変更するか、automatic relocation を有効化してください。
- `compose_published_port_automatic_relocation_failed`: requested host port 以降に利用可能な relocation candidate が見つかりません。使用中の host port を解放するか、Compose `ports` を変更してください。
- `compose_published_port_bind_race`: planning 後に別 process が planned endpoint を取得した可能性があります。再実行するか、該当 endpoint を使っている process を停止してください。
- `compose_published_port_unsupported`: startup failure が relocation 対象外の port entry に関係しています。UDP、range、`network_mode: host` などの Compose `ports` を確認してください。
- `compose_published_port_invalid`: invalid host IP、malformed port syntax など、単純な衝突ではない状態です。Compose `ports` の記述を確認してください。
- `compose_published_port_multi_replica_unsupported`: replica 数が 2 以上の service が fixed TCP published host port を持っています。container-only port、明示的に分けた複数 service、Compose port range、または replica 数 1 を使ってください。
- `compose_published_port_mapping_invalid`: mapping が active service の fixed TCP published port に一意に対応しません。mapping の `service` / `target` / `protocol` と Compose `ports` を確認してください。
- `compose_published_port_mapping_conflict`: mapping の desired endpoint が使用中または予約済みです。使用中の forwarding、process、container、workspace を停止するか、mapping の `host` / `host_ip` を変更してください。複数 mapping の endpoint を入れ替えている場合は、`decune down` の後に `decune rebuild` を実行してください。
