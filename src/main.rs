use anyhow::{ensure, format_err, Context, Result};
use chrono::NaiveTime;
use discord::model::Event;
use discord::Discord;
use log::{error, info, LevelFilter};
use std::path::PathBuf;
use std::thread;
use structopt::StructOpt;

use v4l::buffer::Type;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::Device;
use v4l::FourCC;

use image::RgbImage;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

mod printer;
mod time_range;
use printer::{PrintHandler, PrinterMsg};
use time_range::TimeRange;
mod twitter_login;

#[derive(Debug, StructOpt)]
#[structopt(name = "Printer bot 2", about = "A bot for receipt printers")]
struct Opt {
    /// Disable the camera
    #[structopt(long)]
    disable_camera: bool,

    /// Disable the printer
    #[structopt(long)]
    disable_printer: bool,

    /// Logging path
    #[structopt(long, default_value = "print_bot.log")]
    log_path: PathBuf,

    /// Discord bot token
    #[structopt(long)]
    discord_token: Option<String>,

    /// Twitter API key
    #[structopt(long)]
    twitter_key: Option<String>,

    /// Twitter API secret key
    #[structopt(long)]
    twitter_secret: Option<String>,

    /// Begin active hours (local, 24 hour)
    #[structopt(long)]
    begin_time: Option<String>,

    /// End active hours (local, 24 hour)
    #[structopt(long)]
    end_time: Option<String>,

    /// Max printed bytes for text
    #[structopt(long)]
    max_bytes_text: Option<u32>,

    /// Max printed bytes for images
    /// WARNING: User may escape image mode and use this to write more text than max_bytes_text
    #[structopt(long)]
    max_bytes_image: Option<u32>,

    /// Max instructions
    #[structopt(long)]
    max_instructions: Option<u32>,

    /// Print a header with each message
    #[structopt(long)]
    header: bool,
}

struct CameraClient {
    pub recv: Receiver<Vec<u8>>,
    pub sender: Sender<usize>,
    pub id: usize,
}

impl CameraClient {
    pub fn capture(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.sender.send(self.id).ok()?;
        self.recv.recv_timeout(timeout).ok()
    }
}

fn camera_thread(recv: Receiver<usize>, clients: Vec<Sender<Vec<u8>>>) -> Result<()> {
    // Create a new capture device with a few extra parameters
    let dev = Device::new(0).context("Open device")?;

    // Let's say we want to explicitly request another format
    let mut fmt = dev.format().context("Read format")?;
    fmt.width = 1280;
    fmt.height = 720;
    fmt.fourcc = FourCC::new(b"MJPG");
    dev.set_format(&fmt).context("Write format")?;

    // The camera will remain in use for the duration of the program.
    let dev = Box::leak(Box::new(dev));

    // Create the stream, which will internally 'allocate' (as in map) the
    // number of requested buffers for us.
    let mut stream = Stream::with_buffers(dev, Type::VideoCapture, 4)
        .context("Failed to create buffer stream")?;

    // Prime the camera
    let steps = 5;
    for i in 1..=steps {
        info!("Priming the camera {}/{}", i, steps);
        stream.next()?;
    }

    loop {
        let client_idx = recv.recv()?;
        let (buffer, _meta) = stream.next()?;
        clients[client_idx].send(buffer.to_vec())?;
    }
}

// Settings
pub const HELP_COMMAND: &str = "!help";
pub const PRINT_COMMAND: &str = "!print";
pub const SHOW_COMMAND: &str = "!showme";
pub const LUA_COMMAND: &str = "!lua";

/// Log a result as an error
pub fn log_result(res: Result<()>) {
    if let Err(e) = res {
        error!("{:#}", e);
    }
}

/// Log a fatal error and panic
pub fn fatal_error(res: Result<()>) {
    if res.is_err() {
        log_result(res);
        panic!("Fatal error");
    }
}

fn parse_time(s: &str) -> Result<NaiveTime> {
    let mut s = s.split(':');
    match (s.next(), s.next()) {
        (Some(h), Some(m)) => Ok(NaiveTime::from_hms(h.parse()?, m.parse()?, 0)),
        (Some(_), None) => Err(format_err!("Time missing minutes")),
        (None, Some(_)) => unreachable!(),
        (None, None) => Err(format_err!("Malformed time")),
    }
}

fn lua_err(res: mlua::Error) -> anyhow::Error {
    format_err!("{}", res)
}

/// Role: Act as the communication layer between Discord, LUA, and the Printer
fn lua_thread(
    discord: Receiver<String>,
    printer: Option<Sender<PrinterMsg>>,
    max_instructions: u32,
    max_bytes_text: u32,
    max_bytes_image: u32,
) -> Result<()> {
    info!("Lua thread started");
    use mlua::StdLib;
    let lua = mlua::Lua::new_with(StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::ALL_SAFE)
        .map_err(lua_err)?;

    fn print_res(printer: &Option<Sender<PrinterMsg>>, msg: PrinterMsg) -> Result<()> {
        match printer {
            Some(p) => Ok(p.send(msg)?),
            None => Ok(match msg {
                PrinterMsg::Image(img) => {
                    let path = chrono::Local::now().format("lua-%H-%M-%S.png").to_string();
                    eprintln!("Lua image {}x{}: {}", img.width(), img.height(), &path);
                    img.save(&path)?;
                }
                PrinterMsg::Text(txt) => eprintln!("Lua text: {}", txt),
            }),
        }
    }

    loop {
        // Receive
        let msg = discord.recv()?;

        // If present, remove code block
        let msg = msg
            .trim_start()
            .trim_start_matches("```lua")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim_end();
        use mlua::Error;

        // Text printing and byte exhaustion
        let remaining_bytes = Rc::new(RefCell::new(max_bytes_text as i64));
        let lua_printer = printer.clone();
        let print = lua
            .create_function(move |_, v: String| {
                *remaining_bytes.borrow_mut() -= v.as_bytes().len() as i64;
                match *remaining_bytes.borrow() > 0 {
                    true => Ok(print_res(&lua_printer, PrinterMsg::Text(v)).unwrap()),
                    false => Err(Error::RuntimeError("Text byte limit reached".into())),
                }
            })
            .map_err(lua_err)?;
        lua.globals().set("print", print).map_err(lua_err)?;

        // Image printing and byte exhaustion
        let remaining_bytes = Rc::new(RefCell::new(max_bytes_image as i64));
        let lua_printer = printer.clone();
        let print_image = lua
            .create_function(move |_, v: Vec<bool>| {
                *remaining_bytes.borrow_mut() -= v.len() as i64;
                match *remaining_bytes.borrow() > 0 {
                    true => {
                        let image = lua_image_to_rbgimage(v)
                            .map_err(|e| Error::RuntimeError(e.to_string()))?;
                        print_res(&lua_printer, PrinterMsg::Image(image))
                            .map_err(|e| Error::RuntimeError(e.to_string()))
                    }
                    false => Err(Error::RuntimeError("Image byte limit reached".into())),
                }
            })
            .map_err(lua_err)?;
        lua.globals().set("image", print_image).map_err(lua_err)?;

        // Instruction exhaustion
        lua.set_hook(
            mlua::HookTriggers {
                every_nth_instruction: Some(max_instructions),
                ..Default::default()
            },
            move |_, _| {
                Err(mlua::Error::RuntimeError(
                    "Instruction limit reached".into(),
                ))
            },
        )
        .map_err(lua_err)?;

        // Execute
        match lua.load(&msg).eval::<mlua::MultiValue>() {
            Err(mlua::Error::CallbackError { cause, .. }) => {
                if let mlua::Error::RuntimeError(v) = cause.as_ref() {
                    print_res(&printer, PrinterMsg::Text(format!("{}", v)))?;
                } else {
                    print_res(
                        &printer,
                        PrinterMsg::Text(format!("Callback error: {}", cause)),
                    )?;
                }
            }
            Err(e) => print_res(&printer, PrinterMsg::Text(format!("Error: {}", e)))?,
            Ok(v) => v
                .iter()
                .map(|v| print_res(&printer, PrinterMsg::Text(value_to_string(v))))
                .collect::<Result<Vec<()>, _>>()
                .map(|_| ())?,
        }

        // Remove limit
        lua.remove_hook();
    }
}

fn lua_image_to_rbgimage(image: Vec<bool>) -> Result<RgbImage> {
    ensure!(
        image.len() as u32 % printer::PRINTER_DOTS_PER_LINE == 0,
        "Err: Img width != 384"
    );
    let mut rgb = Vec::with_capacity(image.len() * 3);

    for &px in &image {
        let px = if px { 0x00 } else { 0xFF };
        rgb.extend(&[px; 3]);
    }

    RgbImage::from_raw(
        printer::PRINTER_DOTS_PER_LINE,
        image.len() as u32 / printer::PRINTER_DOTS_PER_LINE,
        rgb,
    )
    .context("Failed to create rgb image")
}

use mlua::Value;
fn value_to_string(value: &Value) -> String {
    match value {
        Value::Nil => "nil".into(),
        Value::Boolean(b) => {
            if *b {
                "true".into()
            } else {
                "false".into()
            }
        }
        Value::Integer(i) => format!("{}", i),
        Value::Number(n) => format!("{}", n),
        Value::String(s) => format!("\"{}\"", s.to_str().unwrap_or("")),
        other => format!("{:?}", other),
    }
}

/// Discord interaction
fn discord_thread(
    token: &str,
    time_range: Option<TimeRange>,
    lua_tx: Sender<String>,
    printer: Option<Sender<PrinterMsg>>,
    camera: Option<CameraClient>,
    header: bool,
) -> Result<()> {
    // Set up printer concurrently with logging into Discord
    let mut print_handler = printer.map(|tx| PrintHandler::new(tx)).transpose()?;

    // Log in to Discord using a bot token from the environment
    info!("Logging into discord");
    let discord = Discord::from_bot_token(token).context("login failed")?;

    // Establish and use a websocket connection
    let (mut connection, _) = discord.connect().context("connect failed")?;

    info!("Discord ready.");
    loop {
        match connection.recv_event() {
            Ok(Event::MessageCreate(message)) => {
                // No bots!
                if message.author.bot {
                    continue;
                }

                // Parse command from message
                let cmd = match message.content.split_whitespace().next() {
                    Some(cmd) => cmd,
                    None => continue,
                };

                // Run command
                match cmd {
                    PRINT_COMMAND => {
                        // TODO: This should be calculated for the PRINTER and not for Discord!
                        if let Some(time_range) = time_range {
                            let (time, in_range) = time_range.check_local();
                            if !in_range {
                                let msg = sorry_asleep(time_range, time);
                                discord.send_message(message.channel_id, &msg, "", false)?;
                                continue;
                            }
                        }

                        info!(
                            "{}#{} began a print job.",
                            message.author.name, message.author.discriminator
                        );

                        if let Some(handler) = &mut print_handler {
                            log_result(handler.handle_discord(message, header));
                        } else {
                            discord.send_message(message.channel_id, SORRY_PRINTER, "", false)?;
                        }
                    }
                    LUA_COMMAND => {
                        if let Some(time_range) = time_range {
                            let (time, in_range) = time_range.check_local();
                            if !in_range {
                                let msg = sorry_asleep(time_range, time);
                                discord.send_message(message.channel_id, &msg, "", false)?;
                                continue;
                            }
                        }

                        lua_tx.send(message.content.trim_start_matches(LUA_COMMAND).to_string())?
                    }
                    HELP_COMMAND => {
                        discord.send_message(message.channel_id, HELP_TEXT, "", false)?;
                    }
                    SHOW_COMMAND => match camera
                        .as_ref()
                        .and_then(|c| c.capture(Duration::from_secs(2)))
                    {
                        Some(buf) => {
                            info!(
                                "{}#{} took a picture.",
                                message.author.name, message.author.discriminator
                            );
                            discord
                                .send_file(
                                    message.channel_id,
                                    "",
                                    std::io::Cursor::new(buf),
                                    "image.jpg",
                                )
                                .context("Failed to send image file!")?;
                        }
                        None => {
                            discord.send_message(message.channel_id, SORRY_CAMERA, "", false)?;
                        }
                    },
                    _ => (),
                }
            }
            Ok(_) => {}
            Err(discord::Error::Closed(code, body)) => {
                break Err(format_err!(
                    "Gateway closed on us with code {:?}: {}",
                    code,
                    body
                ));
            }
            Err(err) => error!("Receive error: {:?}", err),
        }
    }
}

fn twitter_thread(
    printer: Option<Sender<PrinterMsg>>,
    key: String,
    secret_key: String,
    camera: Option<CameraClient>,
) {
    tokio::runtime::Builder::new()
        //.threaded_scheduler()
        .basic_scheduler()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            log_result(
                twitter_thread_internal(printer, key.clone(), secret_key.clone(), camera)
                    .await
                    .context("Twitter failed"),
            )
        });
}

async fn twitter_thread_internal(
    printer: Option<Sender<PrinterMsg>>,
    key: String,
    secret_key: String,
    camera: Option<CameraClient>,
) -> Result<()> {
    info!("Twitter is logging in...");

    use tokio::stream::StreamExt;
    let con_token = egg_mode::KeyPair::new(key, secret_key);
    let (config, token) = twitter_login::login(con_token, "login.txt")
        .await
        .context("Log in")?;

    info!("Twitter logged in as {}", config.screen_name);

    use egg_mode::stream::{filter, StreamMessage};
    let mut stream = filter()
        //.follow(&[config.user_id])
        .track(&[format!("@{}", config.screen_name)])
        .start(&token);

    while let Some(res) = stream.next().await {
        let msg = res.context("Receive message")?;
        if let (StreamMessage::Tweet(t), Some(printer)) = (msg, &printer) {
            // Ignore yourself...
            if t.id == config.user_id {
                continue;
            }

            // Get username
            let user_name = match &t.user {
                Some(u) => &u.screen_name,
                None => continue,
            };

            info!("Handling Tweet from {}", user_name);

            // Send the tweet
            let tweet_text = t
                .text
                .trim_start_matches("@")
                .trim_start_matches(&config.screen_name);
            let text = format!("{}: {}\n\n", user_name, tweet_text);
            printer
                .send(PrinterMsg::Text(text))
                .context("Send to printer")?;

            // Wait for printer to print
            tokio::time::delay_for(Duration::from_secs(1)).await;

            // Take a picture and reply with it
            if let Some(pic) = camera
                .as_ref()
                .and_then(|c| c.capture(Duration::from_secs(2)))
            {
                let handle = egg_mode::media::upload_media(
                    &pic,
                    &egg_mode::media::media_types::image_jpg(),
                    &token,
                )
                .await
                .context("Upload image")?;

                let mut draft = egg_mode::tweet::DraftTweet::new("Here ya go!")
                    .in_reply_to(t.id)
                    .auto_populate_reply_metadata(true);
                draft.add_media(handle.id);
                draft.send(&token).await.context("Send tweet")?;
            }
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    // Arg parsing
    let opt = Opt::from_args();
    let begin_time = opt.begin_time.as_ref().map(|s| parse_time(s)).transpose()?;
    let end_time = opt.end_time.as_ref().map(|s| parse_time(s)).transpose()?;
    let time_range = begin_time.zip(end_time).map(|(b, e)| TimeRange(b, e));

    // Set up logging
    simple_logging::log_to_file(opt.log_path, LevelFilter::Info)?;

    // Channel for Discord <-> printer thread communication
    let printer = (!opt.disable_printer).then(|| {
        let (sender, mut receiver) = mpsc::channel();
        thread::spawn(move || loop {
            crate::log_result(printer::printer_thread(&mut receiver))
        });
        sender
    });

    // Spawn Lua thread
    let (lua_tx, lua_rx) = mpsc::channel::<String>();
    let max_instructions = opt.max_instructions.unwrap_or(u32::MAX);
    let max_bytes_text = opt.max_bytes_text.unwrap_or(u32::MAX);
    let max_bytes_image = opt.max_bytes_image.unwrap_or(u32::MAX);
    let lua_printer = printer.clone();
    let lua_thread = std::thread::spawn(move || {
        lua_thread(
            lua_rx,
            lua_printer,
            max_instructions,
            max_bytes_text,
            max_bytes_image,
        )
    });

    // Spawn camera thread
    let (discord_camera, twitter_camera) = if opt.disable_camera {
        (None, None)
    } else {
        let (camera_tx, camera_rx) = mpsc::channel();
        let (discord_tx, discord_rx) = mpsc::channel();
        let (twitter_tx, twitter_rx) = mpsc::channel();
        std::thread::spawn(move || camera_thread(camera_rx, vec![discord_tx, twitter_tx]));
        let discord = CameraClient {
            recv: discord_rx,
            sender: camera_tx.clone(),
            id: 0,
        };
        let twitter = CameraClient {
            recv: twitter_rx,
            sender: camera_tx,
            id: 1,
        };
        (Some(discord), Some(twitter))
    };

    let header = opt.header;

    // Spawn Discord thread
    let discord_printer = printer.clone();
    if let Some(token) = opt.discord_token {
        std::thread::spawn(move || {
            log_result(discord_thread(
                &token,
                time_range,
                lua_tx.clone(),
                discord_printer.clone(),
                discord_camera,
                header,
            ))
        });
    }

    // Enter Twitter thread
    if let Some((key, secret_key)) = opt.twitter_key.zip(opt.twitter_secret) {
        twitter_thread(printer.clone(), key, secret_key, twitter_camera);
    }

    lua_thread.join().unwrap()?;

    Ok(())
}

const HELP_TEXT: &str = "
**Segfault's printer bot**\n
This bot uses a receipt printer to immortalize your messages on 58mm thermal paper. Printer paper is extra super cheap, but remember that whatever you do print is waste.
If this command works, the printer _should_ be running. Have fun!

__Commands__:
`!print`: Print text or an image URL following this command, or attached images.
`!help`: Print this message
`!showme`: Take a picture of the printer, and show it here.
";

const SORRY_PRINTER: &str = "Sorry, the printer has been disabled for now :(";
const SORRY_CAMERA: &str = "Sorry, the camera has been disabled for now :(";

fn sorry_asleep<T: chrono::TimeZone>(range: TimeRange, time: chrono::DateTime<T>) -> String
where
    T::Offset: std::fmt::Display,
{
    let TimeRange(begin, end) = range;
    format!("Sorry, I'm asleep and the printer makes a bunch of noise. The current bot-local time is {} and the bot is set up to become active between {} and {} (timezone: UTC{}). Please try again later!", begin, end, time.format("%H:%M"), time.format("%:z"))
}
