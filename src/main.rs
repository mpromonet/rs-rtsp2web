/* ---------------------------------------------------------------------------
** This software is in the public domain, furnished "as is", without technical
** support, and with no warranty, express or implied, as to its usefulness for
** any purpose.
**
** SPDX-License-Identifier: Unlicense
**
** -------------------------------------------------------------------------*/

use anyhow::{anyhow, Error};
use actix_files::Files;
use actix_web::{get, web, App, HttpServer, HttpResponse};
use clap::Parser;
use futures::StreamExt;
use log::{error, info, debug};
use retina::client::{SessionGroup, SetupOptions};
use retina::codec::{CodecItem, VideoFrame};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::broadcast;

mod wsservice;

#[derive(Parser)]
pub struct Opts {
    /// `rtsp://` URL to connect to.
    #[clap(long)]
    url: url::Url,
}

pub async fn run(opts: Opts, tx: broadcast::Sender<wsservice::Frame>) -> Result<(), Error> {
    let session_group = Arc::new(SessionGroup::default());
    let r = run_inner(opts, session_group.clone(), tx).await;
    if let Err(e) = session_group.await_teardown().await {
        error!("TEARDOWN failed: {}", e);
    }
    r
}

fn process_video_frame(m: VideoFrame, codec: &str, cfg: &[u8], tx: broadcast::Sender<wsservice::Frame>) {
    debug!(
        "{}: size:{} is_random_access_point:{} has_new_parameters:{}",
        m.timestamp().timestamp(),
        m.data().len(),
        m.is_random_access_point(),
        m.has_new_parameters(),
    );

    let mut metadata = json!({
        "ts": m.timestamp().timestamp(),
        "media": "video",
        "codec": codec,
    });
    let mut data: Vec<u8> = vec![];
    if m.is_random_access_point() {
        metadata["type"] = "keyframe".into();
        data.extend_from_slice(&cfg);
    }
    let mut framedata = m.data().to_vec();
    if framedata.len() > 3 {
        framedata[0] = 0;
        framedata[1] = 0;
        framedata[2] = 0;
        framedata[3] = 1;
    }
    data.extend_from_slice(framedata.as_slice());

    let frame = wsservice::Frame {
        metadata,
        data,
    };

    if let Err(e) = tx.send(frame) {
        error!("Error broadcasting message: {}", e);
    }                        
}

async fn run_inner(opts: Opts, session_group: Arc<SessionGroup>, tx: broadcast::Sender<wsservice::Frame>) -> Result<(), Error> {
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

    let video_params = match session.streams()[video_stream].parameters() {
        Some(retina::codec::ParametersRef::Video(v)) => v.clone(),
        Some(_) => unreachable!(),
        None => unreachable!(),
    };
    info!("video_params:{:?}", video_params);
    let extra_data = video_params.extra_data();
    info!("extra_data:{:?}", extra_data);

    let sps_position = extra_data.iter().position(|&nal| nal & 0x1F == 7);
    let pps_position = extra_data.iter().position(|&nal| nal & 0x1F == 8);

    let mut cfg: Vec<u8> = vec![];
    if let (Some(sps), Some(pps)) = (sps_position, pps_position) {
        if sps < pps {
            cfg = vec![0x00, 0x00, 0x00, 0x01];
            cfg.extend_from_slice(&extra_data[sps..pps]);
            cfg.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            cfg.extend_from_slice(&extra_data[pps..]);
            println!("CFG: {:?}", cfg);
        }
    }

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
                    CodecItem::VideoFrame(m) => process_video_frame(m, video_params.rfc6381_codec(), cfg.as_slice(), tx.clone()),
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


#[tokio::main]
async fn main() {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    // Create a broadcast channel to send video frames to the WebSocket server
    let (tx, rx) = broadcast::channel::<wsservice::Frame>(100);
    let myws = wsservice::MyWs::new(rx);

    let opts = Opts::parse();
    // Start the RTSP client
    info!("start rtsp client");
    tokio::spawn({
        run(opts, tx)
    });

    // Start the Actix web server
    info!("start actix web server");
    HttpServer::new( move || {
        let mut app = App::new().app_data(web::Data::new(myws.clone()))
            .service(version)
            .service(streams)
            .service(web::redirect("/", "/index.html"))
            .service(Files::new("/", "./www").show_files_listing());

        app = app.route("/ws", web::get().to(wsservice::ws_index));
        app
    })
    .bind(("0.0.0.0", 8080)).unwrap()
    .run()
    .await
    .unwrap();


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
