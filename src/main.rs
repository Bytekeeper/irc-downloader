mod dcc;
mod server;

use crate::dcc::DccSend;
use crate::server::{ServerConfig, ServerConnection, ServerId};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{delete, get, post},
    Json, Router,
};
use dashmap::DashMap;
use futures_util::stream::{AbortHandle, Abortable, Aborted, FuturesUnordered};
use irc::client::prelude::*;
use irc::proto::FormattedStringExt;
use irc::proto::Response::*;
use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::sync::watch;
use tokio::time::{Duration, Instant};
use tokio_stream::{wrappers::WatchStream, StreamExt, StreamMap};
use tower_http::services::ServeDir;

lazy_static! {
    pub static ref REX_SEARCH: Regex = Regex::new(
        r"(?P<filename>[[:word:][:punct:]]+)\s+(?:.\s+)+(?i)/msg\s+(?P<nick>[^\s]+)\s+(?P<command>xdcc\s+send\s+#?\d+)"
    )
    .expect("Valid regex");
}

#[derive(Deserialize, Serialize)]
pub struct Configuration {
    servers: Vec<ServerConfig>,
    download_folder: PathBuf,
    port: u16,
}

pub type DownloadId = usize;

#[derive(Serialize, Clone, Debug)]
pub struct DownloadItem {
    pub id: DownloadId,
    pub server: ServerId,
    #[serde(rename = "fileName")]
    pub file_name: String,
    pub nick: String,
    pub status: DownloadStatus,
    #[serde(skip)]
    pub request_command: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct DownloadProgress {
    pub transferred: usize,
    pub file_size: Option<NonZeroUsize>,
    #[serde(skip)]
    pub abort_handle: AbortHandle,
}

#[derive(Serialize, Clone, Debug)]
pub enum DownloadStatus {
    Requested,
    SenderAbsent,
    Delayed(#[serde(skip)] Instant),
    Progress(DownloadProgress),
    Failed(String),
    Connecting,
}

#[derive(Deserialize)]
pub struct AbortDownloadRequest {
    pub id: DownloadId,
}

#[derive(Deserialize)]
pub struct DownloadRequest {
    pub server: ServerId,
    #[serde(rename = "fileName")]
    pub file_name: String,
    pub nick: String,
    pub command: String,
}

#[derive(Serialize, Default, Clone)]
pub struct SearchResult {
    pub server: ServerId,
    #[serde(rename = "fileName")]
    pub file_name: String,
    pub nick: String,
    pub command: String,
}

#[derive(Serialize, Clone)]
pub struct MessageDto {
    pub prefix: String,
    pub message: String,
}

#[derive(Serialize, Default, Clone)]
pub struct Search {
    results: Vec<SearchResult>,
}

pub struct App {
    search: Mutex<Search>,
    message_receiver: watch::Receiver<Message>,
    myip: Ipv4Addr,
    servers: DashMap<String, ServerConnection>,
    download_id: AtomicUsize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    simple_logger::init_with_level(log::Level::Info).unwrap();

    let mut configuration: Configuration =
        toml::from_str(std::str::from_utf8(&std::fs::read("config.toml")?)?)?;

    let (tx, message_receiver) = watch::channel(Message::new(None, "DIE", vec![])?);
    let myip: std::net::Ipv4Addr = reqwest::get("https://api.ipify.org/")
        .await?
        .text()
        .await?
        .parse()
        .expect("Could not retrieve own ip");
    let servers = DashMap::new();
    let mut streams = StreamMap::new();
    let mut connections: FuturesUnordered<_> = configuration
        .servers
        .drain(..)
        .map(ServerConnection::new)
        .collect();
    while let Some((server_connection, server_id, stream)) = connections.next().await.transpose()? {
        log::info!("Connected to {}", server_id);
        servers.insert(server_id.clone(), server_connection);
        streams.insert(server_id, stream);
    }
    let app_state = Arc::new(App {
        search: Default::default(),
        message_receiver,
        myip,
        servers,
        download_id: AtomicUsize::new(0),
    });
    tokio::spawn(web_server(app_state.clone()));

    while let Some((server_id, message)) = streams.next().await {
        let message = message?;
        tx.send(message.clone())?;
        match message.command {
            Command::PRIVMSG(channel, msg) => {
                if !channel.starts_with('#') {
                    eprintln!("GOT {:?}: {:?} - {:?}", message.prefix, channel, msg);
                }
                if let Some(Prefix::Nickname(nick, _, _)) = message.prefix {
                    if let Some((dcc_send, mut receiver)) = DccSend::from_str(&msg) {
                        let app_state = app_state.clone();
                        let download_folder = configuration.download_folder.clone();
                        tokio::spawn(async move {
                            let (download_id, download) = {
                                let server = &app_state
                                    .servers
                                    .get(&server_id)
                                    .expect("Server should be connected");
                                let client = &server.client;
                                let mut download = server.downloads.iter_mut()
                                    .find(|d| d.file_name == dcc_send.file_name)
                                    .expect("Associated download not found. TODO: This can happen if someone is 'trolling' us or the name is different.");
                                if matches!(download.status, DownloadStatus::Connecting) {
                                    log::warn!("Download in progress already");
                                    return;
                                }
                                download.status = DownloadStatus::Connecting;
                                (
                                    download.id,
                                    dcc_send.download(
                                        client.sender(),
                                        nick,
                                        app_state.myip,
                                        configuration.port,
                                        &download_folder,
                                    ),
                                )
                            };
                            let (abort_handle, abort_registration) = AbortHandle::new_pair();
                            let download = Abortable::new(download, abort_registration);
                            tokio::pin!(download);
                            loop {
                                tokio::select! {
                                    x = &mut download => {
                                        match x {
                                            Err(Aborted) => {
                                                eprintln!("Aborted");
                                            }
                                            Ok(Err(y)) => {
                                                eprintln!("Download error: {}", y);
                                                app_state
                                                    .servers
                                                    .get(&server_id)
                                                    .expect("Server should be connected")
                                                    .downloads
                                                    .get_mut(&download_id)
                                                    .expect("File name mismatch")
                                                    .status = DownloadStatus::Failed(format!("{}", y));
                                            }
                                            Ok(Ok(_)) => {
                                                eprintln!("Download completed");
                                                app_state
                                                    .servers
                                                    .get(&server_id)
                                                    .expect("Server should be connected")
                                                    .completed(&download_id);
                                            }
                                        }
                                        break;
                                    }
                                    _ = receiver.changed() => {
                                        // eprintln!("Progress : {:?}", receiver.borrow().transferred_bytes);
                                        let transferred = receiver.borrow().transferred_bytes;
                                        app_state
                                            .servers
                                            .get(&server_id)
                                            .expect("Server should be connected")
                                            .downloads
                                            .get_mut(&download_id)
                                            .expect("File name mismatch")
                                            .status = DownloadStatus::Progress(DownloadProgress {
                                            transferred,
                                            file_size: dcc_send
                                                .file_size
                                                .map(|fs| NonZeroUsize::new(fs).unwrap()),
                                            abort_handle: abort_handle.clone()
                                        });
                                    }
                                }
                            }
                        });
                    }
                }
            }
            Command::Response(RPL_WELCOME, _) => {
                eprintln!(
                    "Known servers: {:?}",
                    app_state
                        .servers
                        .iter()
                        .map(|m| m.key().clone())
                        .collect::<Vec<_>>()
                );
                eprintln!("Tried server: {}", server_id);
                app_state
                    .servers
                    .get(&server_id)
                    .expect("Server should be connected")
                    .join_channels()?;
            }
            Command::NOTICE(_, notice) => {
                let notice = notice.strip_formatting();
                if let Some(captures) = REX_SEARCH.captures(&notice) {
                    if let (Some(file_name), Some(nick), Some(command)) = (
                        captures.name("filename"),
                        captures.name("nick"),
                        captures.name("command"),
                    ) {
                        app_state.search.lock().unwrap().results.push(SearchResult {
                            server: server_id,
                            file_name: file_name.as_str().to_string(),
                            nick: nick.as_str().to_string(),
                            command: command.as_str().to_string(),
                        });
                    } else {
                        eprintln!("capture error {:?} - {:?}", message.prefix, notice);
                    }
                }
            }
            Command::Response(response, args) => {
                if response == Response::ERR_NOSUCHNICK {
                    app_state
                        .servers
                        .get_mut(&server_id)
                        .expect("Server should be connected")
                        .handle_sender_gone(&args[1]);
                }
            }
            // Not yet allowed to send messages to other users
            Command::Raw(code, _) if code == "531" => {
                let retry_at = app_state
                    .servers
                    .get_mut(&server_id)
                    .expect("Server should be connected")
                    .mark_downloads_delayed();
                let app_state = app_state.clone();
                tokio::spawn(async move {
                    tokio::time::sleep_until(retry_at).await;
                    log::info!("Retrying downloads of {}", server_id);
                    let server = &app_state
                        .servers
                        .get_mut(&server_id)
                        .expect("Server should be connected");
                    for download in server.downloads.iter() {
                        server
                            .client
                            .send_privmsg(&download.nick, &download.request_command)?;
                    }
                    Ok::<_, anyhow::Error>(())
                });
            }
            _ => eprintln!("{:?}", message),
        }
    }
    Ok(())
}

async fn web_server(app_state: Arc<App>) -> anyhow::Result<()> {
    let blub = Router::new()
        .route("/downloads", get(downloads))
        .route("/download", post(request_download))
        .route("/download/:id", delete(abort_download))
        .route("/search", get(search))
        .route("/events", get(sse_handler))
        .nest_service("/", ServeDir::new("frontend/dist"))
        .with_state(app_state);
    // .route("/downloads", get
    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(blub.into_make_service())
        .await
        .map_err(anyhow::Error::new)
}

async fn abort_download(
    State(state): State<Arc<App>>,
    Path(id): Path<DownloadId>,
) -> Result<(), StatusCode> {
    log::info!("Aborting download {}", id);
    for server in state.servers.iter_mut() {
        server.abort_download(&id);
    }
    Ok(())
}

async fn request_download(
    State(state): State<Arc<App>>,
    request: Json<DownloadRequest>,
) -> Result<(), StatusCode> {
    let DownloadRequest {
        server,
        file_name,
        nick,
        command,
    } = request.0;
    let server_connection = &mut state
        .servers
        .get_mut(&server)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let id = state.download_id.fetch_add(1, Ordering::SeqCst);

    server_connection.downloads.insert(
        id,
        DownloadItem {
            id,
            server,
            file_name,
            nick: nick.clone(),
            status: DownloadStatus::Requested,
            request_command: command.clone(),
        },
    );
    eprintln!("Requesting DL: {} {}", nick, command);
    server_connection
        .client
        .send_privmsg(nick, command)
        .map_err(|_err| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(())
}

async fn downloads(State(state): State<Arc<App>>) -> Json<Vec<DownloadItem>> {
    let servers = &state.servers;
    let downloads: Vec<_> = servers
        .iter()
        .flat_map(|s| s.downloads.iter().map(|r| r.clone()).collect::<Vec<_>>())
        .collect();
    Json(downloads)
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    query: String,
}

async fn search(
    State(state): State<Arc<App>>,
    Query(search_query): Query<SearchQuery>,
) -> Result<Json<Vec<SearchResult>>, StatusCode> {
    state.search.lock().unwrap().results.clear();
    for server in state.servers.iter_mut() {
        server
            .search(&search_query.query)
            .map_err(|_err| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    // TODO find a better way to wait for results
    tokio::time::sleep(Duration::from_millis(1000)).await;
    Ok(Json(state.search.lock().unwrap().results.clone()))
}

async fn sse_handler(
    State(app_state): State<Arc<App>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    // A `Stream` that repeats an event every second
    // let stream = stream::repeat_with(|| Event::default().event("update").data("hi!"))
    //     .map(Ok)
    //     .throttle(Duration::from_secs(1));
    let message_receiver = app_state.message_receiver.clone();
    let stream = WatchStream::from_changes(message_receiver)
        .map(|msg| {
            Event::default()
                .event("irc-message")
                .json_data(MessageDto {
                    prefix: msg
                        .prefix
                        .map(|p| format!("{:?}", p))
                        .unwrap_or_else(|| "".to_string()),
                    message: format!("{:?}", msg.command),
                })
                .expect("Could not serialize message")
        })
        .map(Ok);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod test {
    use super::*;
    use irc::proto::FormattedStringExt;

    #[test]
    fn search_result1() {
        let input = "\u{3}00,01\u{2}058\u{3}15\u{2})\u{3}10  10x\u{3}04\u{2} |\u{3}10\u{2} 7.5G\u{3}04\u{2} |\u{3}10\u{2} Something.Something-I-dont-really-know.2022.German.DTS.DL.720p.BluRay.x264-JJ.mkv\u{3}04\u{2} |\u{3}09\u{2} /MSG [AA]-DEMO|EU|S|DOESNOTEXIST XDCC SEND 90 \u{3}04\u{2}|\u{2}\u{3}00 Used: 11.53% 29/15 avg: 1.71TiB/s (113328s ago)\u{3}04 ".strip_formatting();

        let capture = REX_SEARCH.captures(&input).unwrap();
        itertools::assert_equal(
            capture.iter().skip(1).flatten().map(|i| i.as_str()),
            [
                "Something.Something-I-dont-really-know.2022.German.DTS.DL.720p.BluRay.x264-JJ.mkv",
                "[AA]-DEMO|EU|S|DOESNOTEXIST",
                "XDCC SEND 90",
            ],
        );
        assert!(capture.name("filename").is_some());
        assert!(capture.name("nick").is_some());
        assert!(capture.name("command").is_some());
    }

    #[test]
    fn search_result2() {
        let input = "\u{3}03(\u{3} 0x \u{3}03[\u{3}001.7G\u{3}03]\u{2} I-cant-believe-this.S01E07.1080p.HEVC.x265-noooaa.mkv \u{2}) (\u{3} /msg IDONOTCAREWHATYOURNAMEIS xdcc send #13384 \u{3}03) (\u{3} Used:\u{3}03 1/10 \u{3}Avg: \u{3}991034.62MB/s )".strip_formatting();
        eprintln!("{}", input);

        let capture = REX_SEARCH.captures(&input).unwrap();
        itertools::assert_equal(
            capture.iter().skip(1).flatten().map(|i| i.as_str()),
            [
                "I-cant-believe-this.S01E07.1080p.HEVC.x265-noooaa.mkv",
                "IDONOTCAREWHATYOURNAMEIS",
                "xdcc send #13384",
            ],
        );
        assert!(capture.name("filename").is_some());
        assert!(capture.name("nick").is_some());
        assert!(capture.name("command").is_some());
    }
}

trait IrcCase {
    type Other: ?Sized;

    fn eq_ignore_irc_case(&self, other: &Self::Other) -> bool;
}

impl IrcCase for [u8] {
    type Other = [u8];

    fn eq_ignore_irc_case(&self, other: &Self::Other) -> bool {
        self.len() == other.len()
            && std::iter::zip(self, other).all(|(a, b)| {
                a.eq_ignore_ascii_case(b)
                    || matches!(
                        (a, b),
                        (b'{', b'[')
                            | (b'[', b'{')
                            | (b'}', b']')
                            | (b']', b'}')
                            | (b'\\', b'|')
                            | (b'|', b'\\')
                    )
            })
    }
}

impl IrcCase for String {
    type Other = str;
    fn eq_ignore_irc_case(&self, other: &Self::Other) -> bool {
        self.as_bytes().eq_ignore_irc_case(other.as_bytes())
    }
}

impl IrcCase for str {
    type Other = str;
    fn eq_ignore_irc_case(&self, other: &Self::Other) -> bool {
        self.as_bytes().eq_ignore_irc_case(other.as_bytes())
    }
}
