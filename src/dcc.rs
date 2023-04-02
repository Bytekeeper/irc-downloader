use anyhow::bail;
use irc::client;
use lazy_static::lazy_static;
use regex::Regex;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch::{self, Receiver, Sender};
use tokio::time::{timeout, Duration};

lazy_static! {
    pub static ref REX_DCC_SEND : Regex = Regex::new("(?i)\u{1}DCC SEND (?P<filename>\\S+) (?P<address>\\d+) (?P<port>\\d+)(?: (?P<filesize>\\d+))?(?: (?P<id>\\d+))?.*\u{1}")
        .expect("Valid regex");
}

#[derive(Default)]
pub struct DownloadProgress {
    pub transferred_bytes: usize,
}

pub struct DccSend {
    pub file_name: String,
    pub address: SocketAddrV4,
    pub file_size: Option<usize>,
    pub id: Option<usize>,
    progress_sender: Sender<DownloadProgress>,
}

impl DccSend {
    pub fn from_str(message: &str) -> Option<(Self, Receiver<DownloadProgress>)> {
        if let Some(capture) = REX_DCC_SEND.captures(message) {
            if let (Some(file_name), Some(address), Some(port), file_size, id) = (
                capture.name("filename"),
                capture.name("address"),
                capture.name("port"),
                capture.name("filesize"),
                capture.name("id"),
            ) {
                let Ok(address) = address.as_str()
                    .parse::<u32>()
                    .map(Ipv4Addr::from) else { return None; };
                let Ok(port) = port.as_str().parse::<u16>() else { return None };
                let file_size = file_size
                    .map(|fs| fs.as_str().parse::<usize>())
                    .transpose()
                    .ok()
                    .flatten();
                let (progress_sender, receiver) = watch::channel(DownloadProgress::default());
                Some((
                    Self {
                        file_name: file_name.as_str().to_string(),
                        address: SocketAddrV4::new(address, port),
                        file_size,
                        id: id.and_then(|id| id.as_str().parse::<usize>().ok()),
                        progress_sender,
                    },
                    receiver,
                ))
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn is_passive(&self) -> bool {
        self.address.port() == 0
    }

    pub async fn download(
        &self,
        sender: client::Sender,
        nick: String,
        myip: Ipv4Addr,
        port: u16,
        download_folder: &Path,
    ) -> anyhow::Result<()> {
        log::info!("Starting to download {}", self.file_name);
        let mut stream = if self.is_passive() {
            log::info!("Initiating passive download");
            let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::from(0), port)).await?;
            let std::net::SocketAddr::V4(addr) = listener.local_addr()? else { bail!("Failed to retrieve port") };
            let port = addr.port();
            let msg = format!(
                "\u{1}DCC SEND {} {} {} {} {}\u{1}",
                self.file_name,
                u32::from(myip),
                port,
                self.file_size
                    .map(|file_size| file_size.to_string())
                    .unwrap_or_else(|| "".to_string()),
                self.id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "".to_string())
            );
            log::debug!("Sending to {}: {:?}", nick, msg);
            sender.send_privmsg(nick, msg)?;
            let (stream, other) = timeout(Duration::from_secs(30), listener.accept()).await??;
            let SocketAddr::V4(addr) = other else { unreachable!("Opened IPv4 port, but got some connection that is not IPv4?!") };
            if addr.ip() != self.address.ip() {
                bail!("IP mismatch on connected client");
            }
            stream
        } else {
            log::info!("Connecting to {:?} to download", self.address);
            timeout(Duration::from_secs(30), TcpStream::connect(self.address)).await??
        };
        log::debug!("Connected");
        std::fs::create_dir_all(download_folder)?;
        let path = download_folder.join(&self.file_name);
        log::debug!("Trying to create file: {}", path.display());
        let target_file = File::create(path).await?;
        let mut writer = BufWriter::new(target_file);
        stream
            .write_all(&self.file_size.unwrap().to_be_bytes())
            .await?;
        let mut transferred_bytes = 0;
        loop {
            stream.readable().await?;

            let mut buf = [0; 16384];
            match stream.try_read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    transferred_bytes += n;
                    writer.write_all(&buf[0..n]).await?;
                    self.progress_sender
                        .send(DownloadProgress { transferred_bytes })
                        .ok();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    continue;
                }
                Err(e) => bail!(e),
            }
        }
        writer.flush().await?;
        log::info!("File successfully transferred: {}", self.file_name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dcc_send_passive1() {
        let input =
            "\u{1}DCC SEND Well_this-could-be.something.mkv 1226420238 0 3498348389 22\u{1}";

        let capture = REX_DCC_SEND.captures(&input).unwrap();
        itertools::assert_equal(
            capture.iter().skip(1).flatten().map(|i| i.as_str()),
            [
                "Well_this-could-be.something.mkv",
                "1226420238",
                "0",
                "3498348389",
                "22",
            ],
        );

        let (dcc_send, _) = DccSend::from_str(&input).unwrap();
        assert_eq!(dcc_send.address.ip().octets(), [73, 25, 176, 14]);
        assert_eq!(dcc_send.address.port(), 0);
        assert_eq!(dcc_send.file_size, Some(3498348389));
        assert_eq!(dcc_send.id, Some(22));
    }

    #[test]
    fn dcc_send_passive2() {
        let input = "\u{1}DCC SEND Well_this-could-be.something.mkv 1226420238 0\u{1}";

        let capture = REX_DCC_SEND.captures(&input).unwrap();
        itertools::assert_equal(
            capture.iter().skip(1).flatten().map(|i| i.as_str()),
            ["Well_this-could-be.something.mkv", "1226420238", "0"],
        );
    }
}
