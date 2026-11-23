# ThirtySFTP

[![CI](https://github.com/thirtyfile/ThirtySFTP/actions/workflows/ci.yml/badge.svg)](https://github.com/thirtyfile/ThirtySFTP/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/thirtysftp.svg)](https://crates.io/crates/thirtysftp)
[![docs.rs](https://docs.rs/thirtysftp/badge.svg)](https://docs.rs/thirtysftp)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

[English](README.md) | **简体中文** | [繁體中文](README.zh-TW.md)

基于 Tokio 的 Rust 异步 SFTP 客户端与服务端库。这是独立实现的项目。
协议核心可以用在任何 `AsyncRead + AsyncWrite` 字节流上,另有可选的集成层,
把它接到 [ThirtySSH](https://github.com/thirtyfile/ThirtySSH) 的通道上。

> **状态:** 1.0 之前,不同次版本之间接口可能改变。要把文件提供给不完全信任的人之前,
> 请先读 [SECURITY.md](SECURITY.md)。

## 功能

- SFTP 第 3 版(OpenSSH 使用的版本),以及这些 OpenSSH 扩展:`posix-rename`、
  `statvfs`、`fstatvfs`、`hardlink`、`fsync`、`lsetstat`、`limits`、`expand-path`、
  `copy-data`、`home-directory`、`users-groups-by-id`
- **客户端:** 低层的请求接口(`Session`)与接近 `tokio::fs` 的接口(`File`、`Dir`、
  `client::fs`)。同时发出多个请求、按编号匹配任意顺序的回复、大文件并行读写并补读
  缺口,远程文件可当作 `AsyncRead`/`AsyncWrite`/`AsyncSeek` 使用
- **服务端:** 协议循环搭配由你实现的 `Filesystem` trait,并限制包大小、句柄数、
  每次请求的字节数、目录项数与同时请求数。内附 `LocalFs`(提供一个本地目录,
  并把客户端限制在其中)与 `MemoryFs`
- 与 OpenSSH 互通:已用它的 `sftp-server`、`sshd`、`sftp`、`scp` 测试
  ([docs/INTEROP.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/INTEROP.md))
- 没有 `unsafe`,收到对方的数据不会 panic,在本机回环上速度与 OpenSSH 自己的客户端相近
  ([docs/PERFORMANCE.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/PERFORMANCE.md))

不包含:SFTP 第 4 到 6 版(几乎没有部署)、SCP 协议、文件同步或续传调度。

## Cargo feature

| Feature | 默认 | 用途 |
|---|---|---|
| `client` | 是 | SFTP 客户端 |
| `server` | 是 | SFTP 服务端与 `LocalFs` |
| `thirtyssh` | 是 | 接到 ThirtySSH 通道(`thirtysftp::ssh`) |

最低支持的 Rust 版本为 1.89。

## 示例

客户端与服务端各有一个可编译的示例,见[英文 README](README.md#client)(代码与英文版相同,
会当作测试一起编译)。

## 性能

在本机回环上传输 128 MiB 的文件,本库的客户端经 SSH 连到 OpenSSH `sshd` 约为
440-490 MiB/s,同一台服务端上 OpenSSH 自己的 `sftp` 约为 420-480 MiB/s。默认值
(同时 64 个请求;每个请求 32 KiB,服务端公布上限时最多 128 KiB)是依
[docs/PERFORMANCE.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/PERFORMANCE.md)
的测量结果确定的,该文档也写了测量方法与数字的局限。基准测试放在独立的
`thirtysftp-bench` crate。

## 更多

- [docs/ARCHITECTURE.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/ARCHITECTURE.md):结构说明
- [SECURITY.md](SECURITY.md):威胁模型、各项上限与已知限制
- [CONTRIBUTING.md](CONTRIBUTING.md) 与 [CHANGELOG.md](CHANGELOG.md)

## 许可证

Apache-2.0,见 [LICENSE](LICENSE)。
