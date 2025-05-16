use clap::Parser;
use env_logger::Env;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::net::TcpListener;
use std::time::Duration;
use std::{fs, mem, thread};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::{accept, Message};

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + t * (b - a)
}

/// MPRIS2 player status proxy as a WebSocket server.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The starting time between connection attempts, in seconds (floating point).
    #[arg(long, default_value_t = 1.0)]
    min_delay: f32,

    /// The maximum time between connection attempts, in seconds (floating point).
    ///
    /// If this is smaller than min_retry_time, the two will be swapped and a warning will be produced.
    ///
    /// It will always take 16 tries to get to this point.
    #[arg(long, default_value_t = 4.0)]
    max_delay: f32,

    /// The address the websocket server will bind to.
    #[arg(long, default_value_t = String::from("127.0.0.1:32100"))]
    ip: String,

    /// The minimum status update interval, in seconds (floating point).
    ///
    /// The updating is lazy, it will only ask for the latest status when a websocket client does.
    ///
    /// This value controls the minimum amount of time needed to pass for the status to update.
    #[arg(short, long, default_value_t = 0.25)]
    interval: f32,

    /// The app names to filter for with Regex. Only players with names that pass a pattern would be connected to.
    /// Leave blank to search for any players.
    #[arg(short, long, default_value = "")]
    app_names: Vec<String>,
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
    playback_state: PlaybackState,
    position: u64,
}

thread_local! {
    static PLAYER_FINDER: mpris::PlayerFinder = mpris::PlayerFinder::new().expect("could not connect to D-Bus!");
}

#[derive(Debug)]
enum PlayerFindResult {
    NewPlayer(mpris::Player),
    SamePlayer,
    NotFound(Duration),
}

fn find_player(
    times_tried: &mut u32,
    min_delay: f32,
    max_delay: f32,
    app_names: &[Regex],
    current_player: Option<&mpris::Player>,
) -> PlayerFindResult {
    let mut player = None;

    PLAYER_FINDER.with(|finder| {
        if app_names.is_empty() {
            player = finder.find_active().ok();
        } else {
            for regex in app_names {
                if let Some(new_player) = finder.iter_players().ok().and_then(|players| {
                    players
                        .flatten()
                        .find(|player| regex.is_match(player.bus_name_player_name_part()))
                }) {
                    player = Some(new_player);
                    break;
                }
            }
        }
    });

    if let Some(player) = player {
        if Some(player.bus_name_player_name_part())
            == current_player.map(|v| v.bus_name_player_name_part())
        {
            return PlayerFindResult::SamePlayer;
        }

        return PlayerFindResult::NewPlayer(player);
    }

    let times_normalized = (*times_tried).min(16) as f32 / 16.0;
    let try_again_time = lerp(min_delay, max_delay, times_normalized);

    *times_tried = times_tried.saturating_add(1);
    log::info!("Could not find a currently playing media player. Been trying for {times_tried} time(s). Trying again in {try_again_time} seconds.");

    PlayerFindResult::NotFound(Duration::from_secs_f32(try_again_time))
}

fn read_status(player: &mpris::Player) -> Option<PlayerStatus> {
    let (playback_status, metadata) = player
        .get_playback_status()
        .ok()
        .zip(player.get_metadata().ok())?;

    Some(PlayerStatus {
        metadata: StatusMetadata {
            title: metadata.title().unwrap_or_default().to_string(),
            artist: metadata.artists().unwrap_or_default().join(", "),
            album: metadata.album_name().unwrap_or_default().to_string(),
            artwork: vec![ArtworkInfo {
                src: metadata.art_url().unwrap_or_default().to_string(),
            }],
            length: metadata.length_in_microseconds().unwrap_or_default(),
        },
        playback_state: playback_status.into(),
        position: player.get_position_in_microseconds().unwrap_or_default(),
    })
}

fn handle_status_request(
    player: Option<&mpris::Player>,
    status_tx: &mut watch::Sender<Option<PlayerStatus>>,
) -> Result<(), ()> {
    let Some(player) = player else {
        return Ok(());
    };

    if let Some(status) = read_status(player) {
        log::debug!(
            "Updated from player \"{} ({})\".",
            player.bus_name_player_name_part(),
            player.bus_name()
        );

        if status_tx.send(Some(status)).is_err() {
            log::info!("Player status isn't being requested anymore! (All connections have dropped)\nPausing updates.");

            return Err(());
        }
    } else {
        status_tx.send(None).unwrap();
        log::info!("Could not read player status.");

        if !player.is_running() {
            log::info!("Player is not running! Detaching.");

            return Err(());
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let args = {
        let mut args = Args::parse();

        if args.min_delay <= 0.0 {
            log::error!(
                "min_retry_time cannot be less than or equal to zero! Setting back to default."
            );
            args.min_delay = 1.0;
        }

        if args.max_delay <= 0.0 {
            log::error!(
                "max_retry_time cannot be less than or equal to zero! Setting back to default."
            );
            args.max_delay = 4.0;
        }

        if args.interval <= 0.0 {
            log::error!("interval cannot be less than or equal to zero! Setting back to default.");
            args.max_delay = 0.25;
        }

        if args.max_delay < args.min_delay {
            log::warn!("max_retry_time({}) is smaller than min_retry_time({})! Proceeding to swap the two.", args.max_delay, args.min_delay);

            mem::swap(&mut args.min_delay, &mut args.max_delay);
        }

        args
    };

    let (status_tx, status_rx) = watch::channel::<Option<PlayerStatus>>(None);

    {
        let min_delay = args.min_delay;
        let max_delay = args.max_delay;
        let update_interval = Duration::from_secs_f32(args.interval);

        let app_name = {
            let names = args.app_names;

            if names.is_empty() {
                vec![]
            } else {
                names
                    .into_iter()
                    .flat_map(|name| match Regex::new(&name) {
                        Ok(v) => Some(v),
                        Err(err) => {
                            log::error!("Could not parse regex!\nError: {err}");

                            None
                        }
                    })
                    .collect()
            }
        };

        thread::spawn(move || {
            let mut status_tx = status_tx;

            let mut player: Option<mpris::Player> = None;
            let mut times_tried = 0;

            loop {
                if handle_status_request(player.as_ref(), &mut status_tx).is_err() {
                    player = None;
                    times_tried = 0;
                };

                match find_player(
                    &mut times_tried,
                    min_delay,
                    max_delay,
                    &app_name,
                    player.as_ref(),
                ) {
                    PlayerFindResult::NewPlayer(new_player) => {
                        log::info!(
                            "Found new player \"{} ({})\"!",
                            new_player.bus_name_player_name_part(),
                            new_player.bus_name()
                        );

                        player = Some(new_player);
                    }
                    PlayerFindResult::NotFound(duration) => {
                        thread::sleep(duration);
                        continue;
                    }
                    PlayerFindResult::SamePlayer => {
                        log::debug!("Found the same player! Skipping.");
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
                let mut status_rx = status_rx.clone();

                tokio::spawn(async move {
                    let mut current_artwork = None;

                    loop {
                        let Ok(msg) = ws_stream.read() else {
                            drop(ws_stream);
                            break;
                        };

                        let Ok(req) = msg.to_text() else {
                            continue;
                        };

                        let status = status_rx.borrow_and_update();
                        let Some(status) = status.as_ref() else {
                            continue;
                        };

                        let replied = (|| {
                            if let Some(artwork_index) = req.strip_prefix("artwork/") {
                                let Ok(index) = str::parse::<usize>(artwork_index) else {
                                    return false;
                                };

                                let Some(artwork) = status.metadata.artwork.get(index) else {
                                    return false;
                                };

                                if Some(artwork) == current_artwork.as_ref() {
                                    return false;
                                }

                                current_artwork = Some(artwork.clone());

                                if let Some(path) = artwork.src.as_str().strip_prefix("file://") {
                                    let _ = ws_stream
                                        .send(Message::Binary(fs::read(path).unwrap().into()));
                                } else {
                                    let _ =
                                        ws_stream.send(Message::Text(artwork.src.clone().into()));
                                }
                            } else {
                                let _ = ws_stream.send(Message::Text(
                                    serde_json::to_string(&status).unwrap().into(),
                                ));
                            }

                            true
                        })();

                        if replied {
                            log::debug!(
                                "Responded to WS!\n    Request: {req}\n    Status: {status:?}\n"
                            );
                        } else {
                            log::debug!(
                                "Didn't respond to WS!\n    Request: {req}\n    Status: {status:?}\n"
                            );
                            let _ = ws_stream.send(Message::Text("null".into()));
                        }
                    }
                });
            }
        }
    }
}
