use std::{future::IntoFuture, time::Duration};

use rand::seq::IteratorRandom;

pub mod api;
pub mod bot;

#[derive(serde::Deserialize)]
pub struct Multiconfig {
    accounts: Vec<bot::Config>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config: Multiconfig =
        toml::from_str(&std::fs::read_to_string("config.toml").unwrap()).unwrap();

    let mut tasks = tokio::task::JoinSet::new();

    for account in config.accounts {
        tasks.spawn(
            tokio::task::spawn(async move {
                let mut bot = bot::Bot::new(account).await.unwrap();
                bot.work().await.unwrap();
            })
            .into_future(),
        );

        tokio::time::sleep(Duration::from_secs(
            (1..20).choose(&mut rand::thread_rng()).unwrap(),
        ))
        .await;
    }

    while let Some(err) = tasks.join_next().await {
        if let Err(err) = err {
            if err.is_panic() {
                err.into_panic();
            }
        }
    }
}
