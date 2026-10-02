use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use librespot::core::Session;
use librespot::playback::config::{Bitrate, PlayerConfig};
use librespot::playback::mixer::NoOpVolume;
use librespot::playback::player::{Player, PlayerEvent};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::sleep;

use crate::stream::channel_sink::{ChannelSink, SinkEvent};
use crate::stream::{StreamError, StreamEvent, StreamEventChannel};
use crate::track::Track;

/// How often a track load is attempted before giving up. Unavailable tracks
/// are never retried (see `load`).
const MAX_LOAD_ATTEMPTS: usize = 3;

pub struct Stream {
    player_config: PlayerConfig,
    session: Session,
}

impl Stream {
    pub fn new(session: Session) -> Self {
        let config = PlayerConfig {
            bitrate: Bitrate::Bitrate320,
            ..Default::default()
        };
        Stream {
            player_config: config,
            session,
        }
    }

    pub async fn stream(&self, track: &Track) -> Result<StreamEventChannel> {
        let metadata = track.metadata(&self.session).await;
        let (sink, mut channel) = ChannelSink::new(metadata);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let player = Player::new(
            self.player_config.clone(),
            self.session.clone(),
            Box::new(NoOpVolume),
            move || Box::new(sink),
        );

        let track = track.clone();
        tokio::spawn(async move {
            let mut attempt = 0usize;
            loop {
                attempt += 1;
                match Self::load(player.clone(), &track).await {
                    Ok(()) => {
                        tracing::info!("Track loaded successfully: {:?}", track.id);
                        break;
                    }
                    Err(err @ StreamError::Unavailable(_)) => {
                        // Unavailable tracks do not come back during a run,
                        // so there is nothing to retry.
                        tracing::warn!("{}: {}", err, track.id);
                        Self::send_event(&tx, StreamEvent::Error(err)).await;
                        return;
                    }
                    Err(err) if attempt >= MAX_LOAD_ATTEMPTS => {
                        tracing::error!("Failed to load track: {:?}, error: {:?}", track.id, err);
                        Self::send_event(&tx, StreamEvent::Error(err)).await;
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(
                            "Attempt {} of {} to load track {:?} failed: {} — retrying",
                            attempt,
                            MAX_LOAD_ATTEMPTS,
                            track.id,
                            err
                        );
                        Self::send_event(
                            &tx,
                            StreamEvent::Retry {
                                attempt,
                                max_attempts: MAX_LOAD_ATTEMPTS,
                            },
                        )
                        .await;
                        let delay = Duration::from_secs(10).saturating_mul(attempt as u32);
                        sleep(delay.min(Duration::from_secs(30))).await;
                    }
                }
            }

            tracing::info!("Streaming track: {:?}", track.id);

            while let Some(event) = channel.recv().await {
                match event {
                    SinkEvent::Write {
                        bytes,
                        total,
                        content,
                    } => {
                        Self::send_event(
                            &tx,
                            StreamEvent::Write {
                                bytes,
                                total,
                                content,
                            },
                        )
                        .await
                    }
                    SinkEvent::Finished => {
                        Self::send_event(&tx, StreamEvent::Finished).await;
                        break;
                    }
                }
            }
        });

        Ok(rx)
    }

    async fn load(player: Arc<Player>, track: &Track) -> Result<(), StreamError> {
        player.load(track.id.clone(), true, 0);

        tracing::info!("Loading track: {:?}", track.id);
        loop {
            match player.get_player_event_channel().recv().await {
                Some(PlayerEvent::Playing { .. })
                | Some(PlayerEvent::TrackChanged { .. })
                | Some(PlayerEvent::EndOfTrack { .. }) => {
                    tracing::info!("Player started playing track: {:?}", track.id);
                    break;
                }
                Some(PlayerEvent::Unavailable { .. }) => {
                    tracing::info!("Track is unavailable: {:?}", track.id);
                    return Err(StreamError::Unavailable(format!(
                        "Could not load track: {}",
                        track.id
                    )));
                }
                _ => {
                    // Ignore other events
                }
            }
        }

        tokio::spawn(async move {
            player.await_end_of_track().await;
            player.stop();
        });

        Ok(())
    }

    async fn send_event(tx: &UnboundedSender<StreamEvent>, event: StreamEvent) {
        tx.send(event).unwrap_or_else(|e| {
            tracing::error!("Failed to send event: {:?}", e);
        });
    }
}
