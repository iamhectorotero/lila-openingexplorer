use crate::{
    api::Error,
    db::Database,
    model::{AnnoLichess, Mode, PersonalEntry, PersonalKeyBuilder, UserName},
    util::NevermindExt as _,
};
use clap::Clap;
use futures_util::StreamExt;
use rustc_hash::FxHashMap;
use shakmaty::{
    uci::Uci, variant::VariantPosition, zobrist::Zobrist, ByColor, CastlingMode, Color, Outcome,
    Position,
};
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;
use tokio::{
    sync::{
        mpsc::{self, error::SendTimeoutError},
        oneshot,
    },
    task::JoinHandle,
    time::timeout,
};

mod lila;

use lila::{Game, Lila};

#[derive(Clap)]
pub struct IndexerOpt {
    #[clap(long = "lila", default_value = "https://lichess.org")]
    lila: String,
}

#[derive(Clone)]
pub struct IndexerStub {
    tx: mpsc::Sender<IndexerMessage>,
}

impl IndexerStub {
    pub fn spawn(db: Arc<Database>, opt: IndexerOpt) -> (IndexerStub, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(2);
        (
            IndexerStub { tx },
            tokio::spawn(
                IndexerActor {
                    rx,
                    db,
                    lila: Lila::new(opt),
                }
                .run(),
            ),
        )
    }

    pub async fn index_player(&self, player: UserName) -> Result<IndexerStatus, Error> {
        let (req, res) = oneshot::channel();

        self.tx
            .send_timeout(
                IndexerMessage::IndexPlayer {
                    player,
                    callback: req,
                },
                Duration::from_secs(2),
            )
            .await
            .map_err(|err| match err {
                SendTimeoutError::Timeout(_) => Error::IndexerQueueFull,
                SendTimeoutError::Closed(_) => panic!("indexer died"),
            })?;

        match timeout(Duration::from_secs(7), res).await {
            Ok(res) => match res.expect("indexer alive") {
                Ok(()) => Ok(IndexerStatus::Completed),
                Err(err) => Err(err),
            },
            Err(_) => return Ok(IndexerStatus::Ongoing),
        }
    }
}

struct IndexerActor {
    rx: mpsc::Receiver<IndexerMessage>,
    db: Arc<Database>,
    lila: Lila,
}

impl IndexerActor {
    async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                IndexerMessage::IndexPlayer { callback, player } => {
                    callback
                        .send(self.index_player(player).await)
                        .nevermind("requester gone away");
                }
            }
        }
    }

    async fn index_player(&self, player: UserName) -> Result<(), Error> {
        log::info!("starting to index {}", player);

        let hash = ByColor::new_with(|color| {
            PersonalKeyBuilder::with_user_pov(&player.to_owned().into(), color)
        });

        let mut num_games = 0;
        let mut games = self.lila.user_games(&player).await?;
        while let Some(game) = games.next().await {
            let game = game?;
            num_games += 1;
            self.index_game(&player, &hash, game);
        }

        log::info!("indexed {} games for {}", num_games, player);
        Ok(())
    }

    fn index_game(&self, player: &UserName, hash: &ByColor<PersonalKeyBuilder>, game: Game) {
        if game.status.is_unindexable() || game.status.is_ongoing() {
            return;
        }

        let color = if Some(player) == game.user_name(Color::White) {
            Color::White
        } else if Some(player) == game.user_name(Color::Black) {
            Color::Black
        } else {
            return;
        };

        let year = AnnoLichess::from_time(game.last_move_at);
        let outcome = Outcome::from_winner(game.winner);
        let variant = game.variant.into();
        let pos = match game.initial_fen {
            Some(fen) => VariantPosition::from_setup(variant, &fen, CastlingMode::Chess960),
            None => Ok(VariantPosition::new(variant)),
        };

        let mut pos: Zobrist<_, u128> = match pos {
            Ok(pos) => Zobrist::new(pos),
            Err(err) => {
                log::warn!("not indexing {}: {}", game.id, err);
                return;
            }
        };

        // Build an intermediate table to remove loops (due to repetitions).
        let mut table: FxHashMap<u128, Uci> =
            FxHashMap::with_capacity_and_hasher(game.moves.len(), Default::default());

        for (ply, san) in game.moves.into_iter().enumerate() {
            let m = match san.to_move(&pos) {
                Ok(m) => m,
                Err(err) => {
                    log::error!("not indexing {}: {} ({} at ply {})", game.id, err, san, ply);
                    return;
                }
            };

            let uci = m.to_uci(CastlingMode::Chess960);
            table.insert(pos.zobrist_hash(), uci);

            pos.play_unchecked(&m);
        }

        let queryable = self.db.queryable();

        for (zobrist, uci) in table {
            let entry = PersonalEntry::new_single(
                uci.clone(),
                game.speed,
                Mode::from_rated(game.rated),
                game.id,
                outcome,
            );

            let mut buf = Cursor::new(Vec::new());
            entry.write(&mut buf).expect("serialize personal entry");

            queryable
                .db
                .put_cf(
                    queryable.cf_personal,
                    hash.by_color(color)
                        .with_zobrist(variant, zobrist)
                        .with_year(year),
                    buf.into_inner(),
                )
                .expect("merge cf personal");
        }
    }
}

enum IndexerMessage {
    IndexPlayer {
        player: UserName,
        callback: oneshot::Sender<Result<(), Error>>,
    },
}

pub enum IndexerStatus {
    Ongoing,
    Completed,
    Failed,
}
