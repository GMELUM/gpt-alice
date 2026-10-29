use std::{env, net::SocketAddr};

use axum::{
    Router,
    response::Html,
    routing::{get, post},
};

#[path = "../training.rs"]
mod training;

const DEFAULT_ADDRESS: &str = "127.0.0.1:8170";

async fn index() -> Html<&'static str> {
    Html(include_str!("../../web/training.html"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address: SocketAddr = env::var("GPT_ALICE_RECORDER_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDRESS.to_owned())
        .parse()?;
    if !address.ip().is_loopback() {
        return Err("the training recorder must bind to localhost only".into());
    }

    let listener = tokio::net::TcpListener::bind(address).await?;
    let app = Router::new()
        .route("/", get(index))
        .route("/api/training/recordings", post(training::save_recording));

    println!("GPT Alice voice recorder: http://{address}/");
    println!(
        "Recordings: {}",
        env::var("GPT_ALICE_TRAINING_DIR").unwrap_or_else(|_| "training/corpus/recorded".into())
    );
    axum::serve(listener, app).await?;
    Ok(())
}
