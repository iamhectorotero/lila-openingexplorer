#![forbid(unsafe_code)]

pub mod api;
pub mod db;
pub mod importer;
pub mod indexer;
pub mod model;
pub mod opening;
pub mod util;

use std::{mem, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::{Extension, Path, Query},
    handler::{get, put},
    http::StatusCode,
    AddExtensionLayer, Json, Router,
};
use clap::Parser;
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};
use shakmaty::{
    fen::Fen,
    san::{San, SanPlus},
    uci::Uci,
    variant::{Variant, VariantPosition},
    zobrist::Zobrist,
    CastlingMode,
};
use tokio::sync::watch;

use crate::{
    api::{
        Error, ExplorerGame, ExplorerGameWithUci, ExplorerMove, ExplorerResponse, LichessQuery,
        Limits, MasterQuery, NdJson, PersonalQuery, PersonalQueryFilter,
    },
    db::{Database, QueryableDatabase},
    importer::{LichessGame, LichessImporter, MasterImporter},
    indexer::{IndexerOpt, IndexerStub},
    model::{GameId, KeyBuilder, KeyPrefix, MasterGame, MasterGameWithId, PreparedMove, UserId},
    opening::{Opening, Openings},
    util::DedupStreamExt as _,
};

#[derive(Parser)]
struct Opt {
    #[clap(long, default_value = "127.0.0.1:9000")]
    bind: SocketAddr,
    #[clap(long, default_value = "_db")]
    db: PathBuf,
    #[clap(long)]
    cors: bool,
    #[clap(flatten)]
    indexer: IndexerOpt,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::new()
            .filter("EXPLORER_LOG")
            .write_style("EXPLORER_LOG_STYLE"),
    )
    .format_timestamp(None)
    .format_module_path(false)
    .format_target(false)
    .init();

    let opt = Opt::parse();

    let openings: &'static Openings = Box::leak(Box::new(Openings::build_table()));
    let db = Arc::new(Database::open(opt.db).expect("db"));
    let (indexer, join_handles) = IndexerStub::spawn(Arc::clone(&db), opt.indexer);
    let master_importer = MasterImporter::new(Arc::clone(&db));
    let lichess_importer = LichessImporter::new(Arc::clone(&db));

    let app = Router::new()
        .route("/admin/db/:prop", get(db_property))
        .route("/admin/game/:prop", get(game_property))
        .route("/admin/personal/:prop", get(personal_property))
        .route("/admin/explorer.indexing", get(num_indexing))
        .route("/import/masters", put(master_import))
        .route("/import/lichess", put(lichess_import))
        .route("/masters/pgn/:id", get(master_pgn))
        .route("/masters", get(master))
        .route("/master", get(master)) // bc
        .route("/personal", get(personal)) // bc
        .route("/player", get(personal))
        .route("/lichess", get(lichess))
        .layer(AddExtensionLayer::new(openings))
        .layer(AddExtensionLayer::new(db))
        .layer(AddExtensionLayer::new(master_importer))
        .layer(AddExtensionLayer::new(lichess_importer))
        .layer(AddExtensionLayer::new(indexer));

    let app = if opt.cors {
        app.layer(tower_http::set_header::SetResponseHeaderLayer::<
            _,
            axum::body::Body,
        >::if_not_present(
            axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
            axum::http::HeaderValue::from_static("*"),
        ))
    } else {
        app
    };

    axum::Server::bind(&opt.bind)
        .serve(app.into_make_service())
        .await
        .expect("bind");

    for join_handle in join_handles {
        join_handle.await.expect("indexer");
    }
}

async fn db_property(
    Extension(db): Extension<Arc<Database>>,
    Path(prop): Path<String>,
) -> Result<String, StatusCode> {
    db.queryable()
        .db_property(&prop)
        .expect("get property")
        .ok_or(StatusCode::NOT_FOUND)
}

async fn game_property(
    Extension(db): Extension<Arc<Database>>,
    Path(prop): Path<String>,
) -> Result<String, StatusCode> {
    db.queryable()
        .game_property(&prop)
        .expect("get property")
        .ok_or(StatusCode::NOT_FOUND)
}

async fn personal_property(
    Extension(db): Extension<Arc<Database>>,
    Path(prop): Path<String>,
) -> Result<String, StatusCode> {
    db.queryable()
        .personal_property(&prop)
        .expect("get property")
        .ok_or(StatusCode::NOT_FOUND)
}

async fn num_indexing(Extension(indexer): Extension<IndexerStub>) -> String {
    indexer.num_indexing().await.to_string()
}

struct PersonalStreamState {
    indexing: Option<watch::Receiver<()>>,
    key: KeyPrefix,
    db: Arc<Database>,
    filter: PersonalQueryFilter,
    limits: Limits,
    pos: VariantPosition,
    opening: Option<&'static Opening>,
    first: bool,
    done: bool,
}

fn finalize_lichess_moves(
    moves: Vec<PreparedMove>,
    pos: &VariantPosition,
    queryable: &QueryableDatabase,
) -> Vec<ExplorerMove> {
    moves
        .into_iter()
        .map(|p| ExplorerMove {
            stats: p.stats,
            san: p.uci.to_move(pos).map_or(
                SanPlus {
                    san: San::Null,
                    suffix: None,
                },
                |m| SanPlus::from_move(pos.clone(), &m),
            ),
            uci: p.uci,
            average_rating: p.average_rating,
            average_opponent_rating: p.average_opponent_rating,
            game: p.game.and_then(|id| {
                queryable
                    .get_game_info(id)
                    .expect("get game")
                    .map(|info| ExplorerGame::from_lichess(id, info))
            }),
        })
        .collect()
}

fn finalize_lichess_games(
    games: Vec<(Uci, GameId)>,
    queryable: &QueryableDatabase,
) -> Vec<ExplorerGameWithUci> {
    games
        .into_iter()
        .flat_map(|(uci, id)| {
            queryable
                .get_game_info(id)
                .expect("get game")
                .map(|info| ExplorerGameWithUci {
                    uci,
                    row: ExplorerGame::from_lichess(id, info),
                })
        })
        .collect()
}

async fn personal(
    Extension(openings): Extension<&'static Openings>,
    Extension(db): Extension<Arc<Database>>,
    Extension(indexer): Extension<IndexerStub>,
    Query(query): Query<PersonalQuery>,
) -> Result<NdJson<impl Stream<Item = ExplorerResponse>>, Error> {
    let player = UserId::from(query.player);
    let indexing = indexer.index_player(&player).await;

    let variant = query.variant.into();

    let mut pos = Zobrist::new(match query.fen {
        Some(fen) => VariantPosition::from_setup(variant, &Fen::from(fen), CastlingMode::Chess960)?,
        None => VariantPosition::new(variant),
    });

    let opening = openings.classify_and_play(&mut pos, query.play)?;

    let key = KeyBuilder::personal(&player, query.color).with_zobrist(variant, pos.zobrist_hash());

    let state = PersonalStreamState {
        filter: query.filter,
        limits: query.limits,
        db,
        indexing,
        opening,
        key,
        pos: pos.into_inner(),
        first: true,
        done: false,
    };

    Ok(NdJson(futures_util::stream::unfold(
        state,
        |mut state| async move {
            if state.done {
                return None;
            }

            let first = mem::replace(&mut state.first, false);
            state.done = match state.indexing {
                Some(ref mut indexing) => {
                    tokio::select! {
                        _ = indexing.changed() => true,
                        _ = tokio::time::sleep(Duration::from_millis(if first { 0 } else { 1000 })) => false,
                    }
                }
                None => true,
            };

            let queryable = state.db.queryable();
            let mut filtered = queryable
                .get_personal(&state.key, state.filter.since, state.filter.until)
                .expect("get personal")
                .prepare(&state.filter);

            filtered.moves.truncate(state.limits.moves.unwrap_or(usize::MAX));
            filtered.recent_games.truncate(state.limits.recent_games);

            Some((
                ExplorerResponse {
                    total: filtered.total,
                    moves: finalize_lichess_moves(filtered.moves, &state.pos, &queryable),
                    recent_games: Some(finalize_lichess_games(filtered.recent_games, &queryable)),
                    top_games: None,
                    opening: state.opening,
                },
                state,
            ))
        },
    ).dedup_by_key(|res| res.total.total())))
}

async fn master_import(
    Json(body): Json<MasterGameWithId>,
    Extension(importer): Extension<MasterImporter>,
) -> Result<(), Error> {
    importer.import(body).await
}

#[serde_as]
#[derive(Deserialize)]
struct MasterGameId(#[serde_as(as = "DisplayFromStr")] GameId);

async fn master_pgn(
    Path(MasterGameId(id)): Path<MasterGameId>,
    Extension(db): Extension<Arc<Database>>,
) -> Result<MasterGame, StatusCode> {
    match db.queryable().get_master_game(id).expect("get master game") {
        Some(game) => Ok(game),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn master(
    Extension(openings): Extension<&'static Openings>,
    Extension(db): Extension<Arc<Database>>,
    Query(query): Query<MasterQuery>,
) -> Result<Json<ExplorerResponse>, Error> {
    let mut pos = Zobrist::new(match query.fen {
        Some(fen) => {
            VariantPosition::from_setup(Variant::Chess, &Fen::from(fen), CastlingMode::Chess960)?
        }
        None => VariantPosition::new(Variant::Chess),
    });

    let opening = openings.classify_and_play(&mut pos, query.play)?;
    let key = KeyBuilder::master().with_zobrist(Variant::Chess, pos.zobrist_hash());
    let queryable = db.queryable();
    let mut entry = queryable
        .get_master(key, query.since, query.until)
        .expect("get master")
        .prepare();

    entry.moves.truncate(query.limits.moves.unwrap_or(12));
    entry.top_games.truncate(query.limits.top_games);

    Ok(Json(ExplorerResponse {
        total: entry.total,
        moves: entry
            .moves
            .into_iter()
            .map(|p| ExplorerMove {
                san: p.uci.to_move(&pos).map_or(
                    SanPlus {
                        san: San::Null,
                        suffix: None,
                    },
                    |m| SanPlus::from_move(pos.clone(), &m),
                ),
                uci: p.uci,
                average_rating: p.average_rating,
                average_opponent_rating: p.average_opponent_rating,
                stats: p.stats,
                game: p.game.and_then(|id| {
                    queryable
                        .get_master_game(id)
                        .expect("get master game")
                        .map(|info| ExplorerGame::from_master(id, info))
                }),
            })
            .collect(),
        top_games: Some(
            entry
                .top_games
                .into_iter()
                .flat_map(|(uci, id)| {
                    queryable
                        .get_master_game(id)
                        .expect("get master game")
                        .map(|info| ExplorerGameWithUci {
                            uci,
                            row: ExplorerGame::from_master(id, info),
                        })
                })
                .collect(),
        ),
        opening,
        recent_games: None,
    }))
}

async fn lichess_import(
    Json(body): Json<Vec<LichessGame>>,
    Extension(importer): Extension<LichessImporter>,
) -> Result<(), Error> {
    for game in body {
        importer.import(game).await?;
    }
    Ok(())
}

async fn lichess(
    Extension(openings): Extension<&'static Openings>,
    Extension(db): Extension<Arc<Database>>,
    Query(query): Query<LichessQuery>,
) -> Result<Json<ExplorerResponse>, Error> {
    let variant = Variant::from(query.variant);
    let mut pos = Zobrist::new(match query.fen {
        Some(fen) => VariantPosition::from_setup(variant, &Fen::from(fen), CastlingMode::Chess960)?,
        None => VariantPosition::new(variant),
    });

    let opening = openings.classify_and_play(&mut pos, query.play)?;

    let key = KeyBuilder::lichess().with_zobrist(variant, pos.zobrist_hash());
    let queryable = db.queryable();
    let mut filtered = queryable
        .get_lichess(&key, query.filter.since, query.filter.until)
        .expect("get lichess")
        .prepare(&dbg!(query.filter));

    filtered.moves.truncate(query.limits.moves.unwrap_or(12));
    filtered.recent_games.truncate(query.limits.recent_games);
    filtered.top_games.truncate(query.limits.top_games);

    Ok(Json(ExplorerResponse {
        total: filtered.total,
        moves: finalize_lichess_moves(filtered.moves, pos.as_inner(), &queryable),
        recent_games: Some(finalize_lichess_games(filtered.recent_games, &queryable)),
        top_games: Some(finalize_lichess_games(filtered.top_games, &queryable)),
        opening,
    }))
}