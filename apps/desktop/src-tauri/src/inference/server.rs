use actix_cors::Cors;
use actix_web::dev::ServerHandle;
use actix_web::web::{Bytes, Json};

use actix_web::{get, post, App, HttpResponse, HttpServer, Responder};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;

use std::sync::{
  atomic::{AtomicBool, Ordering},
  Arc,
};

use crate::abort_stream::AbortStream;
use crate::inference::thread::{
  start_inference, CompletionRequest, InferenceThreadRequest,
};
use crate::model_pool;

#[derive(Default)]
pub struct State {
  server: Arc<Mutex<Option<ServerHandle>>>,
  running: AtomicBool,
}

#[get("/")]
async fn ping() -> impl Responder {
  HttpResponse::Ok().body("pong")
}

#[derive(Serialize)]
struct ModelInfo {
  id: String,
}

#[post("/model")]
async fn post_model() -> impl Responder {
  HttpResponse::Ok().json(ModelInfo {
    id: String::from("local.ai"),
  })
}

#[post("/completions")]
async fn post_completions(payload: Json<CompletionRequest>) -> impl Responder {
  println!("Received completion request: {:?}", payload.0);

  if model_pool::is_empty() {
    println!("No model loaded or available in the pool.");
    return HttpResponse::ServiceUnavailable().finish();
  }

  let (token_sender, receiver) = flume::unbounded::<Bytes>();

  let model_guard = match model_pool::get_model() {
    Some(guard) => guard,
    None => {
      println!("No model available.");
      return HttpResponse::ServiceUnavailable().finish();
    }
  };

  let abort_flag = Arc::new(RwLock::new(false));

  let inference_thread = match start_inference(InferenceThreadRequest {
    model_guard: Arc::clone(&model_guard),
    abort_flag: Arc::clone(&abort_flag),
    token_sender,
    completion_request: payload.0,
  }) {
    Some(thread) => thread,
    None => {
      println!("Failed to spawn inference thread.");
      return HttpResponse::InternalServerError().finish();
    }
  };

  let stream =
    AbortStream::new(receiver, Arc::clone(&abort_flag), inference_thread);

  HttpResponse::Ok()
    .append_header(("Content-Type", "text/event-stream"))
    .append_header(("Cache-Control", "no-cache"))
    // .keep_alive()
    .streaming(stream)
}

#[tauri::command]
pub async fn start_server<'a>(
  state: tauri::State<'a, State>,
  port: u16,
) -> Result<(), String> {
  if state.running.load(Ordering::SeqCst) {
    return Err("Server is already running.".to_string());
  }

  state.running.store(true, Ordering::SeqCst);

  let server_clone = Arc::clone(&state.server);

  tauri::async_runtime::spawn(async move {
    let server = HttpServer::new(|| {
      let cors = Cors::permissive();
      App::new()
        .wrap(cors)
        .service(ping)
        .service(post_model)
        .service(post_completions)
    })
    .bind(("127.0.0.1", port))
    .unwrap()
    // .disable_signals()
    .run();
    *server_clone.lock() = Some(server.handle());
    println!("Server started on port {port}");
    server.await.unwrap();
  });

  Ok(())
}

#[tauri::command]
pub async fn stop_server<'a>(
  state: tauri::State<'a, State>,
) -> Result<(), String> {
  println!("Stopping server on port");

  if !state.running.load(Ordering::SeqCst) {
    return Err("Server is not running.".to_string());
  }

  let server_stop = {
    let mut server_opt = state.server.lock();
    if let Some(server) = server_opt.take() {
      state.running.store(false, Ordering::SeqCst);
      Some(server.stop(true))
    } else {
      None
    }
  };

  if let Some(server_stop) = server_stop {
    server_stop.await;
  } else {
    return Err("Server is not running.".to_string());
  }

  Ok(())
}
