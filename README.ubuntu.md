# Ubuntu 24.04 X11 修改版

这是基于 [Oink](https://github.com/oinkvm/oink) 提交 `546f65b2135e7a1491d2d743f0a1e072a74df36f` 的独立本地修改版，不代表上游已提供 Ubuntu 支持。原作者与第三方归属保留，遵循 GPL-3.0 及仓库附加条款。

适配目标：用户当前 Ubuntu 24.04 x86_64、GNOME X11。Wayland 不在本轮范围内；程序需要实际 X11 会话及 XInput2、XTest、XFixes、RandR。音频使用用户会话里的 PulseAudio 协议，兼容提供该协议的 pipewire-pulse。

## 构建

需要 Rust 1.99、Node 24、npm、编译工具以及 GTK3/WebKitGTK4.1、AppIndicator、X11/Xext/XRender/XFixes/PangoCairo、ALSA/PulseAudio 开发包。系统包清单见 `packaging/linux/install-build-deps.sh`。

```sh
sh packaging/linux/build-deb.sh
```

构建产物为 `dist-linux/oink-ubuntu_0.3.3+ubuntu2_amd64.deb` 和同名 `.tar.gz` 便携包。安装包可用 `sudo apt install ./oink-ubuntu_0.3.3+ubuntu2_amd64.deb` 安装；便携包解压后，在 X11 桌面内运行 `./oink-ubuntu/launch-oink.sh`。便携包仍需要安装包 `Depends` 中的系统运行库。修改版包含独立 UI、后台和命令行程序，后台以桌面用户身份运行，不需要 root 键鼠服务，也不修改设备节点权限或系统默认音频设备。

## 连接与使用

两端仍使用 Oink 原有的配对、主机身份和加密传输。已与官方 Windows 0.3.3 客户端在实际局域网完成配对，并由用户确认跨屏鼠标、点击和键盘正常。Ubuntu 的桌面入口叫 “Oink Ubuntu (modified)”。安装包的命令行入口位于 `/usr/lib/oink-ubuntu/oink-agent`（例如 `status` 或 `stop`）。在 X11 登录会话内启动；通过设置启用 XDG 登录自启。关闭主窗口保留后台共享，通过托盘或程序内退出才停止后台，并释放按键/指针抓取。

原有数据目录按 XDG 规范保存。文件剪贴板与拖放在数据真正落地前不发布假的路径；收到的文件放在私有接收缓存中。拖放仍遵守 Hover 只读元数据、Drop 授权后读文件的边界。单个接收批次最多 64 GiB，目录深度和节点数也有边界；正在使用的批次持锁保护。长时间文件准备可能触及原生选择读取预算，应在准备完成后重试。

虚拟麦克风使用当前用户音频服务里的私有 null-sink/remap-source，由明确的安装/卸载操作管理；保留所有权收据，卸载不触碰其他模块和默认设备。X11 覆盖层需要桌面合成器，GNOME 默认具备。

## 验收状态

完整编译、前后端回归、隔离 Xvfb/Xorg 原生协议测试、隔离 PulseAudio 与 PipeWire 音频测试和桌面启动验证的最终结果，另见随附的验收记录。本文件不能代替实际通过结果。尚未验证的项目不会标成“完全适配”。

Linux 修改版的更新由本地安装包维护；上游只发布的 Windows/macOS 包不能用来覆盖此版本。

## 当前边界

- X11 键盘、相对鼠标及触摸板指针可共享、拦截和注入；真正的绝对坐标数位板、原生触摸和手势不在本轮范围。其他持有原始 XI2 监听权限的程序仍可能观察物理原始事件。
- RandR 调整保留模式、旋转和镜像并验证回滚；已有非单位变换或 panning 的 CRTC 明确拒绝改布局。
- 音频的默认输出范围可用；全局范围遇到多个普通输出时明确返回不支持。隔离测试使用真实协议服务和合成音频，未以物理扬声器/麦克风作验收。
- 文件身份在进程内按原始 inode 与创建时间追踪。进程重启后仅在已保存目录附近作有界、只读元数据恢复；无法确认原始对象时拒绝读取替代文件。文件系统必须提供创建时间。
- Windows 0.3.3 与本机 Ubuntu 的实体双机配对和跨屏键鼠已通过实机验收；文件、多格式剪贴板和系统音频的最新实机矩阵见 Release 附带的 `live-validation.json`；测试输出端点与可选麦克风范围会分别注明。Linux 专属目标映射需要源设备支持新增能力，旧官方客户端的基础键鼠共享不受影响，新增设置对旧 owner 明确禁用。

公开派生仓库：[mortal7520/oink-ubuntu](https://github.com/mortal7520/oink-ubuntu)。预览版运行包、源码及精确锁定的第三方 Rust 源码材料随 [Releases](https://github.com/mortal7520/oink-ubuntu/releases) 提供。

本版修复了与旧官方客户端之间标准剪贴板内容的读取兼容，以及 Linux 播放欠载后持续无声、固定轮询休眠导致供给稀疏的问题。双机实测细节见发行版附带的 `live-validation.json`。
