use mpris::{PlaybackStatus, Player, PlayerFinder};
use std::net::TcpListener;
use std::thread::sleep;
use std::time::Duration;
use std::{mem, thread};

use clap::Parser;
use env_logger::Env;
use json::JsonValue;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::{accept, Message};

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

    /// The maximum amount of time, in seconds, to wait for the player status.
    ///
    /// If the WebSocket thread cannot get the player status in time (likely due to no player being present), it will send null instead.
    #[arg(short, long, default_value_t = 0.5)]
    time_out: f32,

    /// The app name to look for. Leave blank to search for a player automatically.
    #[arg(short, long, default_value_t = String::from(""))]
    app_name: String,
}

/// NOTE: Blocks the thread.
fn find_player(
    min_retry_time: f32,
    max_retry_time: f32,
    app_name: &str,
    finder: &PlayerFinder,
) -> Player {
    let mut times = 0;

    loop {
        let result = if app_name.is_empty() {
            finder.find_active()
        } else {
            finder.find_by_name(app_name)
        };

        if let Ok(found) = result {
            log::info!("Found a player!");
            return found;
        }

        let times_normalized = times.min(16) as f32 / 16.0;

        let try_again_time = lerp(min_retry_time, max_retry_time, times_normalized);

        log::info!("Could not find a currently playing Media Player. Been trying for {} times. Trying again in {try_again_time} seconds.", times + 1);

        sleep(Duration::from_secs_f32(try_again_time));

        times += 1;
    }
}

fn handle_status_request(
    player: &Player,
    message: &mut Option<Message>,
    status_rx: &mut mpsc::UnboundedReceiver<oneshot::Sender<Message>>,
) -> bool {
    let mut updated = false;

    while let Ok(reply) = status_rx.try_recv() {
        if !updated {
            *message = read_status_to_message(&player);
            log::debug!("Message updated.");

            if message.is_none() {
                log::debug!("Could not read status! Aborting message handling.");

                return player.is_running();
            }

            updated = true;
        }
        if reply.send(message.clone().unwrap()).is_err() {
            log::debug!("Message Receiver disconnected!");
        }
    }

    true
}

fn read_status_to_message(player: &Player) -> Option<Message> {
    if let Some((playback_status, metadata)) = player
        .get_playback_status()
        .ok()
        .zip(player.get_metadata().ok())
    {
        let mut json = JsonValue::new_object();

        {
            match playback_status {
                PlaybackStatus::Playing => json.insert("playbackState", "playing").unwrap(),
                PlaybackStatus::Paused => json.insert("playbackState", "paused").unwrap(),
                PlaybackStatus::Stopped => json.insert("playbackState", "none").unwrap(),
            }
        }

        {
            let mut m = JsonValue::new_object();

            if let Some(title) = metadata.title() {
                m.insert("title", title).unwrap();
            }
            if let Some(artists) = metadata.artists() {
                m.insert("artist", artists.join(", ")).unwrap();
            }
            if let Some(album) = metadata.album_name() {
                m.insert("album", album).unwrap();
            }
            if let Some(art_url) = metadata.art_url() {
                let mut art_array = JsonValue::new_array();

                let mut art_json = JsonValue::new_object();
                art_json.insert("src", art_url).unwrap();

                art_array.push(art_json).unwrap();

                m.insert("artwork", art_array).unwrap();
            }

            if let Some(length) = metadata.length_in_microseconds() {
                m.insert("length", length).unwrap();
            }

            json.insert("metadata", m).unwrap();
        }

        if let Some(position) = player.get_position_in_microseconds().ok() {
            json.insert("position", position).unwrap();
        }

        return Some(Message::Text(json.to_string()));
    }

    None
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

    let interval = Duration::from_secs_f32(args.interval);
    let min_retry_time = args.min_retry_time;
    let max_retry_time = args.max_retry_time;
    let app_name = args.app_name;
    let (status_tx, status_rx) = mpsc::unbounded_channel::<oneshot::Sender<Message>>();

    thread::spawn(move || {
        let finder = PlayerFinder::new().expect("could not connect to D-Bus!");

        let mut status_rx = status_rx;

        let mut message: Option<Message> = None;
        let mut player: Option<Player> = None;

        loop {
            while let Some(p) = &player {
                // TODO this is an amateur attempt

                let start = Instant::now();
                if !handle_status_request(p, &mut message, &mut status_rx) {
                    log::info!("Player is not running! Retrying connection.");

                    player = None;
                }
                let elapsed = Instant::now() - start;

                if interval > elapsed {
                    sleep(interval - elapsed);
                }
            }
            player = Some(find_player(
                min_retry_time,
                max_retry_time,
                &app_name,
                &finder,
            ));
        }
    });

    let server = TcpListener::bind(&args.ip).expect(
        format!(
            "could not bind to ip {}! specify a free address with --ip",
            &args.ip
        )
        .as_str(),
    );

    let time_out_duration = Duration::from_secs_f32(args.time_out);

    for stream in server.incoming() {
        if let Ok(stream) = stream {
            let status_tx = UnboundedSender::clone(&status_tx);

            tokio::spawn(async move {
                let mut websocket = accept(stream).unwrap();

                loop {
                    let msg = websocket.read();

                    if let Ok(msg) = msg {
                        if msg.is_binary() || msg.is_text() {
                            let (reply_tx, reply_rx) = oneshot::channel();

                            status_tx.send(reply_tx).unwrap();
                            log::debug!("Attempting to ask for a new Message.");

                            if let Ok(Ok(status)) = timeout(time_out_duration, reply_rx).await {
                                log::debug!("Message reply got.");
                                websocket.send(status).unwrap();
                            } else {
                                log::debug!("Message reply timed out.");
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }
            });
        }
    }
}
