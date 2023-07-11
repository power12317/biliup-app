#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use anyhow::Context;
use biliup::client::StatelessClient;
use biliup::uploader::credential::{login_by_cookies, Credential as BiliCredential};
use biliup::uploader::bilibili::{Studio, Video};
use biliup::uploader::{line, Account, Config, User, VideoFile};
use futures::future::abortable;
use std::borrow::Cow;
use std::path::PathBuf;
use std::str::FromStr;

use app::error;
use app::error::Result;
use app::{config_file, config_path, cookie_file, encode_hex, Credential};
use futures::StreamExt;
use tauri::async_runtime;
use tauri::Window;
use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{filter::LevelFilter, prelude::*, Layer, Registry};

#[tauri::command]
fn login(username: &str, password: &str, remember_me: bool) -> Result<String> {
    async_runtime::block_on(app::login_by_password(username, password))?;
    if remember_me {
        match load() {
            Ok(mut config) => {
                if let Some(ref mut user) = config.user {
                    user.account.username = username.into();
                    user.account.password = password.into();
                }
                save(config)?;
                // let file = std::fs::File::create(config_file()?).with_context(|| "open config.yaml")?;
                // serde_yaml::to_writer(file, &config).with_context(|| "write config.yaml")?
            }
            Err(_) => {
                // let file = std::fs::File::create("config.yaml").with_context(|| "create config.yaml")?;
                save(Config {
                    user: Some(User {
                        account: Account {
                            username: username.into(),
                            password: password.into(),
                        },
                    }),
                    line: None,
                    limit: 3,
                    streamers: Default::default(),
                })?;
            }
        }
    }
    Ok("登录成功".into())
}

#[tauri::command]
fn logout(credential: tauri::State<'_, Credential>) {
    credential.clear()
}

#[tauri::command]
async fn login_by_cookie(credential: tauri::State<'_, Credential>) -> Result<String> {
    credential.get_credential().await?;
    Ok("登录成功".into())
}

#[tauri::command]
async fn login_by_sms(code: u32, res: serde_json::Value) -> Result<String> {
    let info = BiliCredential::new().login_by_sms(code, res).await?;
    let file = std::fs::File::create(cookie_file()?)?;
    serde_json::to_writer_pretty(&file, &info)?;
    println!("短信登录成功，数据保存在{:?}", file);
    Ok("登录成功".into())
}

#[tauri::command]
async fn send_sms(country_code: u32, phone: u64) -> Result<serde_json::Value> {
    let ret = BiliCredential::new().send_sms(phone, country_code).await?;
    Ok(ret)
}

#[tauri::command]
async fn login_by_qrcode(res: serde_json::Value) -> Result<String> {
    let info = BiliCredential::new().login_by_qrcode(res).await?;
    let file = std::fs::File::create(cookie_file()?)?;
    serde_json::to_writer_pretty(&file, &info)?;
    println!("链接登录成功，数据保存在{:?}", file);
    Ok("登录成功".into())
}

#[tauri::command]
async fn get_qrcode() -> Result<serde_json::Value> {
    let mut qrcode = Bili::new().get_qrcode().await?;
    let response = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(
            qrcode
                .get("data")
                .and_then(|d| d.get("url"))
                .and_then(|u| u.as_str())
                .ok_or_else(|| qrcode.to_string())?,
        )
        .send()
        .await?;
    match response.headers().get("location") {
        None => Err(response.text().await?.into()),
        Some(location) => {
            let url = location
                .to_str()
                .unwrap()
                .replace("/confirm", "/common/confirm");
            qrcode["data"]["url"] = url.into();
            Ok(qrcode)
        }
    }
}

#[tauri::command]
async fn upload(
    mut video: Video,
    window: Window,
    credential: tauri::State<'_, Credential>,
) -> Result<Video> {
    let bili = &*credential.get_credential().await?;

    let config = load()?;
    let probe = if let Some(line) = config.line {
        match line.as_str() {
            "kodo" => line::kodo(),
            "bda2" => line::bda2(),
            "ws" => line::ws(),
            "qn" => line::qn(),
            "qnhk" => line::qnhk(),
            "bldsa" => line::bldsa(),
            "cos" => line::cos(),
            "cos-internal" => line::cos_internal(),
            _ => unreachable!(),
        }
    } else {
        line::Probe::probe(&bili.client).await?
    };
    let limit = config.limit;

    let filename = video.filename;
    let filepath = PathBuf::from(&filename);
    let video_file = VideoFile::new(&filepath)?;
    let total_size = video_file.total_size;
    let parcel = probe.pre_upload(bili, video_file).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    // let (tx, mut rx) = mpsc::channel(1);
    let mut uploaded = 0;
    let f_video = parcel.upload(StatelessClient::default(), limit, |vs| {
        vs.map(|chunk| {
            let chunk = chunk?;
            let len = chunk.len();
            uploaded += len;
            tx.send(uploaded).unwrap();
            let progressbar = app::Progressbar::new(chunk, tx2.clone());
            Ok((progressbar, len))
        })
    });
    let (a_video, abort_handle) = abortable(f_video);
    let _id = window.once(
        encode_hex(filename.encode_utf16().collect::<Vec<u16>>().as_slice()),
        move |event| {
            abort_handle.abort();
            println!("got window event-name with payload {:?}", event.payload());
            // is_remove.store(false, Ordering::Relaxed);
        },
    );
    let f2 = filename.clone();
    let w2 = window.clone();
    //fixme
    //使用progressbar返回上传进度遇到错误触发重传时，会重新往channel2中写入信息，导致进度条超过100%，故使用channel1来传输进度。
    //而channel1每10MB左右才会传输一次进度，故保留channel1的信息来计算速度。
    tokio::spawn(async move {
        while let Some(uploaded) = rx.recv().await {
            window
                .emit("progress", (&filename, uploaded, total_size))
                .unwrap();
        }
    });
    tokio::spawn(async move {
        while let Some(len) = rx2.recv().await {
            w2.emit("speed", (&f2, len, total_size)).unwrap();
        }
    });
    video = a_video.await??;
    println!("上传成功");
    Ok(video)
}

#[tauri::command]
async fn submit(
    studio: Studio,
    credential: tauri::State<'_, Credential>,
) -> Result<serde_json::Value> {
    let login_info = &*credential.get_credential().await?;
    let ret = login_info.submit(&studio).await?;
    Ok(ret.data.unwrap())
}

#[tauri::command]
async fn archive_pre(credential: tauri::State<'_, Credential>) -> Result<serde_json::Value> {
    let login_info = &*credential.get_credential().await?;
    Ok(login_info.archive_pre().await?)
}

#[tauri::command]
async fn get_myinfo(_credential: tauri::State<'_, Credential>,
                   file_name: String,) -> Result<serde_json::Value> {
    // let (_, client) = &*credential.get_credential().await?;
    let path_buf = config_path()?.join(&file_name);
    let login_info =login_by_cookies(path_buf).await?;
    Ok(login_info
        .client
        .get("https://api.bilibili.com/x/space/myinfo")
        .send()
        .await?
        .json()
        .await?)
}

#[tauri::command]
fn load_account() -> Result<User> {
    load()?
        .user
        .ok_or_else(|| error::Error::Err("账号信息不存在".into()))
}

#[tauri::command]
fn save(config: Config) -> Result<Config> {
    let file = std::fs::File::create(config_file()?)?;
    serde_yaml::to_writer(file, &config)?;
    Ok(config)
}

#[tauri::command]
fn load() -> Result<Config> {
    let file = std::fs::File::open(config_file()?).with_context(|| "biliup/config.yaml")?;
    let config: Config = serde_yaml::from_reader(file)?;
    Ok(config)
}

#[tauri::command]
async fn cover_up(
    input: Cow<'_, [u8]>,
    credential: tauri::State<'_, Credential>,
) -> Result<String> {
    let bili = &*credential.get_credential().await?;
    let url = bili.cover_up(&input).await?;
    Ok(url)
}

#[tauri::command]
fn is_vid(input: &str) -> bool {
    biliup::uploader::bilibili::Vid::from_str(input).is_ok()
}

#[tauri::command]
async fn show_video(input: &str, credential: tauri::State<'_, Credential>) -> Result<Studio> {
    let login_info = &*credential.get_credential().await?;
    let data = login_info
        .video_data(&biliup::uploader::bilibili::Vid::from_str(input)?)
        .await?;
    let mut data = dbg!(data);
    let mut studio: Studio = serde_json::from_value(data["archive"].take())?;
    studio.videos = data["videos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| Video {
            desc: v["desc"].as_str().unwrap().to_string(),
            filename: v["filename"].as_str().unwrap().to_string(),
            title: v["title"].as_str().map(|t| t.to_string()),
        })
        .collect();
    Ok(studio)
}

#[tauri::command]
async fn edit_video(
    studio: Studio,
    credential: tauri::State<'_, Credential>,
) -> Result<serde_json::Value> {
    let ret = credential.get_credential().await?.edit(&studio).await?;
    Ok(ret)
}

#[tauri::command]
fn log(level: &str, msg: &str) -> Result<()> {
    info!(level = level, msg);
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .setup(|_app| {
            let stdout_log = tracing_subscriber::fmt::layer()
                .pretty()
                .with_filter(LevelFilter::INFO);
            let file_appender = tracing_appender::rolling::never(config_path()?, "biliup.log");
            // let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_appender)
                .with_filter(LevelFilter::INFO);
            Registry::default().with(stdout_log).with(file_layer).init();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            login,
            login_by_cookie,
            load_account,
            upload,
            submit,
            archive_pre,
            load,
            save,
            login_by_sms,
            send_sms,
            login_by_qrcode,
            get_qrcode,
            get_myinfo,
            cover_up,
            is_vid,
            show_video,
            edit_video,
            log,
            logout
        ])
        .manage(Credential::default())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
