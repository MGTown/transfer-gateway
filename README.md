# Minecraft ip2region Transfer Gateway

这个项目做的是一个很明确的事情：玩家先连接到统一入口，网关读取连接 IP，通过 ip2region 判断国家、省份、城市和运营商，再用 Minecraft 的 Transfer Packet 把玩家交给合适的线路。

网关使用 Rust 编写，配置采用 TOML，ip2region 使用 `.xdb` 数据文件。当前内置协议适配覆盖 Java 版 1.20.5 到 26.3 Snapshot 10。

## 它是怎么工作的

```text
Minecraft 客户端
        │ Handshake + Login Start
        ▼
Rust Gateway ── ip2region ── 路由规则
        │ Login Success/Finished + Transfer Packet
        ▼
目标服务器（客户端重新建立连接）
```

Transfer Packet 不是流量隧道。它的作用是告诉客户端：“请改连这个地址。”所以线路配置中的 `host:port` 必须是玩家能够直接访问的域名或公网地址。

## 第一次启动

建议第一次直接启动程序，让它把配置文件和语言模板准备好：

```bash
cargo run --release -- config.toml
```

如果 `config.toml` 不存在，程序会创建以下文件，然后退出：

```text
config.toml
lang/zh-CN.toml
lang/en-US.toml
```

接下来编辑 `config.toml`，检查线路地址和端口，再启动网关。第二次启动时，缺失的 ip2region 数据库和安全名单会按配置自动下载。程序只会在文件不存在时创建模板，不会覆盖你已经改过的配置或语言文件。

## 配置热加载

网关启动后会监听配置文件所在目录。直接修改或原子替换 `config.toml` 后，网关会等待文件写入完成，重新解析并校验配置，然后一次性应用新的运行时状态。路由、MOTD、协议限制、安全名单、语言、日志文件、连接上限和登录超时都可以在运行中生效；修改 `server.bind` 时会重新绑定监听端口。

如果 TOML、线路地址、名单或其他依赖校验失败，网关会记录错误并继续使用上一份有效配置，不会因为一次保存失败而停止服务。正在处理的连接保留建立连接时的配置，新连接使用最新配置。

## ip2region 数据库

网关使用 ip2region 官方 Rust 绑定和 `.xdb` 文件。官方数据分别提供 IPv4 和 IPv6 数据库，配置时应使用与客户端地址类型对应的文件：

```toml
[ip2region]
v4_db = "./data/ip2region_v4.xdb"
v6_db = "./data/ip2region_v6.xdb"
auto_download = true
auto_update = true
update_interval_secs = 86400
download_base_url = "https://raw.githubusercontent.com/lionsoul2014/ip2region/master/data"
```

`auto_download` 只负责在启动或配置重载时补齐不存在的数据库；`auto_update` 开启后，网关会每隔 `update_interval_secs` 秒从 `download_base_url` 检查并下载两个 `.xdb` 文件。`auto_update` 默认关闭，示例配置显式开启。新文件会先校验 IP 版本，再与现有文件比较，最后原子替换并热应用；下载、校验或重建失败时继续使用当前数据库。`update_delay_secs`、`update_delay`、`delay_secs` 和 `delay` 也可作为 `update_interval_secs` 的兼容字段名，单位均为秒。默认示例使用 ip2region 官方 `master/data` 目录，该目录包含 IPv4 和 IPv6 的 xdb 文件；如果希望固定某个版本，可以把地址改成相应 release 的 `data` 目录并将 `auto_update` 设为 `false`。

只需要 IPv4 时，可以把 `v6_db` 设为空字符串。若使用内网镜像，把 `download_base_url` 改成镜像中同时存放两个 xdb 文件的目录。下载失败时网关会记录错误并继续启动，缺少数据库的地址类型会使用默认线路。

ip2region 的标准区域数据格式是：

```text
Country|Province|City|ISP|iso-alpha2-code
```

网关使用其中的国家代码、省份、城市和 ISP 字段进行匹配，其中 ISP 是运营商分流的主要依据。ip2region 的标准数据不包含 ASN，因此本项目不再读取 GeoLite/GeoIP 数据库，也不支持按 ASN 数字匹配；运营商线路请使用 `isp_contains` 匹配 ISP 文本。数据库的更新和自定义数据制作请参考 [ip2region 官方仓库](https://github.com/lionsoul2014/ip2region)。如果固定使用 release 数据目录，升级数据版本时请同时修改 `download_base_url` 并确认绑定版本兼容。

启动：

```bash
cargo run --release -- config.toml
```

## VPN、Tor 和 Spam IP 拦截

网关可以在登录前拦截 VPN/代理、Tor 出口节点，以及共享 Spam IP 黑名单中的地址。拦截发生在 Transfer Packet 之前；命中后不会把连接继续转发给线路服务器。

默认配置如下：

```toml
[security]
enabled = true
block_vpn = true
block_tor = true
block_spam = true
auto_download = true
auto_update = true
update_interval_secs = 86400

tor_exit_list = "./data/tor-exit-list.txt"
tor_exit_list_url = "https://check.torproject.org/torbulkexitlist"

spam_list = "./data/spam-ip-list.txt"
spam_list_url = "https://blackip.ustc.edu.cn/list.php?txt"

vpn_ipv4_list = "./data/vpn-ipv4.txt"
vpn_ipv4_list_url = "https://raw.githubusercontent.com/X4BNet/lists_vpn/main/output/vpn/ipv4.txt"
vpn_ipv6_list = "./data/vpn-ipv6.txt"
vpn_ipv6_list_url = "https://raw.githubusercontent.com/X4BNet/lists_vpn/main/output/vpn/ipv6.txt"

allowlist = []
vpn_isp_contains = []
```

Tor 名单使用 [Tor Project 提供的 bulk exit list](https://check.torproject.org/torbulkexitlist)；VPN 名单默认使用 [X4BNet 的 IPv4/IPv6 CIDR 列表](https://github.com/X4BNet/lists_vpn)；Spam 名单使用 [USTC 文本列表](https://blackip.ustc.edu.cn/list.php?txt)。名单文件即使对应的 `block_*` 关闭，也会被加载供路由规则中的 `vpn`、`spam`、`tor` 使用；`block_*` 只控制连接拦截。`auto_download` 只在名单不存在时下载，`auto_update` 开启后会按 `update_interval_secs` 秒周期性检查并热应用变化；下载内容会先校验再保存，失败时网关继续使用本地已有名单。`auto_update` 默认关闭，示例配置显式开启；这些间隔字段同样支持 `update_delay_secs`、`update_delay`、`delay_secs` 和 `delay` 别名。也可以在配置文件中直接替换名单，保存配置后触发重载。

`allowlist` 支持单个 IP 或 CIDR，例如：

```toml
allowlist = [
    "203.0.113.8",
    "2001:db8:1234::/48",
]
```

名单识别不是绝对准确的：VPN 服务商会更换出口地址，家庭宽带也可能被共享 Spam 名单误收录。`vpn_isp_contains` 只对 ip2region 的 ISP 字段做不区分大小写的包含匹配，建议只填确认过的关键词，例如：

```toml
vpn_isp_contains = ["某云厂商", "某代理服务"]
```

如果不希望启用某一项，可以单独关闭：

```toml
block_vpn = false
block_tor = false
block_spam = false
```

名单命中会同时写入终端和日志文件，并记录命中类型、IP、国家、省份、城市和 ISP。状态查询也会受到拦截影响：被拦截的客户端不会收到服务器列表响应。

客户端使用 Java 版 1.20.5 或更新版本连接网关，例如：

```text
gateway.example.com:25565
```

## 日志

默认情况下，日志会同时输出到终端，并追加保存到 `./logs/gateway.log`。日志目录不存在时会自动创建：

```toml
[logging]
file = "./logs/gateway.log"
```

每次成功发送 Transfer Packet 后，日志都会记录玩家名、来源 IP、客户端协议、命中的线路、目标节点，以及 ip2region 查询到的国家、省份、城市和运营商。核心信息类似这样：

```text
玩家 Steve 将传送到节点 cn-telecom（telecom.example.com:25565）。
```

如果不想写文件，把路径设为空即可：

```toml
[logging]
file = ""
```

当前日志文件采用追加写入，不负责按日期轮转；长期运行时可以交给 logrotate 或 systemd 做轮转和归档。

## MOTD：颜色和多行文本

MOTD 支持 Minecraft 常用的传统颜色和格式码。颜色码可以写成 `&a`，也可以直接写成 `§a`；格式码包括 `&l` 粗体、`&o` 斜体、`&n` 下划线和 `&r` 重置。还支持 `&#12ABEF` 与 `&x&1&2&A&B&E&F` 形式的十六进制颜色。

双引号 TOML 字符串中的 `\n` 会显示为换行，也可以使用 TOML 多行字符串：

```toml
motd = "&6ip2region Gateway\n&7根据 IP 自动选择线路"
```

```toml
motd = """&6第一行
&7第二行"""
```

网关会把这些内容转换成 Minecraft 的 Chat Component JSON，再放进本地回退状态响应中，所以颜色和换行由客户端正常渲染。正常情况下，服务器列表状态会优先透传当前路由子服务器的状态信息。

## 子服务器状态透传

客户端查询服务器列表时，网关会按照和玩家登录相同的 IP、国家、省份、城市及 ISP 规则选择线路，然后向选中的子服务器发起一次 Status 请求。子服务器返回的完整状态 JSON 会原样转发，因此以下信息都可以由子服务器自己决定：

- MOTD 和颜色、换行等 Chat Component 内容；
- 版本名称与协议号；
- 最大人数、在线人数和玩家样本；
- favicon、`enforcesSecureChat`、`previewsChat` 以及其他状态字段。

这意味着不同地区或不同运营商的玩家，看到的服务器列表信息也可能不同。如果目标子服务器暂时无法连接，网关会记录警告并使用 `server.motd`、`status_version_name`、`status_protocol` 等本地配置生成回退响应；玩家仍然可以看到网关，而不是直接得到空白状态。

## 目标服务器需要做什么

每个目标服务器都必须是 Java 版 1.20.5 或更新版本，并在 `server.properties` 中开启：

```properties
accepts-transfers=true
```

目标服务器还必须允许玩家直接访问配置中的 `host:port`。如果目标服务器前面还有 Paper、Velocity、BungeeCord 或其他代理，需要确认代理和后端都能处理 Transfer intent，也就是握手中的 next state `3`。

## 语言文件

语言设置位于 `config.toml`：

```toml
[language]
locale = "zh-CN"
directory = "./lang"
```

语言目录相对于当前配置文件所在目录解析。项目自带简体中文和英文模板；如果想添加日文，可以把 `lang/en-US.toml` 复制为 `lang/ja-JP.toml`，然后改成：

```toml
[language]
locale = "ja-JP"
directory = "./lang"
```

实际翻译内容放在语言文件的 `[messages]` 下。文件中的 `{name}` 是占位符，不能随意删掉；自定义文件没有提供的键会回退到英文。语言文件缺失时，程序会自动释放对应模板，但不会覆盖已有文件。

## 路由规则

线路定义在 `routing.lines` 下，`default_line` 是没有规则匹配时使用的线路：

```toml
[routing]
default_line = "global"

[routing.lines.global]
host = "global.example.com"
port = 25565
resolve_srv = false

[routing.lines.cn]
host = "cn.example.com"
port = 25565

[routing.lines.cn-telecom]
host = "telecom.example.com"
port = 25565
```

每个子线路都可以单独设置 `resolve_srv`，默认值是 `false`。设置为 `true` 后，
网关会为该目标查询 Minecraft 标准的 `_minecraft._tcp.<host>` SRV 记录。记录
存在时，网关会按最低 `priority` 和 `weight` 选择目标，并将解析出的主机和端口
用于状态透传以及 Transfer Packet；没有 SRV 记录时继续使用配置中的
`host:port`。如果线路只配置 `host`，缺失的 `port` 默认是 `25565`。`minecraft_srv`
和 `srv` 也可以作为 `resolve_srv` 的兼容字段名使用。线路组也有自己的
`resolve_srv`，并会将该设置应用于组内选出的节点。

例如，DNS 中存在以下记录时：

```text
_minecraft._tcp.play.example.com.  IN  SRV  0 10 25570 node.example.net.
```

线路可以只写：

```toml
[routing.lines.global]
host = "play.example.com"
resolve_srv = true
```

SRV 目标为 `.` 时表示服务明确不可用，该线路的登录会被拒绝，状态查询会回退
到网关本地状态。

规则按 `priority` 从高到低选择；优先级相同的时候，配置文件中靠前的规则优先。一个规则里的多个字段是 AND 关系，同一字段数组里的多个值是 OR 关系。状态查询没有玩家名，因此包含 `players` 或 `not_players` 的规则只在玩家登录时参与匹配。

支持的匹配条件：

- `countries`：国家 ISO 代码，例如 `CN`、`JP`。
- `provinces`：ip2region 返回的省或州名称，例如 `广东省`、`Tokyo`。为兼容旧配置，`subdivisions` 也可以作为字段名使用。
- `cities`：城市名称，例如 `深圳市`、`Tokyo`。
- `isp_contains`：对 ISP 文本做不区分大小写的包含匹配；例如 `电信`、`联通`、`移动`。旧配置中的 `operator_contains` 仍可识别。
- `players`：玩家名，不区分大小写精确匹配，例如 `herobrine`、`dinnerbone`。
- `not_countries`、`not_provinces`、`not_cities`、`not_isp_contains`、`not_players`：对应条件的排除列表；也支持 `not_subdivisions` 和 `not_operator_contains` 兼容别名。为了简化配置，`countries`、`provinces`、`cities`、`isp_contains`、`players` 中的单个值也可以用 `!` 开头表示“不等于/不包含”，例如 `countries = ["!CN"]`、`players = ["!herobrine"]`；同一个数组可以混合正向值和 `!` 排除值。
- `vpn`、`spam`、`tor`：布尔条件，按已加载的安全名单匹配；例如 `vpn = true` 只匹配 VPN，`spam = false` 排除 Spam IP。`not_vpn`、`not_spam`、`not_tor` 可表达否定判断，名单的 `allowlist` 优先级最高。

例如，下面的规则会把中国电信用户送到 `cn-telecom`：

```toml
[[routing.rules]]
priority = 100
line = "cn-telecom"
countries = ["CN"]
isp_contains = ["China Telecom", "中国电信", "电信"]
```

多个因素可以放在同一个规则中，以下规则只匹配日本且 ISP 包含移动关键词的两个玩家：

```toml
[[routing.rules]]
priority = 120
line = "mobile"
countries = ["JP"]
isp_contains = ["mobile", "移动"]
players = ["herobrine", "dinnerbone"]
```

### 线路组和负载均衡

`routing.group` 用于把多个目标放进同一个线路池。组的 `port` 默认是 `25565`，`hosts` 中的重复项会保留，因此可以用重复次数实现简单权重。规则通过 `group` 引用组，也可以把组名写在旧的 `line` 字段中：

```toml
[[routing.group]]
priority = 10
group_name = "cmcc-cluster"
mode = "round_robin"
port = 25565
resolve_srv = false
hosts = [
    "cmcc-node-01.mgtown.cn",
    "cmcc-node-02.mgtown.cn",
    "cmcc-node-01.mgtown.cn",
]

[[routing.rules]]
priority = 10
group = "cmcc-cluster"
isp_contains = ["移动", "CMCC"]
```

支持的 `mode` 有 `round_robin`、`random` 和 `ip_hash`；`loadblance`（示例中的拼写）和 `loadbalance` 也会按轮询处理。`ip_hash` 按来源 IP 选择固定节点，`round_robin` 和 `random` 使用组内的原子计数器。组的 `priority` 会在关联规则没有显式优先级（为 `0`）时作为该规则的优先级；一般建议直接在规则上写 `priority`。

不同数据版本或自定义数据库里的 ISP 命名可能不同，实际匹配值以日志输出为准。

## 当前协议范围

默认配置中的 `server.supported_protocols = []` 表示启用本项目内置的全部协议适配；如果填写非空数组，就只接受数组中列出的协议。

- protocol `766`：Java 1.20.5/1.20.6。
- protocol `767`：Java 1.21/1.21.1。
- protocol `768` 到 `776`：Java 1.21.2 到 26.2。
- snapshot protocol `1073741995` 到 `1073742156`：从 24w03a 到 26.3 Snapshot 10。

不同版本的登录包并不完全一样。网关会根据客户端协议版本决定是否写入旧版的 `strictErrorHandling` 字段；26.3 Snapshot 3 起，Login Finished 增加了 session id，同时 Transfer Packet 的包 ID 从 `0x0B` 变为 `0x0C`。

快照版本的协议仍可能继续变化。如果未来快照修改了登录或 Transfer Packet 的字段，需要同步更新协议适配表，并用对应版本的客户端实际测试。

## 必须先看：安全限制

目前网关为了保持实现简单，使用的是 Minecraft offline login。它会发送 Login Success/Finished，但没有实现 Mojang 的 RSA/AES 加密登录和 session 校验。

这意味着：如果把网关直接暴露在公网，攻击者可能伪造其他玩家的用户名。当前实现适合本地测试，或者只接受来自你自己的认证前置层的连接；不建议直接作为公网在线服的唯一入口。

生产环境至少需要在网关前增加账号认证或可信代理，也可以继续实现完整的 online-mode 登录流程（RSA 密钥交换、AES/CFB8 和 Mojang session `hasJoined` 校验）。ip2region 只用于选择线路，不能代替身份认证。

## 测试

```bash
cargo fmt --check
cargo test
cargo check
```
