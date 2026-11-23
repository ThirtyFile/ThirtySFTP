//! Throughput measurements for `thirtysftp`.
//!
//! ```text
//! thirtysftp-bench protocol [--size-mib N] [--latency-ms N] [--runs N]
//!                           [--chunks-kib 8,16,...] [--depths 1,4,...]
//! thirtysftp-bench serve --root DIR --authorized-key FILE.pub
//! thirtysftp-bench client --port N --user NAME --key FILE --dir PATH
//!                         [--size-mib N] [--runs N] [--chunk-kib N] [--depth N]
//! ```
//!
//! * `protocol` times uploads and downloads between this crate's client and
//!   server over an in-memory pipe, with a server that stores nothing, for a
//!   grid of request sizes and in-flight request counts. `--latency-ms` puts
//!   that one-way delay on the pipe, which shows what the in-flight count is
//!   for.
//! * `serve` runs this crate's server over SSH on the loopback interface and
//!   prints its port.
//! * `client` times an upload and a download of one file through this crate's
//!   client over SSH, against whatever server answers: this crate's `serve` or
//!   OpenSSH's `sshd`. It trusts any host key; it is a measuring tool.
//!
//! Sizes are mebibytes (MiB) and rates are MiB per second.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use data_encoding::BASE64;
use thirtysftp::client::{Config, File, Session};
use thirtysftp::server::{Filesystem, FsError, LocalFs, ServerConfig, serve};
use thirtysftp::wire::{Attributes, NameEntry, OpenFlags};
use thirtyssh::keys::{Algorithm, Credential, PrivateKeyReader};
use thirtyssh::transport::HostKey;
use thirtyssh::{client, server, session};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf, duplex, split};
use tokio::sync::mpsc;
use tokio::time::sleep_until;

const MIB: u64 = 1024 * 1024;

fn parse_args() -> (String, HashMap<String, String>) {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let mut options = HashMap::new();
    while let Some(key) = args.next() {
        let value = args.next().unwrap_or_default();
        options.insert(key.trim_start_matches("--").to_owned(), value);
    }
    (command, options)
}

fn number<T: std::str::FromStr>(options: &HashMap<String, String>, key: &str, default: T) -> T {
    options.get(key).map_or(default, |v| {
        v.parse()
            .unwrap_or_else(|_| panic!("bad value for --{key}"))
    })
}

fn required<'a>(options: &'a HashMap<String, String>, key: &str) -> &'a str {
    options
        .get(key)
        .unwrap_or_else(|| panic!("--{key} is required"))
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let (command, options) = parse_args();
    match command.as_str() {
        "protocol" => protocol(&options).await,
        "serve" => serve_ssh(&options).await,
        "client" => client_ssh(&options).await,
        _ => {
            eprintln!("usage: thirtysftp-bench protocol | serve | client (see the source)");
            std::process::exit(2);
        }
    }
}

fn rate(bytes: u64, took: Duration) -> f64 {
    bytes as f64 / MIB as f64 / took.as_secs_f64()
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

// ----------------------------------------------------------------------
// The protocol alone.

/// A file system that holds nothing: reads give zeros up to `size`, writes
/// are dropped. The protocol is the only cost left.
struct NullFs {
    size: u64,
}

macro_rules! unsupported {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {$(
        async fn $name(&self, $(_: $ty),*) -> Result<$ret, FsError> {
            Err(FsError::unsupported())
        }
    )*};
}

impl Filesystem for NullFs {
    type File = ();
    type Dir = ();

    async fn open(&self, _: &[u8], _: OpenFlags, _: &Attributes) -> Result<(), FsError> {
        Ok(())
    }

    async fn read(&self, _: &(), offset: u64, len: u32) -> Result<Option<Bytes>, FsError> {
        static ZEROS: [u8; 1 << 20] = [0; 1 << 20];
        if offset >= self.size {
            return Ok(None);
        }
        let n = u64::from(len)
            .min(self.size - offset)
            .min(ZEROS.len() as u64);
        Ok(Some(Bytes::from_static(&ZEROS[..n as usize])))
    }

    async fn write(&self, _: &(), _: u64, _: Bytes) -> Result<(), FsError> {
        Ok(())
    }

    async fn fstat(&self, _: &()) -> Result<Attributes, FsError> {
        Ok(Attributes {
            size: Some(self.size),
            ..Attributes::default()
        })
    }

    async fn fsetstat(&self, _: &(), _: &Attributes) -> Result<(), FsError> {
        Ok(())
    }

    async fn read_dir(&self, _: &()) -> Result<Option<Vec<NameEntry>>, FsError> {
        Ok(None)
    }

    unsupported! {
        stat(path: &[u8]) -> Attributes;
        lstat(path: &[u8]) -> Attributes;
        setstat(path: &[u8], attrs: &Attributes) -> ();
        open_dir(path: &[u8]) -> ();
        remove(path: &[u8]) -> ();
        mkdir(path: &[u8], attrs: &Attributes) -> ();
        rmdir(path: &[u8]) -> ();
        realpath(path: &[u8]) -> Bytes;
        rename(from: &[u8], to: &[u8]) -> ();
        read_link(path: &[u8]) -> Bytes;
        symlink(target: &[u8], link: &[u8]) -> ();
    }
}

/// Copies `from` to `to`, holding every chunk back for `delay`.
async fn delayed_copy(
    mut from: ReadHalf<DuplexStream>,
    mut to: WriteHalf<DuplexStream>,
    delay: Duration,
) {
    let (queue, mut due) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match from.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if queue
                        .send((Instant::now() + delay, buf[..n].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
    while let Some((at, data)) = due.recv().await {
        // The timer ticks in milliseconds; with no latency asked for, skip it.
        if !delay.is_zero() {
            sleep_until(at.into()).await;
        }
        if to.write_all(&data).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
    reader.abort();
}

/// A connected pair of streams with `latency` one way between them and no
/// limit on bandwidth.
fn pipe(latency: Duration) -> (DuplexStream, DuplexStream) {
    let (client, client_net) = duplex(8 << 20);
    let (server, server_net) = duplex(8 << 20);
    let (client_net_read, client_net_write) = split(client_net);
    let (server_net_read, server_net_write) = split(server_net);
    tokio::spawn(delayed_copy(client_net_read, server_net_write, latency));
    tokio::spawn(delayed_copy(server_net_read, client_net_write, latency));
    (client, server)
}

async fn timed_transfer(session: &Session, size: u64) -> io::Result<(Duration, Duration)> {
    let data = Bytes::from(vec![0u8; size as usize]);
    let file = File::create(session, "/bench").await?;
    let start = Instant::now();
    file.write_all_at(0, data).await?;
    let up = start.elapsed();
    file.close().await?;
    let file = File::open(session, "/bench").await?;
    let start = Instant::now();
    let got = file.read_range(0, size).await?;
    let down = start.elapsed();
    assert_eq!(got.len() as u64, size);
    file.close().await?;
    Ok((up, down))
}

async fn protocol_run(
    chunk: u32,
    depth: usize,
    size: u64,
    latency: Duration,
) -> io::Result<(f64, f64)> {
    let (client_end, server_end) = pipe(latency);
    let (sr, sw) = split(server_end);
    let fs = Arc::new(NullFs { size });
    let server_config = ServerConfig::default()
        .with_max_read_length(u32::MAX)
        .with_max_write_length(u32::MAX)
        .with_max_pending(depth.max(128));
    tokio::spawn(serve(sr, sw, fs, server_config));
    let (cr, cw) = split(client_end);
    let config = Config::default()
        .with_chunk_size(chunk)
        .with_pipeline_depth(depth)
        .with_max_in_flight(depth);
    let session = Session::connect(cr, cw, config)
        .await
        .map_err(io::Error::from)?;
    let (up, down) = timed_transfer(&session, size)
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok((rate(size, up), rate(size, down)))
}

async fn protocol(options: &HashMap<String, String>) -> io::Result<()> {
    let size = number(options, "size-mib", 64u64) * MIB;
    let latency = Duration::from_millis(number(options, "latency-ms", 0u64));
    let runs = number(options, "runs", 3usize);
    println!(
        "protocol only, {} MiB, {} ms each way, median of {runs}: upload / download MiB/s\n",
        size / MIB,
        latency.as_millis()
    );
    let list = |key: &str, default: &str| -> Vec<u64> {
        options
            .get(key)
            .map_or(default, String::as_str)
            .split(',')
            .map(|v| {
                v.parse()
                    .unwrap_or_else(|_| panic!("bad value for --{key}"))
            })
            .collect()
    };
    let chunks = list("chunks-kib", "8,16,32,64,128,255");
    let depths = list("depths", "1,4,16,64,128");
    print!("| chunk KiB \\ in flight |");
    for d in &depths {
        print!(" {d} |");
    }
    println!();
    print!("|---|");
    for _ in &depths {
        print!("---|");
    }
    println!();
    for kib in chunks {
        print!("| {kib} |");
        for &depth in &depths {
            let (mut ups, mut downs) = (Vec::new(), Vec::new());
            for _ in 0..runs {
                let (u, d) = protocol_run(kib as u32 * 1024, depth as usize, size, latency).await?;
                ups.push(u);
                downs.push(d);
            }
            print!(" {:.0} / {:.0} |", median(ups), median(downs));
        }
        println!();
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Over SSH.

#[derive(Clone)]
struct Connection {
    fs: Arc<LocalFs>,
    key: [u8; 32],
}

impl server::AuthPolicy for Connection {
    async fn public_key(&mut self, _user: &str, key: &[u8; 32]) -> io::Result<bool> {
        Ok(*key == self.key)
    }
}

impl session::Handler for Connection {
    async fn channel_open(
        &mut self,
        request: session::IncomingChannel,
        _: &session::Context,
    ) -> io::Result<()> {
        thirtysftp::ssh::accept_session(request, self.fs.clone(), ServerConfig::default()).await
    }
}

struct Factory(Connection);

impl server::Connections for Factory {
    type Handler = Connection;

    fn handler(&mut self, _: std::net::SocketAddr) -> Connection {
        self.0.clone()
    }
}

/// The 32 key bytes of an `ssh-ed25519` public key file.
fn ed25519_public(path: &str) -> io::Result<[u8; 32]> {
    const PREFIX: &[u8] = b"\0\0\0\x0bssh-ed25519\0\0\0\x20";
    let text = std::fs::read_to_string(path)?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "not an ssh-ed25519 public key");
    let blob = BASE64
        .decode(text.split_whitespace().nth(1).ok_or_else(bad)?.as_bytes())
        .map_err(|_| bad())?;
    blob.strip_prefix(PREFIX)
        .and_then(|k| k.try_into().ok())
        .ok_or_else(bad)
}

async fn serve_ssh(options: &HashMap<String, String>) -> io::Result<()> {
    let connection = Connection {
        fs: Arc::new(LocalFs::new(required(options, "root"))?),
        key: ed25519_public(required(options, "authorized-key"))?,
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    println!("PORT {}", listener.local_addr()?.port());
    server::listen_on(
        Arc::new(server::Config::new(HostKey::generate()?)),
        listener,
        Factory(connection),
    )?
    .await
}

struct AnyHost;

impl client::Handler for AnyHost {
    async fn verify_host(&mut self, _: &Credential, _: &Algorithm) -> io::Result<bool> {
        Ok(true)
    }
}

impl session::Handler for AnyHost {}

async fn client_ssh(options: &HashMap<String, String>) -> io::Result<()> {
    let port: u16 = number(options, "port", 0);
    let size = number(options, "size-mib", 128u64) * MIB;
    let runs = number(options, "runs", 3usize);
    let dir = required(options, "dir").to_owned();
    let mut config = Config::default();
    if let Some(kib) = options.get("chunk-kib") {
        config = config.with_chunk_size(kib.parse::<u32>().unwrap() * 1024);
    }
    if let Some(depth) = options.get("depth") {
        let depth: usize = depth.parse().unwrap();
        config = config.with_pipeline_depth(depth).with_max_in_flight(depth);
    }

    let mut connection = client::connect(
        Arc::new(client::Config::default()),
        ("127.0.0.1", port),
        AnyHost,
    )
    .await?;
    let identity = PrivateKeyReader::new().file(required(options, "key"))?;
    let accepted = connection
        .authenticate(
            required(options, "user"),
            client::Method::Key {
                key: &identity,
                algorithm: None,
            },
        )
        .await?
        .is_accepted();
    if !accepted {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "key refused",
        ));
    }
    let ssh = connection.start()?;
    let sftp = thirtysftp::ssh::connect(&ssh.handle(), config)
        .await
        .map_err(io::Error::from)?;

    let data = Bytes::from(
        (0..size)
            .map(|i| (i.wrapping_mul(0x9e37_79b9) >> 7) as u8)
            .collect::<Vec<u8>>(),
    );
    let path = format!("{dir}/bench.bin");
    let (mut ups, mut downs) = (Vec::new(), Vec::new());
    for _ in 0..runs {
        let file = File::create(&sftp, &path).await.map_err(io::Error::from)?;
        let start = Instant::now();
        file.write_all_at(0, data.clone())
            .await
            .map_err(io::Error::from)?;
        ups.push(rate(size, start.elapsed()));
        file.close().await.map_err(io::Error::from)?;
        let file = File::open(&sftp, &path).await.map_err(io::Error::from)?;
        let start = Instant::now();
        let got = file.read_range(0, size).await.map_err(io::Error::from)?;
        downs.push(rate(size, start.elapsed()));
        assert!(got[..] == data[..], "downloaded data differs");
        file.close().await.map_err(io::Error::from)?;
    }
    println!(
        "thirtysftp over ThirtySSH: upload {:.0} MiB/s, download {:.0} MiB/s (median of {runs}, {} MiB)",
        median(ups),
        median(downs),
        size / MIB
    );
    drop(sftp);
    ssh.disconnect().await?;
    Ok(())
}
