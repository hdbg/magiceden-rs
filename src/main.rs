use console_subscriber::ConsoleLayer;
use std::{convert::Infallible, future::IntoFuture, time::Duration};
use tracing_subscriber::{filter::EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use rand::seq::IteratorRandom;
use solana_sdk::account::AccountSharedData;
use tracing_subscriber::Registry;

pub mod api;
pub mod bot;

#[derive(serde::Deserialize)]
pub struct Multiconfig {
    accounts: Vec<bot::Config>,
}

async fn worker(account: bot::Config) -> Infallible {
    loop {
        let mut bot = match bot::Bot::new(account.clone()).await {
            Ok(bot) => bot,
            Err(err) => {
                println!("{:?}", err);
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        if let error @ Err(_) = bot.work().await {
            println!("{:?}", error);
            tokio::time::sleep(Duration::from_secs(10)).await;

            continue;
        }
    }
}

#[tokio::main]
async fn main() {
    // Create an EnvFilter layer
    let env_filter = EnvFilter::new("info,fast_socks5=off");
    let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stdout());
    // Create a formatting layer
    let formatting_layer = tracing_subscriber::fmt::layer().with_writer(non_blocking);

    // Combine the layers into a subscriber
    let console_sub = console_subscriber::spawn();
    let subscriber = Registry::default()
        .with(console_sub)
        .with(env_filter)
        .with(formatting_layer);

    // Set the subscriber as the global default
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set subscriber");
    let config: Multiconfig =
        toml::from_str(&std::fs::read_to_string("config.toml").unwrap()).unwrap();

    let mut tasks = tokio::task::JoinSet::new();

    for account in config.accounts {
        tasks.spawn(worker(account));
        tokio::task::yield_now().await;

        // tokio::time::sleep(Duration::from_secs(
        //     (1..20).choose(&mut rand::thread_rng()).unwrap(),
        // ))
        // .await;
    }

    tracing::info!(event = "spawn.done");

    while let Some(err) = tasks.join_next().await {
        match err {
            // finished
            _ => {} // toplevel error
                    // Ok(Err(_)) => {
                    //     println!("{:#?}", err);

                    //     // tokio::time::sleep(Duration::from_secs(10)).await;
                    //     // tracing::info!(event = "account.respawn", target = &account.pk[0..10]);
                    //     // tasks.spawn(worker(account));
                    // }
                    // panic error

                    // e @ Err(_) => {
                    //     e.unwrap();
                    // }
        }
    }

    ()
}
