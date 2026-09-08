# Zeron

在本地管理你的编码 agent（Claude Code、Codex、Cursor、Grok、Hermes、Pi），也可以打开多设备同步。

*[English](README.md) | 简体中文*

![Zeron 驱动一个 Claude Code 会话，侧边栏是实时的分支 diff](apps/landing/public/assets/app-screenshot.jpg)

每台设备各跑一个小引擎，会话就存在这台设备上。装完默认是纯本地模式，不用账号，也不用联网。

## 在本地安装运行（Linux）

```bash
curl -fsSL https://zeron.sh/install.sh | sh
zeron status
```

安装脚本会马上把守护进程拉起来，重启之后也会自己回来。不需要登录，也不需要配置同步。

日常命令：

```bash
zeron status      # 查看本地/同步模式和引擎状态
zeron update      # 更新到最新版本
zeron daemon start|stop|restart|status
```

## 可选：多设备同步

只有想打开账号下的同步工作区时才需要登录。登录会换掉引擎下次启动时用的 profile，所以改之前先停掉守护进程：

```bash
zeron daemon stop
zeron login
zeron daemon start
```

之后就可以在一台同步过的设备上起 agent，换另一台设备接着看、接着操作。一台常开的机器，比如 VPS，可以在你合上笔记本之后继续跑这些 agent。

登录不会上传、搬走或导入已有的本地会话。本地会话和它们的附件仍然留在本地 profile 下，切回纯本地模式时会照常出现：

```bash
zeron daemon stop
zeron logout
zeron daemon start
```

如果有引擎正占着数据目录，`zeron login` 和 `zeron logout` 会拒绝改动凭据。桌面应用同样遵守这条边界：profile 要等下次重启才切换。

macOS 上用桌面版发行包，或者从源码构建 `zeron`，再运行 `zeron daemon install` 装上 launchd 服务。

---

想参与开发，或者好奇它怎么跑起来的？[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/zeronsh/comet)，也可以看 [ARCHITECTURE.md](ARCHITECTURE.md)。

采用 [MIT License](LICENSE)。
