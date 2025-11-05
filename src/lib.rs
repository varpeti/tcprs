use anyhow::Result;
use futures::{SinkExt, StreamExt};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc::{self, Receiver, Sender},
};
use tokio_util::codec::{Framed, LinesCodec};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct TcpRsConfig {
    pub ip_port: String,
    pub to_player_buffer_size: usize,
    pub to_game_buffer_size: usize,
}

impl TcpRsConfig {
    pub fn new(ip_port: &str, to_player_buffer_size: usize, to_game_buffer_size: usize) -> Self {
        Self {
            ip_port: ip_port.to_owned(),
            to_player_buffer_size,
            to_game_buffer_size,
        }
    }
}

pub async fn run_tcp_server(config: TcpRsConfig) -> Result<Receiver<MsgFromPlayerToGame>> {
    let listener = TcpListener::bind(config.ip_port.clone()).await?;

    let (to_game_tx, to_game_rx) = mpsc::channel::<MsgFromPlayerToGame>(config.to_game_buffer_size);

    tokio::spawn(async move {
        while let Ok((socket, addr)) = listener.accept().await {
            info!("New TCP connection: `{}`", addr);
            let framed = Framed::new(socket, LinesCodec::new());
            let to_game_tx_clone = to_game_tx.clone();
            let config_clone = config.clone();
            tokio::spawn(
                async move { handle_connection(config_clone, framed, to_game_tx_clone).await },
            );
        }
    });
    Ok(to_game_rx)
}

async fn handle_connection(
    config: TcpRsConfig,
    framed: Framed<TcpStream, LinesCodec>,
    to_game_tx: Sender<MsgFromPlayerToGame>,
) {
    let (tcp_tx, tcp_rx) = framed.split();
    let (to_player_tx, to_player_rx) = mpsc::channel::<String>(config.to_player_buffer_size);

    tokio::spawn(async move { send_to_player(to_player_rx, tcp_tx).await });
    tokio::spawn(async move { send_to_game(to_player_tx, tcp_rx, to_game_tx).await });
}

async fn send_to_player(
    mut to_player_rx: Receiver<String>,
    mut tcp_tx: futures::stream::SplitSink<Framed<TcpStream, LinesCodec>, String>,
) {
    while let Some(msg) = to_player_rx.recv().await {
        if let Err(err) = tcp_tx.send(msg).await {
            error!("Unable to send Msg to Player! (send_to_player): `{}`", err);
        }
    }
    info!("send_to_player exited")
}

async fn send_to_game(
    to_player_tx: Sender<String>,
    mut tcp_rx: futures::stream::SplitStream<Framed<TcpStream, LinesCodec>>,
    mut to_game_tx: Sender<MsgFromPlayerToGame>,
) {
    let mut player = None;

    while let Some(Ok(msg)) = tcp_rx.next().await {
        // MsgFromPlayerToLobby
        if let Ok(msg) = serde_json::from_str::<MsgFromPlayerToLobby>(&msg) {
            match msg {
                MsgFromPlayerToLobby::Connect { name, uuid } => {
                    if let Some(player) = &player {
                        warn!("Player `{}` Already Connected!", name);
                        msg_to_player(player, MsgFromLobbyToPlayer::AlreadyConnected).await;
                        continue;
                    }
                    player = Some(TcpRsPlayer {
                        name,
                        uuid,
                        to_player_tx: to_player_tx.clone(),
                    });
                    if let Some(player) = &player {
                        msg_to_game(&mut to_game_tx, player, MsgFromLobbyToGame::Connect).await;
                    }
                    continue;
                }
                MsgFromPlayerToLobby::Disconnect => {
                    break;
                }
            }
        }

        // MsgFromPlayerToGame
        if let Some(player) = &player {
            msg_to_game_string(&mut to_game_tx, player, msg).await;
        } else {
            warn!(
                "Player is not connected yet, but tried to send message: `{}`",
                msg
            );
            msg_to_player(
                &TcpRsPlayer {
                    name: "Unkown".to_string(),
                    uuid: Uuid::nil(),
                    to_player_tx: to_player_tx.clone(),
                },
                MsgFromLobbyToPlayer::Unconnected,
            )
            .await;
        }
    }

    // Disconnect
    match player {
        Some(player) => {
            info!("Player `{}` disconnecting", player.name);
            msg_to_game(&mut to_game_tx, &player, MsgFromLobbyToGame::Disconnect).await;
        }
        None => {
            info!("Unkown Player disconnecting");
        }
    }
}

#[derive(Deserialize)]
enum MsgFromPlayerToLobby {
    Connect { name: String, uuid: Uuid },
    Disconnect,
}

#[derive(Debug, Serialize)]
enum MsgFromLobbyToPlayer {
    Unconnected,
    UnableToSendMsgToGame,
    AlreadyConnected,
}

#[derive(Debug, Serialize)]
enum MsgFromLobbyToGame {
    Connect,
    Disconnect,
}

#[derive(Debug, Clone)]
pub struct TcpRsPlayer {
    pub name: String,
    pub uuid: Uuid,
    pub to_player_tx: Sender<String>,
}

#[derive(Debug)]
pub struct MsgFromPlayerToGame {
    pub msg: String,
    pub player: TcpRsPlayer,
}

impl MsgFromPlayerToGame {
    fn new(msg: String, player: TcpRsPlayer) -> Self {
        Self { msg, player }
    }
}

pub async fn msg_to_player<M: Serialize + std::fmt::Debug>(player: &TcpRsPlayer, msg: M) {
    match serde_json::to_string(&msg) {
        Err(err) => {
            error!(
                "Unable to serialize Msg `{:?}` to Player `{:?}`: `{}`",
                msg, player.name, err
            )
        }
        Ok(msg) => {
            if let Err(err) = player.to_player_tx.send(msg).await {
                error!(
                    "Unable to send Msg to Player `{:?}`! (msg_to_player): `{}`",
                    player.name, err
                );
            }
        }
    }
}

pub async fn msg_to_game<M: Serialize + std::fmt::Debug>(
    to_game_tx: &mut Sender<MsgFromPlayerToGame>,
    player: &TcpRsPlayer,
    msg: M,
) {
    match serde_json::to_string(&msg) {
        Err(err) => {
            error!(
                "Unable to serialize Msg `{:?}` to Player `{:?}`: `{}`",
                msg, player.name, err
            )
        }
        Ok(msg) => msg_to_game_string(to_game_tx, player, msg).await,
    }
}

pub async fn msg_to_game_string(
    to_game_tx: &mut Sender<MsgFromPlayerToGame>,
    player: &TcpRsPlayer,
    msg: String,
) {
    let msg = MsgFromPlayerToGame::new(msg, player.clone());
    if let Err(err) = to_game_tx.send(msg).await {
        error!(
            "Unable to send Msg to Game by Player `{:?}`: `{}`",
            player.name, err
        );
        msg_to_player(player, MsgFromLobbyToPlayer::UnableToSendMsgToGame).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_log::test;

    #[derive(Deserialize)]
    enum MsgToGame {
        Connect,
        Disconnect,
        Echo { msg: String },
    }

    #[derive(Debug, Serialize)]
    enum MsgToPlayer {
        Connected,
        Echo { msg: String },
        UnknownMessage,
    }

    #[test(tokio::test)]
    async fn mock_game_test() {
        // Start Server
        let ip_port = "127.0.0.1:5942";
        let config = TcpRsConfig {
            ip_port: ip_port.to_string(),
            to_player_buffer_size: 1024,
            to_game_buffer_size: 1024,
        };
        let to_game_rx = run_tcp_server(config).await.expect("run_tcp_server");

        // Start Mock Game
        tokio::spawn(async move {
            mock_game(to_game_rx).await;
        });

        // Client TCP Connect
        let stream = TcpStream::connect(ip_port).await.expect("TCP Client");
        let framed = Framed::new(stream, LinesCodec::new());
        let (mut tcp_tx, mut tcp_rx) = framed.split();

        // Unconnected
        assert!(tcp_tx.send("asd").await.is_ok());
        if let Some(Ok(msg)) = tcp_rx.next().await {
            assert_eq!(msg, r#""Unconnected""#);
        }

        // Connect
        assert!(
            tcp_tx
                .send(
                    r#"{"Connect":{"name":"P001","uuid":"390f91c9-1603-4998-8005-024fe6e0f822"}}"#
                )
                .await
                .is_ok()
        );
        if let Some(Ok(msg)) = tcp_rx.next().await {
            assert_eq!(msg, r#""Connected""#);
        }

        // UnknownMessage
        assert!(tcp_tx.send("asd").await.is_ok());
        if let Some(Ok(msg)) = tcp_rx.next().await {
            assert_eq!(msg, r#""UnknownMessage""#);
        }

        // Echo
        assert!(tcp_tx.send(r#"{"Echo":{"msg":"Hello!"}}"#).await.is_ok());
        if let Some(Ok(msg)) = tcp_rx.next().await {
            assert_eq!(msg, r#"{"Echo":{"msg":"Hello!"}}"#);
        }

        // Disconnect
        assert!(tcp_tx.send(r#""Disconnect""#).await.is_ok());
        assert!(tcp_rx.next().await.is_none())
    }

    async fn mock_game(mut to_game_rx: Receiver<MsgFromPlayerToGame>) {
        while let Some(msg_to_game_from_player) = to_game_rx.recv().await {
            let player = msg_to_game_from_player.player;
            match serde_json::from_str::<MsgToGame>(&msg_to_game_from_player.msg) {
                Ok(msg_to_game) => match msg_to_game {
                    MsgToGame::Connect => {
                        info!("Player `{}` connected!", player.name);
                        msg_to_player(&player, MsgToPlayer::Connected).await;
                    }
                    MsgToGame::Disconnect => {
                        info!("Player `{}` disconnected!", player.name);
                        break;
                    }
                    MsgToGame::Echo { msg } => {
                        info!("Player `{}` sent: `{}`", player.name, msg);
                        msg_to_player(&player, MsgToPlayer::Echo { msg }).await;
                    }
                },
                Err(err) => {
                    warn!(
                        "Player `{}` sent unkown message `{}` Error: `{}`",
                        player.name, msg_to_game_from_player.msg, err
                    );
                    msg_to_player(&player, MsgToPlayer::UnknownMessage).await;
                }
            }
        }
    }
}
