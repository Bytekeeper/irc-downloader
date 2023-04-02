use crate::{DownloadId, DownloadItem, DownloadStatus, IrcCase};
use dashmap::DashMap;
use irc::client::{data::Config, Client, ClientStream};
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, Instant};

pub type ServerId = String;

#[derive(Serialize, Deserialize)]
pub struct Channel {
    pub name: String,
    pub search: bool,
}

#[derive(Serialize, Deserialize)]
pub struct ServerConfig {
    pub config: Config,
    pub channels: Vec<Channel>,
}

pub struct ServerConnection {
    pub client: Client,
    pub channels: Vec<Channel>,
    pub downloads: DashMap<DownloadId, DownloadItem>,
    pub connected_at: Instant,
}

impl ServerConnection {
    pub async fn new(config: ServerConfig) -> anyhow::Result<(Self, ServerId, ClientStream)> {
        let server = config.config.server.clone().expect("Server URL missing");
        let mut client = Client::from_config(config.config).await?;
        client.identify()?;
        let stream = client.stream()?;
        Ok((
            Self {
                client,
                channels: config.channels,
                downloads: DashMap::new(),
                connected_at: Instant::now(),
            },
            server,
            stream,
        ))
    }

    pub fn join_channels(&self) -> anyhow::Result<()> {
        for channel in self.channels.iter() {
            self.client.send_join(&channel.name)?;
        }
        Ok(())
    }

    pub fn search(&self, query: &str) -> anyhow::Result<()> {
        for channel in self.channels.iter().filter(|c| c.search) {
            self.client
                .send_privmsg(&channel.name, format!("!s {}", query))?;
        }
        Ok(())
    }

    pub fn mark_downloads_delayed(&mut self) -> Instant {
        let until = self.connected_at + Duration::from_secs(70);
        for mut item in self.downloads.iter_mut() {
            if matches!(item.status, DownloadStatus::Requested) {
                item.status = DownloadStatus::Delayed(until);
            }
        }
        until
    }

    pub fn handle_sender_gone(&mut self, nick: &str) {
        for mut item in self.downloads.iter_mut() {
            if item.nick.eq_ignore_irc_case(nick) {
                item.status = DownloadStatus::SenderAbsent;
            }
        }
    }

    pub fn abort_download(&self, id: &DownloadId) {
        let download = self.downloads.remove(id);
        if let Some((
            _,
            DownloadItem {
                file_name,
                status: DownloadStatus::Progress(progress),
                ..
            },
        )) = download
        {
            log::info!("Aborted download of {}", file_name);
            progress.abort_handle.abort();
        }
    }

    pub fn completed(&self, id: &DownloadId) {
        self.downloads.remove(id);
    }
}
