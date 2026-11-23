# ThirtySFTP

[![CI](https://github.com/thirtyfile/ThirtySFTP/actions/workflows/ci.yml/badge.svg)](https://github.com/thirtyfile/ThirtySFTP/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/thirtysftp.svg)](https://crates.io/crates/thirtysftp)
[![docs.rs](https://docs.rs/thirtysftp/badge.svg)](https://docs.rs/thirtysftp)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

[English](README.md) | [简体中文](README.zh-CN.md) | **繁體中文**

以 Tokio 為基礎的 Rust 非同步 SFTP 用戶端與伺服器函式庫。這是獨立實作的專案。
協定核心可以用在任何 `AsyncRead + AsyncWrite` 位元組流上,另有選用的整合層,
把它接到 [ThirtySSH](https://github.com/thirtyfile/ThirtySSH) 的通道上。

> **狀態:** 1.0 之前,不同次版本之間介面可能改變。要把檔案提供給不完全信任的人之前,
> 請先讀 [SECURITY.md](SECURITY.md)。

## 功能

- SFTP 第 3 版(OpenSSH 使用的版本),以及這些 OpenSSH 擴充:`posix-rename`、
  `statvfs`、`fstatvfs`、`hardlink`、`fsync`、`lsetstat`、`limits`、`expand-path`、
  `copy-data`、`home-directory`、`users-groups-by-id`
- **用戶端:** 低階的請求介面(`Session`)與接近 `tokio::fs` 的介面(`File`、`Dir`、
  `client::fs`)。同時送出多個請求、依編號配對任意順序的回覆、大檔並行讀寫並補讀
  缺口,遠端檔案可當作 `AsyncRead`/`AsyncWrite`/`AsyncSeek` 使用
- **伺服器:** 協定迴圈搭配由你實作的 `Filesystem` trait,並限制封包大小、控制代碼數、
  每次請求的位元組數、目錄項目數與同時請求數。內附 `LocalFs`(提供一個本機目錄,
  並把用戶端限制在其中)與 `MemoryFs`
- 與 OpenSSH 互通:已用它的 `sftp-server`、`sshd`、`sftp`、`scp` 測試
  ([docs/INTEROP.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/INTEROP.md))
- 沒有 `unsafe`,收到對方的資料不會 panic,在本機迴路上速度與 OpenSSH 自己的用戶端相近
  ([docs/PERFORMANCE.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/PERFORMANCE.md))

不包含:SFTP 第 4 到 6 版(幾乎沒有部署)、SCP 協定、檔案同步或續傳排程。

## Cargo feature

| Feature | 預設 | 用途 |
|---|---|---|
| `client` | 是 | SFTP 用戶端 |
| `server` | 是 | SFTP 伺服器與 `LocalFs` |
| `thirtyssh` | 是 | 接到 ThirtySSH 通道(`thirtysftp::ssh`) |

最低支援的 Rust 版本為 1.89。

## 範例

用戶端與伺服器各有一個可編譯的範例,見[英文 README](README.md#client)(程式碼與英文版相同,
會當作測試一起編譯)。

## 效能

在本機迴路上傳輸 128 MiB 的檔案,本函式庫的用戶端經 SSH 連到 OpenSSH `sshd` 約為
440-490 MiB/s,同一台伺服器上 OpenSSH 自己的 `sftp` 約為 420-480 MiB/s。預設值
(同時 64 個請求;每個請求 32 KiB,伺服器公告上限時最多 128 KiB)是依
[docs/PERFORMANCE.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/PERFORMANCE.md)
的量測結果決定的,該文件也寫了量測方法與數字的限制。基準測試放在獨立的
`thirtysftp-bench` crate。

## 更多

- [docs/ARCHITECTURE.md](https://github.com/thirtyfile/ThirtySFTP/blob/main/docs/ARCHITECTURE.md):結構說明
- [SECURITY.md](SECURITY.md):威脅模型、各項上限與已知限制
- [CONTRIBUTING.md](CONTRIBUTING.md) 與 [CHANGELOG.md](CHANGELOG.md)

## 授權

Apache-2.0,見 [LICENSE](LICENSE)。
