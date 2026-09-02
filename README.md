# Minecraft ip2region Transfer Gateway

一个用 Rust 编写的 Minecraft Java 转发入口。

玩家连接网关后，网关会根据以下信息选择线路：

- Minecraft 握手中的访问 host；
- IP 的国家、省份、城市和 ISP；
- VPN、Tor 和 Spam IP 名单。

选好线路后，网关发送 Transfer Packet，让客户端重新连接目标服务器。
它不是流量代理，也不会转换协议版本。目标地址必须能被玩家直接访问。

当前内置协议适配覆盖 Java 版 1.20.5 到 26.3 Snapshot 10。

## 快速开始

需要 Rust 和 Cargo。第一次运行：

~~~bash
cargo run --release -- config.toml
~~~

如果配置文件不存在，程序会创建：

~~~text
config.toml
lang/zh-CN.toml
lang/en-US.toml
vhosts/alpha.toml
vhosts/beta.toml
~~~

编辑 config.toml 和 vhosts/*.toml 后再次启动。缺少的 ip2region 数据库和安全名单
会按配置自动下载。程序不会覆盖已有的配置、语言文件或 vhost 文件。

也可以直接复制 config.example.toml 使用。

## 基本配置

主配置文件包含以下部分：

- server：监听地址、连接数、登录超时、状态信息和允许的协议；
- vhosts 或 routing：访问 host 和线路规则；
- ip2region：IPv4/IPv6 数据库路径和更新设置；
- security：VPN、Tor、Spam 名单和拦截开关；
- language：语言文件；
- logging：日志文件。

常用的 server 配置：

~~~toml
[server]
bind = "0.0.0.0:25565"
max_connections = 4096
login_timeout_ms = 10000
supported_protocols = []
~~~

supported_protocols 为空时启用所有内置协议；填写协议号后，只接受列表中的版本。

## vhost 和路由

推荐使用 vhosts，把不同访问 host 的配置拆开：

~~~toml
[vhosts]
default = "alpha"
"alpha" = "./vhosts/alpha.toml"
"beta" = "./vhosts/beta.toml"
~~~

最简单的 vhost 文件只写一个目标：

~~~toml
host = "alpha-backend.example.com"
port = 25565
resolve_srv = false
~~~

host 是发给客户端的目标地址，不是 vhost 的匹配键。访问 alpha 时使用
vhosts/alpha.toml；未知 host 使用 default 指向的文件。

vhost 文件也可以写完整路由：

~~~toml
default_line = "global"

[lines.global]
host = "global.example.com"
port = 25565

[lines.cn]
host = "cn.example.com"
port = 25565

[[rules]]
priority = 100
line = "cn"
countries = ["CN"]
isp_contains = ["China Telecom", "中国电信", "电信"]
~~~

规则按 priority 从高到低匹配；相同优先级按文件中的顺序处理。一个规则中的不同
字段是 AND，同一字段列表中的值是 OR。

可用的匹配条件包括：

- hosts、not_hosts；
- countries、provinces、cities；
- isp_contains；
- players、not_players；
- vpn、spam、tor 及对应的否定条件。

支持使用 ! 表示排除，例如 countries = ["!CN"]。

线路组可以把多个目标放在一起，支持 round_robin、random 和 ip_hash：

~~~toml
[[group]]
group_name = "cmcc"
mode = "round_robin"
hosts = [
    "cmcc-01.example.com",
    "cmcc-02.example.com",
]
port = 25565
~~~

规则通过 group = "cmcc" 引用线路组。旧的单文件 [routing] 配置仍然支持。

## SRV

线路设置 resolve_srv = true 后，网关会查询：

~~~text
_minecraft._tcp.<host>
~~~

有记录时，按最低 priority 和 weight 选择目标；没有记录时继续使用配置中的
host 和 port。SRV 目标为 . 时表示线路明确不可用。

## 配置热加载

网关会递归监听配置文件所在目录下的 TOML 文件。修改 config.toml、vhosts 文件或
语言文件后会自动重载。

新配置会先解析和校验。校验失败时保留上一份有效配置，正在处理的连接也不受影响。
修改 server.bind 会重新绑定监听地址。

ip2region 数据库和安全名单可以按 update_interval_secs 定期检查并热更新。下载或
校验失败时继续使用本地已有文件。

## 安全名单

security.enabled 和 block_vpn、block_tor、block_spam 控制登录拦截。

allowlist 支持单个 IP 或 CIDR：

~~~toml
allowlist = [
    "203.0.113.8",
    "2001:db8:1234::/48",
]
~~~

名单即使没有开启对应的 block_*，只要文件已经存在，仍可用于路由规则中的 vpn、tor
和 spam 条件；自动下载和更新只维护对应 block_* 已开启的名单。vpn_isp_contains 可按
ip2region 的 ISP 文本匹配 VPN 关键词。

默认名单地址写在 config.example.toml 中，包括 [Tor Project](https://check.torproject.org/torbulkexitlist)、
[X4BNet](https://github.com/X4BNet/lists_vpn) 和 USTC Spam IP 列表。

## 日志和状态

日志默认写入终端和 ./logs/gateway.log：

~~~toml
[logging]
file = "./logs/gateway.log"
~~~

把 file 设为空字符串可关闭文件日志。日志不会自动轮转，长期运行时请交给
logrotate 或 systemd 处理。

服务器列表状态查询使用和登录相同的 vhost、IP 和路由规则。目标服务器可连接时，
网关转发目标服务器的状态；连接失败时返回本地配置中的 MOTD 和版本信息。

## 目标服务器要求

目标服务器必须：

1. 使用 Java 版 1.20.5 或更新版本；
2. 在 server.properties 中开启：

~~~properties
accepts-transfers=true
~~~

3. 允许玩家直接访问配置中的 host:port；
4. 如果前面还有 Paper、Velocity、BungeeCord 或其他代理，代理和后端都要支持
   Transfer intent，也就是握手中的 next state 3。

Transfer 不负责跨版本连接。例如 26.2 客户端重连时仍使用 protocol 776。
如果目标服务器版本较低，需要在目标前面使用 ViaVersion 等协议兼容层；只把目标
服务器降级不能完成转换。

## 协议范围

- protocol 766：Java 1.20.5/1.20.6；
- protocol 767：Java 1.21/1.21.1；
- protocol 768 到 776：Java 1.21.2 到 26.2；
- snapshot protocol 1073741995 到 1073742156：24w03a 到 26.3 Snapshot 10。

登录包会随版本变化：

- protocol 776（26.2）在 Login Finished 末尾增加 sessionId；
- 26.3 Snapshot 3 起，Transfer Packet 的包 ID 从 0x0B 变为 0x0C；
- 快照版本可能继续变化，升级时应使用对应客户端测试。

## 重要限制

网关使用 offline login，没有实现 Mojang 的 RSA/AES 加密登录和 session 校验。
直接暴露到公网时，攻击者可以伪造玩家名称。

生产环境请在网关前增加账号认证或可信代理。ip2region 只用于线路选择，不能代替
玩家身份认证。

## 测试

~~~bash
cargo fmt --check
cargo test
cargo check
~~~
