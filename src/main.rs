use clap::Parser;
use env_logger::Env;
use serde::{Deserialize, Serialize};
use std::net::TcpListener;
use std::time::Duration;
use std::{fs, mem, thread};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::{accept, Message, Utf8Bytes};

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + t * (b - a)
}

/// MPRIS2 status reporter as a WebSocket connection.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The starting player-reconnection time, in seconds. Player reconnection interval will be at least this number.
    #[arg(long, default_value_t = 1.0)]
    min_retry_time: f32,

    /// The maximum player-reconnection time, in seconds. Player reconnection interval will be at most this number.
    ///
    /// If this is smaller than min_retry_time, the two will be swapped and a warning will be produced.
    ///
    /// It will always take 16 retires to get to this point.
    #[arg(long, default_value_t = 4.0)]
    max_retry_time: f32,

    /// Silence the media player. If this is not to be used, use the RUST_LOG environment variables: https://docs.rs/env_logger/latest/env_logger/
    #[arg(short, long, default_value_t = false)]
    silent: bool,

    /// The address the websocket server will bind to.
    #[arg(long, default_value_t = String::from("127.0.0.1:32100"))]
    ip: String,

    /// The minimum status update interval, in seconds.
    ///
    /// The updating is lazy, it will only ask for the latest status when a websocket client does.
    ///
    /// This value controls the minimum amount of time needed to pass for the status to update.
    #[arg(short, long, default_value_t = 0.25)]
    interval: f32,

    /// The app name to look for. Leave blank to search for a player automatically.
    #[arg(short, long, default_value_t = String::from(""))]
    app_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum PlaybackState {
    /// A track is currently playing.
    Playing,
    /// A track is currently paused.
    Paused,
    /// There is no track currently playing.
    None,
}

impl From<mpris::PlaybackStatus> for PlaybackState {
    fn from(value: mpris::PlaybackStatus) -> Self {
        match value {
            mpris::PlaybackStatus::Playing => Self::Playing,
            mpris::PlaybackStatus::Paused => Self::Paused,
            mpris::PlaybackStatus::Stopped => Self::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtworkInfo {
    src: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusMetadata {
    playback_state: PlaybackState,
    title: String,
    artist: String,
    album: String,
    artwork: Vec<ArtworkInfo>,
    length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlayerStatus {
    metadata: StatusMetadata,
    position: u64,
}

thread_local! {
    static PLAYER_FINDER: mpris::PlayerFinder = mpris::PlayerFinder::new().expect("could not connect to D-Bus!");
}

fn find_player(
    times_tried: &mut u32,
    min_retry_time: f32,
    max_retry_time: f32,
    app_name: &str,
    current_player: Option<&mpris::Player>,
) -> Result<mpris::Player, Duration> {
    let result = if app_name.is_empty() {
        PLAYER_FINDER.with(|finder| finder.find_active())
    } else {
        PLAYER_FINDER.with(|finder| finder.find_by_name(app_name))
    };

    if let Ok(found) = result {
        if Some(found.bus_name()) == current_player.map(|v| v.bus_name()) {
            return Err(Duration::from_secs_f32(min_retry_time));
        }

        return Ok(found);
    }

    let times_normalized = (*times_tried).min(16) as f32 / 16.0;
    let try_again_time = lerp(min_retry_time, max_retry_time, times_normalized);

    *times_tried = times_tried.saturating_add(1);
    log::info!("Could not find a currently playing media player. Been trying for {} time(s). Trying again in {try_again_time} seconds.", times_tried);

    Err(Duration::from_secs_f32(try_again_time))
}

fn read_status(player: &mpris::Player) -> Option<PlayerStatus> {
    let (playback_status, metadata) = player
        .get_playback_status()
        .ok()
        .zip(player.get_metadata().ok())?;

    Some(PlayerStatus {
        metadata: StatusMetadata {
            playback_state: playback_status.into(),
            title: metadata.title().unwrap_or_default().to_string(),
            artist: metadata.artists().unwrap_or_default().join(", "),
            album: metadata.album_name().unwrap_or_default().to_string(),
            artwork: vec![ArtworkInfo {
                src: metadata.art_url().unwrap_or_default().to_string(),
            }],
            length: metadata.length_in_microseconds().unwrap_or_default(),
        },
        position: player.get_position_in_microseconds().unwrap_or_default(),
    })
}

fn handle_status_request(
    player: Option<&mpris::Player>,
    status_tx: &mut watch::Sender<Option<PlayerStatus>>,
) -> bool {
    let Some(player) = player else {
        return false;
    };

    if let Some(status) = read_status(player) {
        log::debug!(
            "Updated from player {}.",
            player.bus_name_player_name_part()
        );

        if status_tx.send(Some(status)).is_err() {
            log::info!("Player status isn't being requested anymore(all connections dropped)! Pausing updates.");

            return true;
        }
    } else {
        status_tx.send(None).unwrap();
        log::info!("Could not read player status...");

        if !player.is_running() {
            log::info!("Player is not running! Aborting updates.");

            return true;
        }
    }

    false
}

#[tokio::main]
async fn main() {
    let mut args = Args::parse();

    {
        if args.min_retry_time <= 0.0 {
            log::error!(
                "min_retry_time cannot be less than or equal to zero! Setting back to default."
            );
            args.min_retry_time = 1.0;
        }

        if args.max_retry_time <= 0.0 {
            log::error!(
                "max_retry_time cannot be less than or equal to zero! Setting back to default."
            );
            args.max_retry_time = 4.0;
        }

        if args.interval <= 0.0 {
            log::error!("interval cannot be less than or equal to zero! Setting back to default.");
            args.max_retry_time = 0.25;
        }

        if args.max_retry_time < args.min_retry_time {
            log::warn!("max_retry_time({}) is smaller than min_retry_time({})! Proceeding to swap the two.", args.max_retry_time, args.min_retry_time);

            mem::swap(&mut args.min_retry_time, &mut args.max_retry_time);
        }
    }

    {
        let mut env = Env::default();
        if !args.silent {
            env = env.default_filter_or("info");
        }
        env_logger::Builder::from_env(env).init();
    }

    let (status_tx, status_rx) = watch::channel::<Option<PlayerStatus>>(None);

    {
        let min_retry_time = args.min_retry_time;
        let max_retry_time = args.max_retry_time;
        let app_name = args.app_name;
        let update_interval = Duration::from_secs_f32(args.interval);

        thread::spawn(move || {
            let mut status_tx = status_tx;

            let mut player: Option<mpris::Player> = None;
            let mut times_tried = 0;

            loop {
                if handle_status_request(player.as_ref(), &mut status_tx) {
                    player = None;
                };

                match find_player(
                    &mut times_tried,
                    min_retry_time,
                    max_retry_time,
                    &app_name,
                    player.as_ref(),
                ) {
                    Ok(new_player) => {
                        log::info!(
                            "Found new player \"{} ({})\"!",
                            new_player.bus_name_player_name_part(),
                            new_player.bus_name()
                        );
                        player = Some(new_player);
                        times_tried = 0;
                    }
                    Err(duration) => {
                        if player.is_none() {
                            thread::sleep(duration);
                        }
                    }
                }

                thread::sleep(update_interval);
            }
        });
    }

    {
        let listener = TcpListener::bind(&args.ip).unwrap_or_else(|_| {
            panic!(
                "could not bind to ip {}! specify a free address with --ip",
                &args.ip
            )
        });

        log::info!("Bound to ip {}!", args.ip);

        loop {
            let Ok((stream, _)) = listener.accept() else {
                continue;
            };

            if let Ok(mut ws_stream) = accept(stream) {
                let status_rx = status_rx.clone();

                tokio::spawn(async move {
                    let mut current_artwork = None;

                    loop {
                        let Ok(msg) = ws_stream.read() else {
                            break;
                        };

                        if let Ok(req) = msg.into_text().as_ref().map(Utf8Bytes::as_str) {
                            if let Some(status) = status_rx.borrow().as_ref() {
                                if let Some(artwork_index) = req.strip_prefix("artwork/") {
                                    let Ok(index) = str::parse::<usize>(artwork_index) else {
                                        continue;
                                    };

                                    if let Some(artwork) = status.metadata.artwork.get(index) {
                                        if Some(artwork) != current_artwork.as_ref() {
                                            current_artwork = Some(artwork.clone());

                                            if let Some(path) =
                                                artwork.src.as_str().strip_prefix("file://")
                                            {
                                                let _ = ws_stream.send(Message::Binary(
                                                    fs::read(path).unwrap().into(),
                                                ));

                                                continue;
                                            } else {
                                                let _ = ws_stream.send(Message::Text(
                                                    artwork.src.clone().into(),
                                                ));
                                                continue;
                                            }
                                        } else {
                                            let _ = ws_stream.send(Message::Text("null".into()));
                                            continue;
                                        }
                                    }
                                } else {
                                    let _ = ws_stream.send(Message::Text(
                                        serde_json::to_string(&status).unwrap().into(),
                                    ));
                                    continue;
                                }
                            } else {
                                let _ = ws_stream.send(Message::Text("null".into()));
                                continue;
                            }
                        }
                    }
                });
            }
        }
    }
}
