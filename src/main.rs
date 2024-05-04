/* ---------------------------------------------------------------------------
** This software is in the public domain, furnished "as is", without technical
** support, and with no warranty, express or implied, as to its usefulness for
** any purpose.
**
** SPDX-License-Identifier: Unlicense
**
** -------------------------------------------------------------------------*/

use anyhow::{anyhow, Error};
use clap::Parser;
use futures::StreamExt;
use log::{error, info, debug};
use retina::client::{SessionGroup, SetupOptions};
use retina::codec::CodecItem;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use std::sync::Arc;
use tokio::sync::broadcast;

use actix::{Actor, AsyncContext, StreamHandler};
use actix_web::{get, web, App, HttpServer, HttpRequest, HttpResponse};
use actix_files::Files;
use actix_web_actors::ws;
use serde_json::json;



#[derive(Parser)]
pub struct Opts {
    /// `rtsp://` URL to connect to.
    #[clap(long)]
    url: url::Url,
}

pub async fn run(opts: Opts, tx: broadcast::Sender<Vec<u8>>) -> Result<(), Error> {
    let session_group = Arc::new(SessionGroup::default());
    let r = run_inner(opts, session_group.clone(), tx).await;
    if let Err(e) = session_group.await_teardown().await {
        error!("TEARDOWN failed: {}", e);
    }
    r
}

async fn run_inner(opts: Opts, session_group: Arc<SessionGroup>, tx: broadcast::Sender<Vec<u8>>) -> Result<(), Error> {
    let stop = tokio::signal::ctrl_c();

    let mut session = retina::client::Session::describe(
        opts.url,
        retina::client::SessionOptions::default()
            .session_group(session_group),
    )
    .await?;
    info!("{:?}", session.streams());

    let video_stream = session
        .streams()
        .iter()
        .position(|s| {
            matches!(
                s.parameters(),
                Some(retina::codec::ParametersRef::Video(..))
            )
        })
        .ok_or_else(|| anyhow!("couldn't find video stream"))?;

    session
        .setup(video_stream, SetupOptions::default())
        .await?;

    let mut videosession = session
        .play(retina::client::PlayOptions::default())
        .await?
        .demuxed()?;

    tokio::pin!(stop);
    loop {
        tokio::select! {
            item = videosession.next() => {
                match item.ok_or_else(|| anyhow!("EOF"))?? {
                    CodecItem::VideoFrame(m) => {
                        debug!(
                            "{}: size:{}\n",
                            m.timestamp().timestamp(),
                            m.data().len(),
                        );
                        if let Err(e) = tx.send(m.data().to_vec()) {
                            error!("Error broadcasting message: {}", e);
                        }                        
                    },
                    _ => continue,
                };
            },
            _ = &mut stop => {
                break;
            },
        }
    }
    Ok(())
}

struct MyWs {
    rx: broadcast::Receiver<Vec<u8>>,
}

impl MyWs {
    fn new(rx: broadcast::Receiver<Vec<u8>>) -> Self {
        Self { rx }
    }
}

impl Clone for MyWs {
    fn clone(&self) -> Self {
        Self {
            rx: self.rx.resubscribe(),
        }
    }
}

impl Actor for MyWs {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        info!("Websocket connected");

        let rx = self.rx.resubscribe();
        let stream = tokio_stream::wrappers::BroadcastStream::<Vec<u8>>::new(rx);
        ctx.add_stream(stream);
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        info!("Websocket disconnected");
    }    
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for MyWs {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            _ => (),
        }
    }
}

impl StreamHandler<Result<Vec<u8>, BroadcastStreamRecvError>> for MyWs {
    fn handle(&mut self, msg: Result<Vec<u8>, BroadcastStreamRecvError>, ctx: &mut Self::Context) {
        match msg {
            Ok(msg) => ctx.binary(msg),
            _ => (),
        }
    }
}

async fn ws_index(req: HttpRequest, stream: web::Payload, data: web::Data<MyWs>) -> Result<HttpResponse, actix_web::Error> {
    let rx = data.get_ref().rx.resubscribe();
    let resp = ws::start(MyWs::new(rx), &req, stream);
    resp
}

#[tokio::main]
async fn main() {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    // Create a broadcast channel to send video frames to the WebSocket server
    let (tx, rx) = broadcast::channel::<Vec<u8>>(100);
    let app_state = MyWs::new(rx);

    // Start the Actix web server
    info!("start actix web server");
    tokio::spawn(async {
        HttpServer::new( move || {
            App::new().app_data(web::Data::new(app_state.clone()))
                .route("/ws", web::get().to(ws_index))
                .service(version)
                .service(streams)
                .service(web::redirect("/", "/index.html"))
                .service(Files::new("/", "./www").show_files_listing())
        })
        .bind(("0.0.0.0", 8080)).unwrap()
        .run()
        .await
        .unwrap();
    });

    // Start the RTSP client
    info!("start rtsp client");
    if let Err(e) = {
        let opts = Opts::parse();
        run(opts, tx).await
    } {
        error!("Fatal: {}", itertools::join(e.chain(), "\ncaused by: "));
        std::process::exit(1);
    }


    info!("Done");
}

#[get("/api/streams")]
async fn streams() -> HttpResponse {
    let data = json!({
        "/ws": "stream1",
    });

    HttpResponse::Ok().json(data)
}

#[get("/api/version")]
async fn version() -> HttpResponse {
    let data = json!("version");

    HttpResponse::Ok().json(data)
}
