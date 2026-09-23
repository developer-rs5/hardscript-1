use axum::{
    extract::Path,
    routing::{get, post},
    Json, Router,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

async fn root() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn hello(Path(name): Path<String>) -> Json<Value> {
    Json(json!({ "hello": name }))
}

async fn echo(Json(body): Json<Value>) -> Json<Value> {
    Json(json!({ "echo": body }))
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(root))
        .route("/hello/{name}", get(hello))
        .route("/echo", post(echo));

    let listener = TcpListener::bind("127.0.0.1:8080").await.expect("bind");
    println!("ready");
    axum::serve(listener, app).await.expect("serve");
}